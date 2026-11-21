# Viewport — project thesis

Viewport is a Wayland compositor whose entire desktop shell — wallpaper, dock, window frames,
titlebars, bar, overview and lock screen — is a web page. Smithay owns DRM/KMS, outputs, modes
and vblank, input devices, the Wayland protocols, focus and hit-testing of real clients, and
painting client surfaces; the browser is the layout and paint engine for the desktop chrome, not the window system.

The boundary is one buffer: the page is a full-screen element below every client surface, and an
application's own contents are never DOM. The compositor paints real Wayland clients on top, and
`Space::element_under()` returns a client surface over a window and the shell element elsewhere,
so hit-testing follows the layering rather than a second geometry model that can go stale. A client
cannot be styled from the shell, and a broken page cannot repaint a terminal.

Layout lives in CSS, not Rust. The tiling tree is nested flexboxes: the browser computes every
rectangle, the shell measures where the hole (`<div class="viewport">`) landed and sends those rects
over IPC, and the compositor moves client scene nodes there. The IPC carries layout results and
state, not pixels or draw commands, so there is no second layout implementation in Rust to keep in
sync. Floating-window borders are drawn a second time above the windows — an explicit part of the protocol.

Zero-copy is the point: the engine hands the compositor one DMA-BUF per frame and the compositor
presents real client surfaces next to it, so neither ever hands a pixel to the CPU. A frame is
acknowledged from the output's frame handler, so pacing is real vblank rather than a timer; explicit
sync imports a rendering fence into a `drm_syncobj` timeline and waits in the render pass rather than blocking the compositor.

The engine is a choice, and the page cannot tell which one drew it: `chromium` (the default — nixpkgs'
Chromium as a child process over DevTools, with nothing in the closure linking an engine), `webkitgtk`
(its own process, an ordinary Wayland client), `servoshell` (driven the same way), `cef` (Blink embedded),
`wpe` (WPE WebKit inside the compositor, built from source), and embedded `servo`. `nix run github:codebam/viewport`
gives a compositor and a desktop from a flake with no WebKit compiled anywhere.

Stability is promised at the boundary, not in the internals. While the version is `0.x` the covered
surfaces are still settling and may change in a minor release; `1.0.0` is where the promise becomes
a commitment — breaking a covered surface costs a major version. Covered: the IPC protocol and its
tolerance rules, config schema, extension contracts, CLI and environment, NixOS module, package
identity and advertised Wayland globals. Additions are free, unknown fields and flags are ignored,
and both directions tolerate a peer one release ahead. Not covered: the compositor's Rust internals,
the reference shell's DOM/CSS/JS, log text and benchmark numbers, and engine-specific behaviour of
experimental backends. Of the six backends, `chromium`, `webkitgtk`, `wpe` and `servoshell` are
supported and `cef` and embedded `servo` are experimental; a security fix may change behaviour in any release.
