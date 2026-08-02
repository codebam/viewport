# Shell backends

The shell is a web page. Which engine renders it is a choice, and this is what
the choices are.

```
--shell-backend=webkitgtk   WebKitGTK, in a process of its own     implemented
--shell-backend=wpe         WPE WebKit, inside the compositor      implemented
--shell-backend=servo       Servo, inside the compositor           refused
--shell-backend=cef         Chromium through CEF, inside it        refused
```

`webkitgtk` is what the NixOS module installs unless told otherwise, because
it is the one that installs without building a browser engine. A build with
`--features wpe` still defaults to `wpe` at run time: a binary that paid for
the in-process engine should use it.

The name can also come from `VIEWPORT_SHELL_BACKEND`, or from `shell_backend`
in the config file, in that order of precedence. A name that cannot be honoured
is reported and fallen back from rather than being fatal: this is the setting
that decides whether there is a desktop, and a compositor that refuses to start
over one line in a config file leaves nothing to log in to and fix it with.

## Why there is a choice at all

WPE WebKit is the only engine that hands an embedder a DMA-BUF per frame
without being argued with, which is why it is what this project started on. It
is also packaged by nobody. `flake.nix` builds it from the upstream tarball
against nixpkgs' `webkitgtk` dependency closure — about four hours, and no
binary cache has it.

WebKitGTK is the same WebKit, one port apart, and nixpkgs ships it prebuilt.

## wpe — the engine in this process

`crates/viewport/src/shell.rs`, `crates/viewport-web/`. Needs
`--features wpe` at build time; without it the binary says so and falls back.

WebKit runs on a thread of its own with a `GMainContext` of its own. Frames
arrive as `WPEBufferDMABuf` and become the shell element; input is translated
into engine calls; the page is spoken to by evaluating JavaScript in it.

This is the backend with the fewest moving parts at run time and the most at
build time. The shell cannot crash independently of the compositor, because it
is not independent of the compositor.

## webkitgtk — the engine in a process of its own

`crates/viewport/src/shell_client.rs`, `crates/viewport-shell-gtk/`. Always
compiled; needs no feature flag, because the compositor side of it is Wayland
and nothing else.

The compositor makes a socket pair, inserts one end as a Wayland client marked
as the shell, and starts `viewport-shell-gtk` on the other. Everything after
that is ordinary:

| | wpe | webkitgtk |
| --- | --- | --- |
| pixels | `WPEBufferDMABuf` per frame | a client attaches a buffer |
| input | translated into engine calls | `wl_pointer` / `wl_keyboard` |
| pacing | acknowledge a frame to release the next | `wl_surface::frame` |
| bridge | `messageHandlers` in-process | `messageHandlers` over the control socket |
| a crash | takes the session | takes the shell |

The page needs no edit for either: WebKitGTK has the same user-content API WPE
does, so `window.webkit.messageHandlers.viewport` is real rather than shimmed,
and inbound events are the same `CustomEvent` built by `viewport_ipc::js`.

**Identity is the connection, not the `app_id`.** A client that named itself
`dev.viewport.shell` would otherwise be able to take the desktop's place —
drawn under every window, handed every click that misses one. The shell's
connection was made by the compositor and handed to a process it spawned, and
nothing that connected the ordinary way can claim it.

**Placement.** The shell is one page across the whole output layout, so its
toplevel is configured to the layout's size and its buffer is drawn through the
existing shell element rather than being mapped into the `Space` as a window.
That is also what keeps `shell.overlay` — the parts of the shell drawn back on
top of the windows — working unchanged.

**It must paint into a DMA-BUF.** The shell element takes a buffer, not a
surface. A shell painting into shared memory is reported once, loudly, and
nothing is drawn; that is what a machine whose GTK stack cannot reach a render
node looks like. The headless backend advertises no `linux-dmabuf` at all, so
the shell is expected to be blank there — it is still useful for testing that
the process starts, is adopted and talks.

**Recovery.** WebKit's web process can die without taking the shell process
with it: the shell reloads. If it dies three times, the shell exits asking to
be started again with `WEBKIT_DISABLE_DMABUF_RENDERER=1`, and the compositor
does that and says so. WebKit then transfers the page through shared memory
internally, one copy more; the window's own buffer is still a DMA-BUF, so the
handoff to the compositor is zero-copy either way. This is not hypothetical —
it is what WebKit's web process does today against the nested backend.

If the shell process itself exits, the compositor restarts it, five times in a
minute, then leaves it down and says so. The desktop is blank at that point;
the compositor is not.

## servo — refused

The original plan for the rewrite, and the buffer handoff is already spiked:
`crates/viewport-web/src/dmabuf.rs` allocates with GBM, imports as an
`EGLImage`, hangs it off an FBO and exports a fence, verified on hardware
across two EGL displays. What is missing is a `RenderingContext` and a
`WebEngine` over it. `docs/RUST-REWRITE.md` has the analysis.

nixpkgs has a `servo` package, but it builds servoshell — an embedding is a
cargo dependency on the `servo` crate either way, so this buys a second engine
rather than a shorter build.

## cef — refused

CEF's offscreen rendering hands `OnAcceleratedPaint` dmabuf planes, a modifier
and a format on Linux, which is very nearly `viewport_web::Frame` already, and
nixpkgs' `cef-binary` is a prebuilt blob — no engine build at all. The cost is
a C++ API and a multi-process model to host.

## Building and installing

Each package is named for the engine that draws its shell, which is the only
thing that differs between them:

```
# the engine in-process; builds WebKit
nix build .#wpe

# the engine out of process; builds no WebKit at all
nix build .#webkitgtk       # and this is `.#default`
```

`.#viewport-smithay` is still an alias for `.#wpe`, because that is what any
existing pin says.

On NixOS:

```nix
programs.viewport = {
  enable = true;
  shellBackend = "webkitgtk";   # the default; "wpe" builds WebKit instead
};
```

The compositor finds `viewport-shell-gtk` beside itself in `bin/`, then on
`PATH`; `VIEWPORT_SHELL_BIN` overrides both.

## Running the shell by hand

The whole point of a shell in its own process is that it can be started by
hand, against a compositor that is already up, without restarting the session:

```
VIEWPORT_IPC_SOCKET=$XDG_RUNTIME_DIR/viewport-$WAYLAND_DISPLAY.sock \
  viewport-shell-gtk --url http://localhost:3000 --inspector
```

It will be an ordinary window rather than the desktop — it did not get the
compositor's connection, so it is not the shell — but the bridge is the same
one, so the page runs, talks, and can be opened in the web inspector.
