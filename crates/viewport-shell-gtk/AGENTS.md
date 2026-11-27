# `crates/viewport-shell-gtk` — Out-of-Process WebKitGTK Shell

- **License**: MIT
- **Role**: Prebuilt WebKitGTK 6.0 (`gtk4` + `webkit6`) shell process (`--shell-backend=webkitgtk`, also `cargo build` default when `--features wpe` is off).

## Architecture (`src/main.rs`)
- Connects to `viewport-shell-bridge` and forwards `Line`s to the GTK main context via `async_channel::bounded` (`QUEUE_LIMIT = 512` pre-commit queue drained on `LoadEvent::Committed`).
- Registers native `UserContentManager::register_script_message_handler("viewport", None)` + injects `viewport_ipc::js::SUBFRAME_GUARD` across all frames; enforces `shell_url_allowed` on navigation/URI changes.
- Translates `VIEWPORT_SHELL_TRANSPARENT` into transparent GTK window + WebView background (enabling `background_terminal` behind the shell).
- **Exit-code supervisor contract with `crates/viewport/src/shell_client.rs`**:
  - `87` (`RETRY_WITHOUT_DMABUF`): after 3 web-process crashes, asks compositor to respawn with `WEBKIT_DISABLE_DMABUF_RENDERER=1`.
  - `88` (`DEGRADED_EXHAUSTED`): after 5 degraded reload attempts (`DEGRADED_RELOAD_LIMIT`), tells compositor to stop respawning that slot.
