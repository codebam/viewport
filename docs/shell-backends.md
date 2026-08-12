# Shell backends

The shell is a web page. Which engine renders it is a choice, and this is what
the choices are.

```
--shell-backend=cef         Chromium embedded through CEF          implemented, default
--shell-backend=webkitgtk   WebKitGTK, in a process of its own     implemented
--shell-backend=chromium    Chromium, driven as a child process    implemented
--shell-backend=wpe         WPE WebKit, inside the compositor      implemented
--shell-backend=servo       Servo, embedded in the shell process   implemented, built by hand
--shell-backend=servoshell  Servo, driven as a child process       implemented
```

`cef` is what the NixOS module installs and what `nix run` on this flake gives:
it builds no engine, and of the measured backends that build none it is the
cheapest per frame the shell paints. `webkitgtk` is 156 MB lighter, which on a
machine short of memory is the better trade — see
[`benchmarks.md`](benchmarks.md), which has no numbers for either Servo backend
yet.

Two of these are the same engine twice, and that is the pattern rather than an
accident: `cef` links Blink where `chromium` drives it, and `servo` links Servo
where `servoshell` drives it. Linking costs a build and buys an engine API;
driving costs a process and buys a build that takes seconds.

Two defaults that are not that one. A build with `--features wpe` uses `wpe` at
run time, because a binary that paid for the in-process engine should use it.
And a plain `cargo build` defaults to `webkitgtk`, because that is the shell
program the workspace builds beside the compositor — the cef crate is outside
the workspace and its binary is not in `target/`.

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

| | wpe | webkitgtk | chromium | cef | servo | servoshell |
| --- | --- | --- | --- | --- | --- | --- |
| pixels | `WPEBufferDMABuf` per frame | a client attaches a buffer | a client attaches a buffer | a client attaches a buffer | a client attaches a buffer | a client attaches a buffer |
| input | translated into engine calls | `wl_pointer` / `wl_keyboard` | `wl_pointer` / `wl_keyboard` | `wl_pointer` / `wl_keyboard` | `wl_pointer` / `wl_keyboard`, translated into engine calls | `wl_pointer` / `wl_keyboard` |
| pacing | acknowledge a frame to release the next | `wl_surface::frame` | `wl_surface::frame` | `wl_surface::frame` | `wl_surface::frame` | `wl_surface::frame` |
| bridge | `messageHandlers` in-process | `messageHandlers`, engine API | DevTools over a pipe | DevTools through the library | intercepted `fetch`, engine API | `fetch` at a loopback server |
| engine | built here, hours | prebuilt, linked | prebuilt, not linked | prebuilt, linked | built there, hours | prebuilt, not linked |
| a crash | takes the session | takes the shell | takes the shell | takes the shell | takes the shell | takes the shell |

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

## servo — the engine embedded, and compiled

`crates/viewport/src/shell_client.rs`, `crates/viewport-shell-servo/`. A
Wayland client like the other out-of-process backends — winit window,
`WindowRenderingContext` over its raw handles, `WebView::paint` into it — so
placement, input, pacing and the buffer are the same code they are for
WebKitGTK.

**What is different is the build, and it is confined on purpose.** Servo is not
a package: it is an rlib in a language with no stable ABI, so an embedding is a
cargo dependency on the engine's source and building it builds Servo. That
crate is therefore *outside this workspace*, with a lock file and a
`[workspace]` table of its own, exactly as `viewport-shell-cef` is. Nothing a
compositor rebuild, `cargo test --workspace`, the pre-commit hook or CI does
can reach it. It compiles when somebody cd's into it and asks:

```
nix develop            # the workstation shell; it carries Servo's recipe
cd crates/viewport-shell-servo
cargo build --release  # once. hours.
```

and the resulting binary goes beside `viewport` in `bin/`, or is named with
`VIEWPORT_SHELL_BIN`.

There is no `nix build .#servo` for the same reason: a package would put that
build inside every evaluation that touched it. `servoshell` below is the same
engine with none of this.

