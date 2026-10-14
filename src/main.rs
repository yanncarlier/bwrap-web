//! bwrap-web — a minimal local web UI for configuring and launching a
//! bubblewrap sandbox around the OpenCode coding agent.
//!
//! Security model (this process can launch programs, so treat it like a shell):
//!   * binds to 127.0.0.1 only
//!   * every request needs a random per-run token
//!   * the Host header must be 127.0.0.1:<port> or localhost:<port> (DNS rebinding)
//!   * the Origin header, when present, must be the same origin (cross-site websockets)
//!   * bwrap is started with an explicit argument vector, never through a shell
//!   * mounts of "/" or of your home directory (or a parent of it) are refused

use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, Request, State,
    },
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};

const INDEX_HTML: &str = include_str!("../static/index.html");
const DEFAULT_PORT: u16 = 7878;

/// System paths that are always mounted read-only and must never be writable.
const SYSTEM_RO: [&str; 5] = ["/usr", "/bin", "/sbin", "/lib", "/lib64"];
const PROTECTED_RW: [&str; 10] = [
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/boot", "/proc", "/sys", "/dev",
];

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    /// Directory the agent may read and write (the "project"). Relative paths are
    /// resolved against the directory bwrap-web was started from.
    project_dir: String,
    /// Program to run inside the sandbox: bare name (looked up on the host) or absolute path.
    command: String,
    /// Arguments passed to the program, one per entry.
    args: Vec<String>,
    /// Keep the host network (needed for LLM APIs). Off = loopback only.
    share_net: bool,
    /// Extra read-only mounts (`~/` is expanded). Missing paths are skipped.
    ro_mounts: Vec<String>,
    /// Extra read-write mounts. Missing paths under $HOME are created.
    rw_mounts: Vec<String>,
    /// Host environment variables copied into the sandbox if they are set.
    env_passthrough: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Config {
            project_dir: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            command: "opencode".into(),
            args: vec![],
            share_net: true,
            ro_mounts: strs(&[
                "~/.opencode",
                "~/.gitconfig",
                "~/.config/gh",
                "~/.npmrc",
                "~/.local/bin",
                "~/.nvm",
                "~/.local/share/fnm",
                "~/.npm-global",
                "~/.bun",
                "~/.cargo",
                "~/.rustup",
            ]),
            rw_mounts: strs(&[
                "~/.config/opencode",
                "~/.local/share/opencode",
                "~/.local/state/opencode",
                "~/.cache/opencode",
            ]),
            env_passthrough: strs(&[
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "OPENROUTER_API_KEY",
                "XAI_API_KEY",
                "GOOGLE_API_KEY",
                "GEMINI_API_KEY",
                "GOOGLE_GENERATIVE_AI_API_KEY",
                "GROQ_API_KEY",
                "DEEPSEEK_API_KEY",
                "MISTRAL_API_KEY",
                "ZAI_API_KEY",
                "OPENCODE_CONFIG",
            ]),
        }
    }
}

fn clean_list(v: &[String]) -> Vec<String> {
    v.iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

impl Config {
    fn normalized(mut self) -> Config {
        self.project_dir = self.project_dir.trim().to_string();
        self.command = self.command.trim().to_string();
        self.args = clean_list(&self.args);
        self.ro_mounts = clean_list(&self.ro_mounts);
        self.rw_mounts = clean_list(&self.rw_mounts);
        self.env_passthrough = clean_list(&self.env_passthrough);
        self
    }
}

fn home_dir() -> Result<PathBuf> {
    let h = std::env::var_os("HOME").ok_or_else(|| anyhow!("$HOME is not set"))?;
    let h = PathBuf::from(h);
    if !h.is_absolute() {
        bail!("$HOME is not an absolute path");
    }
    Ok(h)
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg.normalized())
}

