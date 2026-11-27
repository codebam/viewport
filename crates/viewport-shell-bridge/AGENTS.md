# `crates/viewport-shell-bridge` — Out-of-Process Shell Plumbing

- **License**: MIT
- **Role**: Engine-agnostic client half of the control socket, shared by `viewport-shell-gtk`, `viewport-shell-chromium`, `viewport-shell-cef`, `viewport-shell-servo`, and `viewport-shell-servoshell`.

## Modules
- `lib.rs`:
  - `Options::parse` & `Options::parse_checked` (CLI `--url`, `--socket`, `--inspector` falling back to `VIEWPORT_SHELL_URL`, `VIEWPORT_IPC_SOCKET`, `VIEWPORT_SHELL_INSPECTOR`, `WAYLAND_DISPLAY`; enforces `viewport_ipc::js::shell_url_allowed`).
  - `connect(path, on_line) -> Result<Sender>`: spawns `ipc-write` (batches up to `MAX_BATCH = 64 KiB` per `write_all`) and `ipc-read` (`read_compositor_line` capped at `MAX_LINE = 64 MiB`, `catch_unwind` ensures `Line::Closed` is always delivered).
  - `Sender::send`: non-blocking queue capped at `MAX_QUEUED = 4 MiB` (overflow shuts down write half so compositor restarts the shell).
  - `is_reload(json)`: fast check for `{"type":"shell.reload"}` without deserializing unknown future `Event`s.
- `guard.rs`:
  - Shared CDP/DevTools security & reload state: `LoadGate` (`loaded && url_allowed`, opens *only* on `Page.loadEventFired` for an allowed URL), `FrameGuard` (tracks main frame id, main URL, default `main_context`; rejects iframe contexts), `DocumentGuard` (wires `FrameGuard` + `LoadGate` together, ignoring subframe navigations).
- `startup.rs`: `random_hex` (`/dev/urandom`), `private_dir` (`0o700` `DirBuilder::create`), `init_tracing`, `extra_args_env`.
