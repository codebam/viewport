# Glossary

Terms a newcomer meets across Viewport's compositor and IPC docs, collected in
one place. Definitions are drawn only from
[`docs/RUST-REWRITE.md`](RUST-REWRITE.md), [`docs/ipc.md`](ipc.md),
[`docs/shell-backends.md`](shell-backends.md),
[`docs/configuration.md`](configuration.md) and the [README](../README.md).

- **Space** — `smithay::desktop::Space`, the scene graph that holds the client
  windows; the shell element renders below every space element.
- **output** — a monitor, announced to the shell with a name, make, model, serial,
  geometry and modes; `output.configure` sets its mode, scale and rotation or
  turns it off. The headless backend can plug and unplug virtual outputs.
- **view** — a client window, identified by an id: the shell learns of it in
  `view.added` and places it with `view.layout` (x, y, width, height, clip, frame).
- **shell** — the web page that is the desktop — wallpaper, dock, window frames,
  titlebars — composited zero-copy below the client windows in the `Space`.
- **layout** — where windows go, which is JavaScript's job: the shell draws a
  frame with a hole and sends the measured rect over IPC; no compositor policy.
- **workspace** — the shell's alone: it decides what a workspace is and which one
  shows. Workspaces 1–9 exist initially; `workspace.list`/`workspace.request`
  relay `ext-workspace-v1`.
- **WPEPlatform** — the WPE WebKit built with `-DENABLE_WPE_PLATFORM=ON`: no
  `libwpe`/`libwpe-fdo`; `WPEDisplay` advertises the DRM device, and
  `WPEView::render_buffer` delivers each frame as a `WPEBufferDMABuf`.
- **IPC** — the JSON protocol between shell and compositor: the page's script
  handler or an external UNIX socket of newline-delimited JSON, one dispatch
  table, byte-compatible with the C protocol.
- **vblank** — the display's frame cadence: the compositor acknowledges a frame
  from the output's frame handler, pinning the shell's paint rate to real vblank
  rather than a free-running timer.
- **headless backend** — no GPU, no display: a virtual output and a timer for
  vblank, rendering via surfaceless EGL with no DMA-BUF and no `linux-dmabuf`
  (so the shell is blank); it makes IPC and window lifecycle testable in CI.
- **XWayland** — the path for X11 clients; the `xwayland-keyboard-grab` global is
  advertised only to Xwayland, so it does not appear in `wayland-info`.
- **DMA-BUF (DMABUF)** — the zero-copy buffer WebKit renders into, so no pixel
  goes to the CPU; its fds, offsets, strides and modifier map onto
  `wlr_dmabuf_attributes`, and the shell element takes a buffer, not a surface.
