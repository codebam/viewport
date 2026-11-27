# `crates/viewport-shell-servoshell` — Driven Servo Shell

- **License**: MIT
- **Role**: Workspace-member Servo backend (`--shell-backend=servoshell`); spawns `servoshell` (`VIEWPORT_SERVOSHELL_BIN`, `VIEWPORT_SERVOSHELL_ARGS`) with `--userscripts <dir>` and no compiled Servo dependency.

## Architecture (`src/main.rs`)
- **Bridge**: Binds a loopback `TcpListener` on `127.0.0.1:0` with a per-process `/dev/urandom` token (`X-Viewport-Token` / query token) and injects `viewport_ipc::js::bridge_script` (`POST /send` outbound with `batch=1` line splitting, long-poll `GET /events` inbound with `POLL_TIMEOUT = 25s`).
- **Hardening bounds**: `MAX_REQUEST_LINE = 8 KiB`, `MAX_HEADER_LINES = 100`, `MAX_HEADER_BYTES = 64 KiB`, `MAX_BODY = 1 MiB` (checked before allocation), `MAX_CONNECTIONS = 4`, `SOCKET_TIMEOUT = 5s`, `QUEUE_LIMIT = 4096`.
- **Reload sequencing**: `shell.reload` bumps an epoch/floor so the previous page cannot consume events meant for the reloaded page.
- **Shutdown**: sends `SIGTERM` and waits `TERM_GRACE = 1500ms` before `SIGKILL` to avoid Servo teardown faults.