fn save_config(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Path handling and validation
// ---------------------------------------------------------------------------

/// Expand `~`, require an absolute path, and reject `..` components.
fn expand(p: &str, home: &Path) -> Result<PathBuf> {
    let p = p.trim();
    if p.is_empty() || p.contains('\0') {
        bail!("empty or invalid path");
    }
    let path = if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    };
    if !path.is_absolute() {
        bail!("path must be absolute or start with ~/ : {p}");
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => bail!("'..' is not allowed in paths: {p}"),
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

fn expand_all(list: &[String], home: &Path) -> Result<Vec<PathBuf>> {
    list.iter().map(|m| expand(m, home)).collect()
}

/// Resolve the project directory. Relative paths (`./test`, `../x`, `test`) are resolved
/// against the directory bwrap-web was started from. The result is canonical, with
/// symlinks and `..` resolved, so the safety checks below see the real location.
fn resolve_project(p: &str, home: &Path) -> Result<PathBuf> {
    let p = p.trim();
    if p.is_empty() || p.contains('\0') {
        bail!("project directory is empty or invalid");
    }
    let path = if p == "~" || p.starts_with("~/") {
        expand(p, home)?
    } else {
        PathBuf::from(p)
    };
    let canonical = std::fs::canonicalize(&path).with_context(|| {
        let shown = std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or_else(|_| path.clone());
        format!("project directory {} does not exist", shown.display())
    })?;
    if !canonical.is_dir() {
        bail!("project directory {} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", p.display()))
}

fn check_mount(path: &Path, home: &Path, rw: bool, what: &str) -> Result<()> {
    let home_canonical = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if home.starts_with(path) || home_canonical.starts_with(path) {
        bail!(
            "{what}: {} is your home directory or one of its parents, which would expose everything",
            path.display()
        );
    }
    if rw {
        for p in PROTECTED_RW {
            if path.starts_with(p) {
                bail!("{what}: {} must not be writable", path.display());
            }
        }
    }
    Ok(())
}

fn valid_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate(cfg: &Config, home: &Path) -> Result<()> {
    let proj = resolve_project(&cfg.project_dir, home)?;
    check_mount(&proj, home, true, "project directory")?;
    if cfg.command.is_empty() {
        bail!("command must not be empty");
    }
    for m in &cfg.ro_mounts {
        let p = expand(m, home).with_context(|| format!("read-only mount '{m}'"))?;
        check_mount(&p, home, false, "read-only mount")?;
    }
    for m in &cfg.rw_mounts {
        let p = expand(m, home).with_context(|| format!("read-write mount '{m}'"))?;
        check_mount(&p, home, true, "read-write mount")?;
    }
    for v in &cfg.env_passthrough {
        if !valid_env_name(v) {
            bail!("invalid environment variable name: {v}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Building the bwrap command line
// ---------------------------------------------------------------------------

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn resolve_program(cmd: &str, home: &Path) -> Result<PathBuf> {
    if cmd.contains('/') || cmd.starts_with('~') {
        let p = expand(cmd, home)?;
        if is_executable(&p) {
            return Ok(p);
        }
        bail!("{} is not an executable file", p.display());
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    // The official OpenCode installer puts the binary here.
    dirs.push(home.join(".opencode/bin"));
    dirs.push(home.join(".local/bin"));
    for d in dirs {
        let candidate = d.join(cmd);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("'{cmd}' was not found in PATH; set an absolute path in the command field")
}

fn add(a: &mut Vec<String>, xs: &[&str]) {
    a.extend(xs.iter().map(|s| s.to_string()));
}

/// Build the full argument list for `bwrap`. When `reveal` is false, passed-through
/// environment values are masked so the preview never shows secrets.
///
/// Returns the arguments and the index at which the sandboxed program begins.
fn build_args(cfg: &Config, home: &Path, reveal: bool) -> Result<(Vec<String>, usize)> {
    validate(cfg, home)?;
    let proj = resolve_project(&cfg.project_dir, home)?;
    let prog = resolve_program(&cfg.command, home)?;
    let prog_real = std::fs::canonicalize(&prog)?;
    let ro = expand_all(&cfg.ro_mounts, home)?;
    let rw = expand_all(&cfg.rw_mounts, home)?;

    // The program (and the target of any symlink) must be visible inside the sandbox.
    let mut visible: Vec<PathBuf> = SYSTEM_RO.iter().map(PathBuf::from).collect();
    visible.extend(ro.iter().cloned());
    visible.extend(rw.iter().cloned());
    visible.push(proj.clone());
    for p in [&prog, &prog_real] {
        if !visible.iter().any(|v| p.starts_with(v)) {
            bail!(
                "{} is not inside any mounted path; add its directory to the read-only mounts",
                p.display()
            );
        }
    }

    let mut a: Vec<String> = Vec::new();
    add(&mut a, &["--unshare-all"]);
    if cfg.share_net {
        add(&mut a, &["--share-net"]);
    }
    add(&mut a, &["--die-with-parent"]);

    // System (read-only)
    add(&mut a, &["--ro-bind", "/usr", "/usr"]);
    for p in [
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc/resolv.conf",
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/hosts",
        "/etc/passwd",
        "/etc/group",
        "/etc/nsswitch.conf",
        "/etc/ld.so.cache",
        "/etc/alternatives",
    ] {
        add(&mut a, &["--ro-bind-try", p, p]);
    }

    // Runtime
    add(&mut a, &["--dev", "/dev", "--proc", "/proc"]);
    add(&mut a, &["--tmpfs", "/tmp", "--tmpfs", "/var/tmp", "--tmpfs", "/run"]);

    // Project (read-write)
    let proj_s = path_str(&proj)?;
    add(&mut a, &["--bind", &proj_s, &proj_s, "--chdir", &proj_s]);

    // User-configured mounts
    for p in &ro {
        let s = path_str(p)?;
        add(&mut a, &["--ro-bind-try", &s, &s]);
    }
    for p in &rw {
        let s = path_str(p)?;
        add(&mut a, &["--bind-try", &s, &s]);
    }

    // Environment
    let home_s = path_str(home)?;
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let mut path_dirs: Vec<String> = vec![
        "/usr/local/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
        format!("{home_s}/.local/bin"),
    ];
    for p in [&prog, &prog_real] {
        if let Some(d) = p.parent().and_then(|d| d.to_str()) {
            if !path_dirs.iter().any(|x| x == d) {
                path_dirs.push(d.to_string());
            }
        }
    }
    let sandbox_path = path_dirs.join(":");
    let lang = std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into());

    add(&mut a, &["--clearenv"]);
    add(&mut a, &["--setenv", "HOME", &home_s]);
    add(&mut a, &["--setenv", "USER", &user]);
    add(&mut a, &["--setenv", "PATH", &sandbox_path]);
    add(&mut a, &["--setenv", "TERM", "xterm-256color"]);
    add(&mut a, &["--setenv", "COLORTERM", "truecolor"]);
    add(&mut a, &["--setenv", "LANG", &lang]);
    for name in &cfg.env_passthrough {
        if let Ok(val) = std::env::var(name) {
            let shown = if reveal { val } else { "********".to_string() };
            add(&mut a, &["--setenv", name, &shown]);
        }
    }

    // The program itself
    let program_index = a.len();
    a.push(path_str(&prog)?);
    a.extend(cfg.args.iter().cloned());
    Ok((a, program_index))
}

/// Create missing read-write mount directories that live under $HOME, so that
/// agent state persists instead of vanishing with the sandbox.
fn prepare_dirs(cfg: &Config, home: &Path) -> Result<()> {
    for p in expand_all(&cfg.rw_mounts, home)? {
        if p.starts_with(home) && !p.exists() {
            std::fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
        }
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn format_command(args: &[String], program_index: usize) -> String {
    let mut out = String::from("bwrap");
    for (i, a) in args.iter().enumerate() {
        if a.starts_with("--") || i == program_index {
            out.push_str(" \\\n  ");
        } else {
            out.push(' ');
        }
        out.push_str(&shell_quote(a));
    }
    out
}

// ---------------------------------------------------------------------------
// HTTP layer
// ---------------------------------------------------------------------------

struct AppState {
    token: String,
    port: u16,
    home: PathBuf,
    config_path: PathBuf,
    config: RwLock<Config>,
}

type Shared = Arc<AppState>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn bad_request(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Host / Origin / token checks applied to every request.
async fn guard(State(st): State<Shared>, req: Request, next: Next) -> Response {
    let hosts = [
        format!("127.0.0.1:{}", st.port),
        format!("localhost:{}", st.port),
    ];
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !hosts.iter().any(|h| h == host) {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        if !hosts.iter().any(|h| origin == format!("http://{h}")) {
            return (StatusCode::FORBIDDEN, "bad origin").into_response();
        }
    }
    let from_header = req.headers().get("x-token").and_then(|v| v.to_str().ok());
    let from_query = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")));
    let supplied = from_header.or(from_query).unwrap_or("");
    if !constant_time_eq(supplied, &st.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response();
    }
    next.run(req).await
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn get_config(State(st): State<Shared>) -> Json<Config> {
    Json(st.config.read().await.clone())
}

async fn put_config(
    State(st): State<Shared>,
    Json(cfg): Json<Config>,
) -> Result<Json<Config>, ApiError> {
    let cfg = cfg.normalized();
    validate(&cfg, &st.home).map_err(bad_request)?;
    save_config(&st.config_path, &cfg).map_err(|e| {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("saving config: {e:#}"))
    })?;
    *st.config.write().await = cfg.clone();
    Ok(Json(cfg))
}

async fn preview(State(st): State<Shared>) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = st.config.read().await.clone();
    let (args, program_index) = build_args(&cfg, &st.home, false).map_err(bad_request)?;
    Ok(Json(
        serde_json::json!({ "command": format_command(&args, program_index) }),
    ))
}

// ---------------------------------------------------------------------------
// Terminal websocket
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct WsQuery {
    cols: Option<u16>,
    rows: Option<u16>,
}

#[derive(Deserialize)]
struct ControlMsg {
    cols: u16,
    rows: u16,
}

struct Pty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: mpsc::Receiver<Vec<u8>>,
}

fn start_pty(cfg: &Config, home: &Path, cols: u16, rows: u16) -> Result<Pty> {
    prepare_dirs(cfg, home)?;
    let (args, _) = build_args(cfg, home, true)?;
    let proj = resolve_project(&cfg.project_dir, home)?;

    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("bwrap");
    cmd.args(&args);
    cmd.cwd(&proj);
    let child = pair
        .slave
        .spawn_command(cmd)
        .context("starting bwrap (is bubblewrap installed?)")?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    Ok(Pty {
        master: pair.master,
        child,
        writer,
        output: rx,
    })
}

async fn ws_handler(
    State(st): State<Shared>,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| run_session(socket, st, q))
}

async fn run_session(mut socket: WebSocket, st: Shared, q: WsQuery) {
    let cfg = st.config.read().await.clone();
    let cols = q.cols.unwrap_or(80).clamp(10, 500);
    let rows = q.rows.unwrap_or(24).clamp(2, 200);

    let mut pty = match start_pty(&cfg, &st.home, cols, rows) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("\r\n\x1b[31m[bwrap-web] {e:#}\x1b[0m\r\n");
            let _ = socket.send(Message::Binary(msg.into_bytes())).await;
            let _ = socket.close().await;
            return;
        }
    };

    loop {
        tokio::select! {
            out = pty.output.recv() => match out {
                Some(bytes) => {
                    if socket.send(Message::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = socket
                        .send(Message::Binary(b"\r\n[sandbox exited]\r\n".to_vec()))
                        .await;
                    break;
                }
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    if pty.writer.write_all(&data).is_err() {
                        break;
                    }
                    let _ = pty.writer.flush();
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(c) = serde_json::from_str::<ControlMsg>(&text) {
                        let _ = pty.master.resize(PtySize {
                            rows: c.rows.clamp(2, 200),
                            cols: c.cols.clamp(10, 500),
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }

    // Tear down: killing bwrap also ends the sandbox (--die-with-parent).
    let _ = pty.child.kill();
    let mut child = pty.child;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let _ = socket.close().await;
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn parse_port() -> Result<u16> {
    let mut args = std::env::args().skip(1);
    let mut port = DEFAULT_PORT;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" | "-p" => {
                let v = args.next().ok_or_else(|| anyhow!("--port needs a value"))?;
                port = v.parse().context("invalid port")?;
            }
            "--help" | "-h" => {
                println!("Usage: bwrap-web [--port N]   (default {DEFAULT_PORT})");
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(port)
}

#[tokio::main]
async fn main() -> Result<()> {
    let port = parse_port()?;
    let home = home_dir()?;
    let config_path = home.join(".config/bwrap-web/config.json");
    let config = load_config(&config_path)?;

    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();

    let state = Arc::new(AppState {
        token: token.clone(),
        port,
        home,
        config_path,
        config: RwLock::new(config),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/preview", get(preview))
        .route("/ws", get(ws_handler))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;

    if std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("warning: 'bwrap' was not found in PATH; sessions will fail until bubblewrap is installed");
    }

    println!("bwrap-web is running. Open this URL in your browser:");
    println!("  http://127.0.0.1:{port}/?token={token}");
    println!("(the token changes every time the server starts; press Ctrl-C to stop)");

    axum::serve(listener, app).await?;
    Ok(())
}