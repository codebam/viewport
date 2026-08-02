# Shell backends

The shell is a web page. Which engine renders it is a choice, and this is what
the choices are.

```
--shell-backend=webkitgtk   WebKitGTK, in a process of its own     implemented
--shell-backend=chromium    Chromium, driven as a child process    implemented
--shell-backend=wpe         WPE WebKit, inside the compositor      implemented
--shell-backend=servo       Servo, inside the compositor           refused
--shell-backend=cef         Chromium embedded in-process           refused
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

| | wpe | webkitgtk | chromium |
| --- | --- | --- | --- |
| pixels | `WPEBufferDMABuf` per frame | a client attaches a buffer | a client attaches a buffer |
| input | translated into engine calls | `wl_pointer` / `wl_keyboard` | `wl_pointer` / `wl_keyboard` |
| pacing | acknowledge a frame to release the next | `wl_surface::frame` | `wl_surface::frame` |
| bridge | `messageHandlers` in-process | `messageHandlers`, engine API | `messageHandlers`, DevTools protocol |
| engine | built here, hours | prebuilt, linked | prebuilt, not linked |
| a crash | takes the session | takes the shell | takes the shell |

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

## chromium — the engine in a browser, driven

`crates/viewport/src/shell_client.rs`, `crates/viewport-shell-chromium/`. The
compositor starts `viewport-shell-chromium`, which starts a browser and drives
it. Nothing links an engine: this crate compiles in seconds on a machine with
no browser installed, and the engine is whatever `chromium` the package names —
`VIEWPORT_CHROMIUM_BIN` overrides it.

The window is a Wayland client exactly as the WebKitGTK one is, on the
connection the compositor handed over, so placement, input, pacing and the
buffer are all the same code.

**The bridge is the DevTools protocol**, over `--remote-debugging-pipe` rather
than a port: a debugging *port* is a socket anything local can connect to, and
what it can do there is drive the desktop. `Runtime.addBinding` puts
`__viewport_send` in the page, `viewport_ipc::js::BRIDGE_SHIM` wraps it in the
`window.webkit.messageHandlers.viewport` name the shell was written against,
and `Runtime.evaluate` delivers the same `CustomEvent` inbound. Chromium
announces several targets and answers an attach slower than it announces the
next one, so the shell attaches once and ignores the rest — without that, the
bridge is installed three times and every message arrives in triplicate.

**It runs the GPU in the browser process.** With a GPU process of its own,
Chromium segfaults on this compositor — `exit_code=139`, three times over —
and falls back to software rendering, which means shared-memory buffers, which
the shell element cannot draw. `--in-process-gpu` produces a DMA-BUF on the
first frame. `VIEWPORT_CHROMIUM_GPU_PROCESS=1` restores the separate process
for a machine where this is not true, and is worth knowing about when
comparing this backend's numbers against the other two.

`VIEWPORT_CHROMIUM_ARGS` adds arguments to the browser's command line.

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

The same Blink as `chromium`, embedded as a library rather than driven as a
browser. What that would add is offscreen rendering — `OnAcceleratedPaint`
hands over dmabuf planes, a modifier and a format, which is very nearly
`viewport_web::Frame` already — so the shell would be a buffer in this process
rather than a client, like `wpe` and unlike everything else here.

What it costs, measured rather than guessed:

- crates.io `cef` is `149.3.0+149.0.6`; nixpkgs' `cef-binary` is `149.0.5`. One
  of the two has to move.
- `cef-dll-sys`'s build script builds `libcef_dll_wrapper` from the
  distribution with cmake and ninja, and downloads a CEF archive unless
  `CEF_PATH` holds one with an `archive.json` it recognises — which a nix
  sandbox forbids, so that file has to be produced.
- CEF re-executes the host binary for its zygote, GPU and render processes, so
  the entry point has to hand off before anything else runs.

Until that is done, `--shell-backend=chromium` is the same engine.

## Building and installing

Each package is named for the engine that draws its shell, which is the only
thing that differs between them:

```
# the engine in-process; builds WebKit
nix build .#wpe

# the engine out of process; builds no WebKit at all
nix build .#webkitgtk       # and this is `.#default`

# no engine built or linked; runs nixpkgs' chromium
nix build .#chromium
```

`.#viewport-smithay` is still an alias for `.#wpe`, because that is what any
existing pin says.

On NixOS:

```nix
programs.viewport = {
  enable = true;
  shellBackend = "webkitgtk";   # the default; also "chromium" or "wpe"
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
