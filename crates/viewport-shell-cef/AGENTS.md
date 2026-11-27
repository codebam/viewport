# `crates/viewport-shell-cef` — Embedded CEF Shell (Excluded Crate)

- **License**: MIT
- **Workspace Status**: Excluded from root `Cargo.toml`; carries its own `[workspace]` + `Cargo.lock`; requires `CEF_PATH` (built in CI via `nix build .#viewport-shell-cef` and checked by `excluded-crates`).

## Architecture (`src/main.rs`)
- Runs `cef::execute_process` immediately at entry for CEF zygote/GPU/renderer subprocesses before browser-process setup.
- Uses CEF Views window (`Wayland` client via inherited `WAYLAND_SOCKET`) and drives the page through CEF's in-library DevTools observer (`viewport_shell_bridge::DocumentGuard`, `QUEUE` mutex drained on `Page.loadEventFired`).
- Pinned to `cef = "151.8"` to match nixpkgs `cef-binary` API version (`15101`).
- Note: windowed Chromium/CEF paints opaque `#1f1f1f` even when `background_color = 0`, so `ShellBackend::supports_transparent_background` returns `false`.
