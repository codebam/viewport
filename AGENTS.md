# Viewport — Agent Orientation

## Workspace & Toolchain
- Language: Rust (edition 2021) + vanilla JS/CSS/HTML (`data/shell/`, classic scripts loaded in `data/shell/index.html` order).
- Dev shell: run all Cargo/Node commands inside `nix develop .#rust` (plain `nix develop` pulls WPE/Servo closures; `.#wpe` builds `wpewebkit-2.54.2` from source).
- Workspace members (`Cargo.toml`): `viewport`, `viewport-ipc`, `viewport-shell-bridge`, `viewport-shell-chromium`, `viewport-shell-gtk`, `viewport-shell-servoshell`, `viewport-web`.
- Excluded crates (own `Cargo.lock` + `[workspace]`): `viewport-shell-cef` (needs `CEF_PATH`), `viewport-shell-servo` (compiles `servo` from source).
- External pins:
  - `smithay`: fork `codebam/smithay` branch `atomic-exit-restore` (`Cargo.toml` + `[patch."https://github.com/Smithay/smithay"]`).
  - `viewport-vulkan`: external repo `codebam/viewport-vulkan` (MIT); both pins must stay aligned on Smithay rev bumps.

## Space & Shell-Element Layering (Core Invariant)
- **Compositor has no layout policy**: `data/shell/` lays out `.viewport` holes via CSS/DOM, measures them (`geometry.js`), and sends `view.layout` rects over IPC.
- **`Views` vs `Space`**: `ViewportState.views` (`crates/viewport/src/views.rs`) tracks all windows from creation (`mapped`, `placed`, `visible`, `minimized`, `capture_allowed`); `ViewportState.space` (`smithay::desktop::Space<Window>`) holds only mapped+placed windows.
- **Stacking bands (`crates/viewport/src/state.rs` `Layer`)**: `Tiled < Floating < Override` (X11 override-redirect).
- **Render pipeline**: `state/render_frame.rs` (`ViewportState::frame_for`) builds renderer-independent `render::Frame`; `render.rs` (`build`) emits `OutputElement<R>` wrapped in `ScreenElement<R>` (optional `Magnified` `RelocateRenderElement<RescaleRenderElement<_>>`).
- **Bottom-to-top per-output stack**:
  1. `Shell`: single DMA-BUF texture spanning the entire output layout (placed at `-output_origin`), below all `Space` elements.
  2. `LayerFrame`: wlr-layer-shell `Background` and `Bottom` layers.
  3. `WindowFrame`: windows in `Space` order (`CropRenderElement` clip, `RoundedRenderElement` corners, `RescaleRenderElement` overview thumbnails, `BackgroundEffect` blur behind `SurfaceElement`, `Redaction` `SolidColorRenderElement` when `!capture_allowed`, floating `RoundedShell` corners below / `CroppedShell` border sides above).
  4. `Ghost` (`windowsOut` closing-window `Memory` buffers via `ghost.rs` / `fade.rs`).
  5. `shell.overlay` (`CroppedShell` rects lifted *above* windows for floating frames, dialogs, menus, OSK, notifications + optional `BackgroundEffect`).
  6. `LayerFrame`: `Top` and `Overlay` layers.
  7. `Cursor`: `WlSurface` or themed `Image` (`cursor.rs` / `hyprcursor.rs`).
- **Lock override**: if `locked && lock_owned_by_shell`, draw only shell buffer (fail closed black until `session.lock.drawn` + 1 frame); if external `ext-session-lock-v1`, draw only its `lock_surface`.
- **Hit-testing (`state/capture.rs`, `input.rs`)**: checks lock ownership -> `Top`/`Overlay` layers -> `shell.overlay` rects (respecting `keep_focus` for OSK) -> `Space::element_under()` (with `views::clip_covers`, `unscale_about` in overview) -> fallback to shell element.

## Licensing Boundary
- `crates/viewport` is **GPL-3.0-or-later**.
- All other crates (`viewport-ipc`, `viewport-shell-bridge`, `viewport-web`, `viewport-shell-*`) and `data/shell/` are **MIT**.
- **Rule**: never copy GPL code from `crates/viewport` into MIT crates or upstream Smithay.

## Verification Gates
- `scripts/check-versions.sh` (checks 6 version sites: root `Cargo.toml`, `viewport-shell-cef/Cargo.toml`, `viewport-shell-servo/Cargo.toml`, 2x `flake.nix`, `package.json`).
- `scripts/vendor-shell-deps.sh --check`
- `nix develop .#rust --command bash -c 'cargo fmt --all --check && cargo deny check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace'`
- Wayland integration suite: `cargo build --bin viewport --bin viewport-shell-gtk && scripts/integration.sh target/debug/viewport`
- Shell JS suites: `for m in tiling scrolling solar matrix canvas; do node tests/shell.test.js data/shell $m; node tests/shell.test.js data/shell $m session; done && node tests/kiosk.test.js examples/kiosk`
- Pre-commit (`.githooks/pre-commit`): also runs `cargo check -p viewport --features wpe` (via container or `.#wpe`).

## Reference Index
- Per-crate summaries: `crates/*/AGENTS.md`
- IPC contract & wire tables: `docs/ipc.md`, `docs/stability.md`
- Shell architecture & script load order: `data/shell/shell.md`, `data/shell/index.html`
- Known defects & audit history: `FIXES.md`, `docs/audit-2026-11-13.md`, `docs/audit-2026-11-24.md`, `docs/audit-2026-11-25.md`
- Open items: `docs/roadmap.md`
