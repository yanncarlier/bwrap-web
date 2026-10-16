# AGENTS.md — bwrap-web

Guidelines for AI agents working in this repository. This is a **small,
single-crate, binary-only** Rust project. Read the security section before
touching anything that builds a command line or answers an HTTP request.

## What this is

A local web UI (127.0.0.1 only) that lets you configure and launch a
[bubblewrap](https://github.com/containers/bubblewrap) sandbox around a coding
agent (OpenCode by default), with a browser terminal over a PTY.

This process can launch programs with the user's privileges. **Treat it like a
shell, not like a web app.** The security model is documented at the top of
`src/main.rs`; keep it true.

## Layout

```
Cargo.toml          single crate, no workspace
src/main.rs         the entire server (~780 lines)
static/index.html   the entire frontend, include_str!()'d into the binary
SECURITY.md         vulnerability reporting policy
```

There is **no `src/lib.rs`** and that is intentional — this is an application,
not a library. Do not split it into `lib.rs` + `main.rs` as a drive-by
refactor; it adds churn without a benefit here.

## Build, check, test

```bash
cargo build --release                # LTO + strip; use this for the shipped binary
cargo check                          # fast feedback loop
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo test                           # see "Testing" below — currently no tests
cargo audit                          # after any dependency change
```

Notes:
- MSRV is **1.85** (see README). Ubuntu's packaged 1.75 is too old.
- The tree is currently **clippy-clean**; keep it that way.
- The tree has a few `rustfmt` deviations (long lines). Run `cargo fmt` on the
  files you touch rather than reformatting everything, to keep diffs
  reviewable.
- `cargo` is not on the default `PATH` in every shell; use
  `export PATH="$HOME/.cargo/bin:$PATH"` if needed.
- Do not enable `-W clippy::pedantic` as a gate — the existing code does not
  satisfy it and that is fine.

## Security invariants — do not break these

These are load-bearing. A change that relaxes any of them is a vulnerability,
not a refactor. If you believe one is wrong, say so explicitly rather than
quietly changing it.

1. **Bind to `127.0.0.1` only.** Never `0.0.0.0`.
2. **Every request is authenticated** by the `guard` middleware, which is
   layered over the *whole* router (including `/`). The token is 32 random
   bytes, regenerated on every process start. Accept it from the `x-token`
   header or the `?token=` query string; compare it with `constant_time_eq`.
   Do not replace that comparison with `==`.
3. **Host header must be exactly** `127.0.0.1:<port>` or `localhost:<port>`.
   This is the DNS-rebinding defence. Exact string match — do not loosen it to
   "ends with localhost", suffix matching, or allow other hosts.
4. **Origin header, when present, must be** `http://<allowed-host>`. This stops
   cross-site WebSocket connections. Do not make this check optional.
5. **bwrap is always invoked with an explicit argument vector**, never through
   `sh -c`. Never introduce a shell to build the command line.
6. **Mounts are refused if they are the home directory or a parent of it**
   (`check_mount`), and read-write mounts are refused for the `PROTECTED_RW`
   prefixes (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`, `/boot`,
   `/proc`, `/sys`, `/dev`). Both checks exist so a typo cannot expose the host.
   `expand` additionally rejects NUL bytes and `..` components and requires
   absolute paths.
7. **The sandboxed program must be visible inside the sandbox.** `build_args`
   refuses to run a program (or its symlink target) that lives outside every
   mounted path. That check is not decoration.
8. **Secrets are masked in the preview.** `build_args` takes a `reveal: bool`.
   - `reveal = false` → `/api/preview`. Passed-through env values become
     `"********"`.
   - `reveal = true` → `start_pty` only, the path that actually execs.

   Do not add a third `reveal = true` caller, and do not echo raw env values
   into any response, log, or terminal banner.
9. **`--unshare-all`, `--clearenv`, `--die-with-parent` stay.** Environment is
   passed explicitly with `--setenv`; the host environment is never inherited.
   `--die-with-parent` is what guarantees the sandbox dies when the page closes.
10. **Killing the child is what tears down the sandbox.** `run_session` calls
    `child.kill()` and reaps on a detached thread; do not leak zombies.

## Request flow

- `main` → parse port → load config → build `AppState` (token, port, home,
  config path, `RwLock<Config>`) → `Router` with the `guard` layer → bind.
- `PUT /api/config` normalizes, **validates**, then persists, then updates the
  in-memory `RwLock`.
- Config is written atomically: write `config.json.tmp`, then `rename`. Keep it
  that way — a torn write would leave the user with an unparseable config.
- `GET /api/preview` calls `build_args(.., false)` and formats it for display
  only. `shell_quote` / `format_command` are **presentation helpers**; they are
  never used to build what actually gets executed.
- `/ws` upgrades, spawns the PTY (`start_pty` → `build_args(.., true)`), and
  pumps bytes. Binary frames are terminal data; JSON text frames are
  `ControlMsg` resize messages.

## Conventions

**Imports.** Group std, external crates, local — blank line between groups.
Import explicitly, not with globs (except `use super::*;` in test modules).

**Naming.** `snake_case` functions/variables/modules, `PascalCase` types,
`SCREAMING_SNAKE_CASE` constants, single uppercase letters for generics.

**Errors.**
- Return `anyhow::Result<T>`; attach context with `.with_context(|| ...)` /
  `.ok_or_else(...)`. Use `{e:#}` when rendering to a message.
- HTTP handlers return `Result<Json<T>, ApiError>`, mapping failures with
  `bad_request`. **Do not switch to `(StatusCode, String)` tuples** — `ApiError`
  implements `IntoResponse` and returns JSON `{"error": "..."}`, which the
  frontend depends on.
- Error messages shown in the UI should name the offending value and the
  constraint ("path must be absolute or start with ~/ : {p}").
- **Never add a bare `.unwrap()`, `.expect()`, or `panic!` in `main.rs`.** The
  file currently has zero of each; keep it that way. The `unwrap_or`,
  `unwrap_or_default`, `unwrap_or_else` family *is* used — that is fine, it is
  for genuinely infallible fallbacks only.
- Do not introduce `thiserror` or a bespoke error enum. The crate is an
  application, not a library; `anyhow` throughout is the established pattern.

**Structure.** Small single-purpose functions; early returns over nesting;
pure helpers (path expansion, validation, arg building) separated from the
axum handlers and the PTY plumbing.

**Async.** One runtime: tokio (`features = ["full"]`), started once by
`#[tokio::main]`. The PTY reader is a dedicated `std::thread` doing blocking
fd reads and `blocking_send` into a **bounded** `mpsc` channel (capacity 64) —
keep it a thread. Do not move PTY reads onto the async executor or make the
channel unbounded.

**Docs.** `///` on every item, explaining *why* where it is not obvious. The
security-relevant functions (`check_mount`, `build_args`, `guard`,
`resolve_program`) must keep their doc comments: they are the only record of
why the code is shaped that way.

**Dependencies.** Current set: `anyhow`, `axum` (ws), `portable-pty`, `rand`,
`serde`, `serde_json`, `tokio`. Justify any addition, and prefer what is already
there. Two specific temptations to avoid:
- **No `clap`.** CLI parsing is hand-rolled in `parse_port` (`--port`/`-p`,
  `--help`/`-h`). Do not add a CLI framework for one optional flag.
- **No `unsafe`.** The crate root has no `unsafe` and no
  `#![forbid(unsafe_code)]`. Keep it that way; `portable-pty` uses `unsafe`
  internally, which is not our problem to propagate.

## Testing

There are currently **no tests**, and that is a known gap, not a policy.

The logic worth testing is private and lives in `src/main.rs`: `expand`,
`resolve_project`, `clean_list`, `valid_env_name`, `check_mount`, `validate`,
`shell_quote`, `format_command`, `constant_time_eq`, and `build_args`.

- Put unit tests in a `#[cfg(test)] mod tests` **inside `src/main.rs`**, using
  `use super::*;`. Do not create `tests/` for them — those functions are not
  reachable from an integration test without first introducing a `lib.rs`.
- Follow Arrange/Act/Assert. Name tests for the scenario:
  `rejects_home_directory_as_mount`, `masks_env_values_when_not_revealing`,
  `quotes_arguments_containing_spaces`.
- Always cover the **rejection** paths, not just the happy path. The rejection
  branches in `expand`, `check_mount`, and `valid_env_name` are the security
  behaviour; a test that only proves the happy path is not worth much.
- Do not write tests that need `bwrap` installed or that spawn a real sandbox.

## Frontend

`static/index.html` is a single self-contained file with vanilla JS and xterm.js.

- It is embedded with `include_str!`, so **any edit changes the binary** — you
  must rebuild for the change to take effect. There is no static file serving
  and no build step.
- xterm is loaded from jsDelivr and **pinned**: `xterm@5.3.0` and
  `xterm-addon-fit@0.8.0`. Keep the pins; don't float to `latest`.
- The page reads the token from `?token=` and sends it as `x-token` on fetches,
  and passes it in the `/ws` query string. Changing either requires matching
  changes in `guard`.
- No framework, no bundler, no npm. Adding one would be a significant,
  deliberate change — ask first.

## Before committing

1. `cargo clippy --all-targets -- -D warnings` — clean.
2. `cargo fmt` run on the files you touched.
3. `cargo build --release` succeeds.
4. `cargo test` passes (once tests exist).
5. `cargo audit` shows no new advisories.
6. **If you touched `build_args`, `check_mount`, `guard`, `expand`, or anything
   in the security list: re-read that list and confirm every invariant still
   holds, and say in your summary which ones you verified.**
7. No secrets committed. API key *names* in the default env list are fine;
   actual values never appear in the repo, in logs, or in the preview.
8. Update `README.md` if you changed CLI flags, routes, config fields, or the
   default mount/env lists.