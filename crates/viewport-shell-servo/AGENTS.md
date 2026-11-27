# `crates/viewport-shell-servo` — Embedded Servo Shell (Excluded Crate)

- **License**: MIT
- **Workspace Status**: Excluded from root `Cargo.toml`; carries its own `[workspace]` + `Cargo.lock` because `servo` is a source git dependency that takes hours to compile (`cd crates/viewport-shell-servo && cargo build --release` inside `nix develop`).

## Architecture (`src/main.rs`)
- Creates a `winit` window on the compositor-provided `WAYLAND_SOCKET`, hands raw handles to Servo's `WindowRenderingContext`, and runs `WebView::paint()`.
- Translates `winit` pointer/scroll/keyboard events into Servo `InputEvent`s (leaving physical `Code::Unidentified` since compositor owns shortcuts and shell reads `event.key`).
- Bridge uses `viewport_ipc::js::bridge_script` (`fetch` to `http://viewport-ipc.invalid/send?m=...` intercepted in `WebViewDelegate::load_web_resource` + `WebView::evaluate_javascript` inbound).
- Installs `rustls` `aws-lc-rs` crypto provider before Servo initialization; pins `winit`, `url`, `euclid` versions to match Servo's workspace.