**The bridge is built out of the web platform**, because Servo has no
`window.webkit.messageHandlers`. Outbound is a `fetch` at
`http://viewport-ipc.invalid/send?m=…` with `mode: 'no-cors'`, intercepted by
`WebViewDelegate::load_web_resource` before it reaches the network and answered
with an empty 200 — a real embedder API rather than the usual trick of reading
`window.prompt` or console output, and no preflight, no origin and no blocking
of the page's script thread. Inbound is `WebView::evaluate_javascript`
delivering the same `CustomEvent('viewport')` every other backend delivers.
`viewport_ipc::js::BRIDGE_SHIM`, injected as a user script before the page's
own scripts, hangs the familiar name on the sender. `data/shell/*.js` needs no
edit.

**The keyboard conversion is deliberately small.** `Code` — the physical key —
is left `Unidentified`: the compositor owns every shortcut and the shell's
pages read `event.key`. Servo's own port has the full table in
`ports/servoshell/desktop/keyutils.rs` if that stops being true.

`crates/viewport-web/src/dmabuf.rs` is still the older spike at the *other*
shape of this — Servo painting into a buffer the compositor owns, through a
`RenderingContext` of ours, in the compositor's own process the way `wpe` runs
WebKit. It works, on hardware, across two EGL displays; what this backend
chooses instead is the surface every other out-of-process shell already uses,
which is a window. `docs/RUST-REWRITE.md` has that analysis.

## servoshell — the engine driven

`crates/viewport/src/shell_client.rs`, `crates/viewport-shell-servoshell/`. The
same engine as `servo`, and the same relationship to it that `chromium` has to
`cef`: nixpkgs' `servo` package installs a browser called `servoshell`, and a
browser can be started as a child process. This crate compiles in seconds on a
machine that has never built an engine, and it is a workspace member because it
links nothing. `VIEWPORT_SERVOSHELL_BIN` names the browser;
`VIEWPORT_SERVOSHELL_ARGS` adds to its command line.

**The bridge is a loopback HTTP server in the shell process**, reached by a
script `servoshell --userscripts` injects into the page. Servo's two drivable
surfaces were both rejected for this, and the reasons are worth keeping:

- Its **devtools server** speaks the Firefox remote debugging protocol, whose
  actor layout is Servo's own partial implementation and moves between
  releases. A backend written against it breaks when the installed browser
  changes.
- Its **WebDriver server** is the best-tested surface it has — it is what runs
  WPT — but a WebDriver session executes one command at a time, so a long poll
  for outbound messages is a session that cannot deliver inbound ones.

Both are TCP ports anyway, so neither buys the isolation that made
`--remote-debugging-pipe` worth insisting on for Chromium. The injected script
gets the same two directions with no protocol to drift: `fetch` to `POST /send`
outbound, a long poll of `GET /events` inbound dispatching the usual
`CustomEvent`, and `viewport_ipc::js::BRIDGE_SHIM` over the top.

**The server is 127.0.0.1 on an ephemeral port, and every request carries a
token** read from `/dev/urandom` at startup and known only to the injected
script. This is not decoration: what that server can do is drive the desktop,
and loopback alone is not an access control on a multi-user machine.

Two consequences worth knowing before choosing this backend:

- **A shell page served over `https` cannot reach it.** The bridge is `http`
  on loopback, and a secure page's fetch to it is mixed content. `file://` and
  `http://localhost` — which is what the bundled shell and a development server
  are — are fine.
- **`shell.reload` reloads the page rather than the engine.** There is no
  engine handle here to call, so the compositor's reload arrives as an item in
  the poll and the page calls `location.reload()`, which injects the script
  again with it.

## cef — the engine embedded

`crates/viewport/src/shell_client.rs`, `crates/viewport-shell-cef/`. The same
Blink as `chromium`, linked in rather than driven: no browser binary, no
DevTools pipe over a socket, one process fewer. The window is CEF's Views
framework on Wayland; the compositor sees an ordinary client on the connection
it handed over, as with the other two out-of-process backends.

