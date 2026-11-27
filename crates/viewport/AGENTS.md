# `crates/viewport` — Compositor Binary

- **License**: GPL-3.0-or-later
- **Role**: Main compositor binary (`viewport`) and IPC CLI subcommand (`viewport msg`, dispatched early in `main.rs` before Wayland/seat init).

## Module Map (High-Signal Entrypoints)
- **Core State & Lifecycle**:
  - `main.rs`: CLI `OPTIONS` table (feeds `--help` + unknown-flag validation), backend selection, `_exit` teardown (skips glibc atexit handlers that abort WebKit).
  - `state.rs`: `ViewportState`, `Layer` stacking (`Tiled < Floating < Override`), `restack`, `unscale_about`. Uses `include!` for cohesive sub-files in `state/`:
    - `state/render_frame.rs`: `frame_for`, `effective_opacity`, `placed_layer_bounds`, `ViewStamp`.
    - `state/frame_barriers.rs` & `state/frame_clock.rs`: `live_surfaces` walks, vblank pacing.
    - `state/capture.rs`: `surface_under` hit-testing, offscreen screencopy/portal capture.
    - `state/config_apply.rs`: config application & reload diffing.
    - `state/outputs.rs`, `state/output_controls.rs`, `state/auto_hdr.rs`: output hotplug, `vcgt`/ICC cube loading, `cm_auto_hdr`.
    - `state/lock_power.rs`: lock ownership (`lock_owned_by_shell`), `idle.lock_delay`, `PrepareForSleep` immediate lock.
    - `state/shell_lifecycle.rs`, `state/ghost.rs`, `state/clipboard_launcher.rs`, `state/screencast.rs`.
- **Rendering & Geometry**:
  - `render.rs`: `SurfaceElement`, `OutputElement`, `ScreenElement`, `build()` (shell DMA-BUF -> layers -> windows -> ghosts -> `shell.overlay` -> top layers -> cursor -> magnifier).
  - `views.rs`: `Views` registry + cached `Index`, `ProcessIdentity`, `clip_covers` (i64-safe).
  - `rounded.rs`: staircase corner `bands`, `bands_within`, `cutaway` wedges.
  - `background_effect.rs`: `ext-background-effect-v1` + shell glass blur regions.
  - `ghost.rs` & `fade.rs`: `windowsOut` kept-frame fade (`OutStyle`, `Curve`, springs) + `fadeDim`/`fadePopups`.
  - `cross_gpu.rs` & `multigpu.rs`: foreign-GPU shadow buffer copy + `portable`/`native` dmabuf feedback.
- **Backends**:
  - `udev.rs`: DRM/KMS + libinput + libseat + `ViewportRenderer`/`VulkanRenderer` + `GbmDevice` + `recovery.rs` GPU hotplug/reset.
  - `winit.rs`: nested Wayland/X11 window with `GlesRenderer`.
  - `headless.rs`: `--headless` + lazy `ensure_subsystem` for `output.test_add` virtual outputs with surfaceless GLES.
- **Wayland / XWayland Handlers**:
  - `handlers/mod.rs`: `SeatHandler`, `SelectionHandler`, `DndGrabHandler`, `wp-pointer-warp-v1`, `screencopy`, `drm_lease`.
  - `handlers/compositor.rs`, `handlers/xdg_shell.rs`, `handlers/layer_shell.rs`, `handlers/session_lock.rs`, `handlers/xwayland.rs`.
  - Custom protocol modules: `color_management.rs`, `color_representation.rs`, `output_management.rs`, `output_power.rs`, `gamma.rs`, `screencopy.rs`, `foreign_toplevel.rs`, `tearing.rs`, `workspace.rs`, `inhibit.rs`, `anr.rs`.
- **Input, Config & IPC**:
  - `input.rs` & `keyboard_focus.rs`: `KeyboardFocus` (`Wayland` | `X11`), `binding.rs` chord matcher, `pointer.rs` constraints & `remembered_confinement`, `cursor.rs` & `hyprcursor.rs`, `magnify.rs`, `idle.rs`, `libei.rs`, `input_capture.rs`.
  - `config.rs` & `settings.rs`: bootstrap JSON + `settings.json` overlay merge (`settings::apply`).
  - `ipc.rs`, `apply.rs`, `framing.rs`, `msg.rs`: UNIX control socket, request trust/lock gates (`reads_only`, `moves_focus`, `acts_while_locked`), `view.layout` coalescing (`settle()`), `viewport msg` CLI.
- **Shell Host & D-Bus Services**:
  - `shell.rs` (`#[cfg(feature = "wpe")]` in-process WPE thread + `Mailbox`), `shell_client.rs` (out-of-process Wayland socketpair + restart backoff + exit codes `87`/`88`), `shell_backend.rs`, `shell_pacing.rs`, `shell_watch.rs`.
  - `dbus.rs`, `dbus_util.rs`, `notification.rs`, `tray.rs`, `mpris.rs`, `power.rs`, `network.rs`, `bluetooth.rs`, `shortcuts.rs`, `appearance.rs`, `launcher.rs`, `status.rs`, `ai_usage.rs`, `sound.rs`, `icc.rs`, `icon.rs`, `session.rs`, `screencast/`.

## Testing Notes
- Unit tests + integration tests in `crates/viewport/tests/` (including dual-mode `harness = false` tests re-exec'd via `VIEWPORT_SHELL_BIN`).
- `cargo check -p viewport --features wpe` and default features must both compile (pre-commit checks both).
