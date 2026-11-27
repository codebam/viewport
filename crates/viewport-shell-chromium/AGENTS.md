# `crates/viewport-shell-chromium` — Driven Chromium Shell

- **License**: MIT
- **Role**: Default out-of-process shell (`--shell-backend=chromium`); spawns `chromium` (`VIEWPORT_CHROMIUM_BIN`, plus `VIEWPORT_CHROMIUM_ARGS`) on the compositor-provided Wayland socket and drives it over `--remote-debugging-pipe` (fds 3/4).

## Architecture (`src/main.rs`)
- Zero browser-library dependencies: speaks JSON-RPC CDP over pipe fds; isolates profile via `private_dir("viewport-shell")`.
- Uses `viewport_shell_bridge::DocumentGuard` to track main frame, default execution context, and `LoadGate` (`Page.loadEventFired` drains `queued`, `QUEUE_LIMIT = 512`).
- Injects `viewport_ipc::js::BRIDGE_SHIM` + `SUBFRAME_GUARD` via `Page.addScriptToEvaluateOnNewDocument`; registers `Runtime.addBinding` (`__viewport_send`); dispatches inbound events via `Runtime.evaluate`.
- Attaches to the first page target only (ignores service workers/extra targets) so the bridge is installed once.
- Passes `--in-process-gpu` by default (unless `VIEWPORT_CHROMIUM_GPU_PROCESS=1`) so Chromium allocates DMA-BUFs instead of crashing/falling back to SHM.
- Drops excess inbound compositor events on channel backpressure, but blocks on `Line::Closed` (`forward_compositor`).