**The bridge goes straight into the library.**
`add_dev_tools_message_observer` and `send_dev_tools_message` on the browser
host give the same `Runtime.addBinding` / `Runtime.evaluate` pair the
`chromium` backend uses over a pipe, without its target discovery — the host
*is* the page. Lines from the socket reach CEF's UI thread through
`post_task`, because a `Browser` may not leave the thread that made it.

**Nothing here builds Chromium.** nixpkgs' `cef-binary` is a prebuilt 1.3 GB
`libcef.so`. The only thing compiled is `libcef_dll_wrapper`, from the
distribution's own source, with cmake and ninja.

Five things had to be right, none of which says so when it is wrong:

- **The layout.** `cef-dll-sys` wants the tree its downloader produces, which
  is flattened: `Release/` *is* the root, with `Resources/` emptied into it
  beside `include/`, `libcef_dll/`, `cmake/` and `CMakeLists.txt`. nixpkgs'
  unflattened layout fails on a missing `locales`, and on the second attempt
  reads as a permissions error, because the first left read-only copies in
  `target/`.
- **The download.** The build script fetches a CEF of its own unless
  `CEF_PATH` holds an `archive.json` — `{type, name, sha1}`, and only `name`
  is read. The check is `archive <= expected`, so nixpkgs' 149.0.5 satisfies a
  crate built against 149.0.6. There is no version skew to resolve.
- **The API version.** Every CEF structure carries one, set by the first call
  to `api_hash`. Without it the process dies with `CefApp_0_CToCpp called with
  invalid version -1`.
- **When a window may be made.** From `on_context_initialized`, not when
  `initialize` returns. Early, it traps thirteen seconds later with no message.
- **`can_resize`.** Every delegate method that is not implemented answers
  `Default::default()`, which for a `c_int` is 0 — so a delegate that says
  nothing says the window cannot be resized, Chromium tells the compositor its
  minimum and maximum are both 800x600, and the desktop is drawn 800x600 in
  the corner of the screen whatever the layout is.

The crate is outside the workspace: it does not build without `CEF_PATH`, and
`cargo test --workspace` is what the pre-commit hook and CI run. It carries its
own lock file and its own `[workspace]` table for the same reason.

Offscreen rendering is the piece still worth doing here, and the reason this
backend exists rather than only `chromium`: `OnAcceleratedPaint` hands over
DMA-BUF planes, a modifier and a format, which is very nearly
`viewport_web::Frame` — so CEF is the route to running Blink *in* the
compositor the way `wpe` runs WebKit. Today it is a Wayland client like the
others.

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

# the same engine, embedded; builds a C++ wrapper and no engine
nix build .#cef

# Servo, in nixpkgs' servoshell; builds no engine either
nix build .#servoshell
```

There is no `.#servo`. The embedded Servo shell is a cargo dependency on the
engine's source, so a package would be a Servo build inside every evaluation
that touched this flake; it is built by hand instead, once, and the section
above says how. That is the whole reason there are two Servo backends.

`.#viewport-smithay` is still an alias for `.#wpe`, because that is what any
existing pin says.

On NixOS:

```nix
programs.viewport = {
  enable = true;
  shellBackend = "cef";   # the default; also "webkitgtk", "chromium",
                          # "servoshell" or "wpe"
};
```

`servo` is not in that enum, because it has no package to point the module at.
A system that wants it sets `programs.viewport.package` to a build carrying
`viewport-shell-servo`, and `shell_backend = "servo"` through `settings`.

The compositor finds the shell program — `viewport-shell-gtk`,
`viewport-shell-servoshell`, whichever the backend names — beside itself in
`bin/`, then on `PATH`; `VIEWPORT_SHELL_BIN` overrides both. A backend whose
program is not installed says so by name rather than falling back to another
engine: "not installed" and "no such backend" are different answers and only
one of them is worth quietly drawing the desktop with something else.

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

Every shell program takes the same two arguments, so `viewport-shell-servo` and
`viewport-shell-servoshell` are started the same way. `--inspector` differs:
for `servoshell` it opens Servo's devtools server on port 6080, and for the
embedded `servo` there is nothing to open — the log says so rather than
pretending otherwise.
