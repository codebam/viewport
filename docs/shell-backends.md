# Shell backends

The shell is a web page. Which engine renders it is a choice, and this is what
the choices are.

```
--shell-backend=servoshell  Servo, driven as a child process       implemented, default
--shell-backend=cef         Chromium embedded through CEF          implemented
--shell-backend=webkitgtk   WebKitGTK, in a process of its own     implemented
--shell-backend=chromium    Chromium, driven as a child process    implemented
--shell-backend=wpe         WPE WebKit, inside the compositor      implemented
--shell-backend=servo       Servo, embedded in the shell process   implemented, built by hand
```

`servoshell` is what the NixOS module installs and what `nix run` on this flake
gives: it builds no engine, and it is the lightest desktop of the five that
have been measured — 8.5% of a core under load against 9.9 to 11.5, 357 MB
against 449 to 639, four processes against nine to twelve.

It is also the slowest to paint, and by enough to be the first thing to know
about the default: 14 frames a second under that load against 43 to 48, which
is 0.607% of a core per frame the shell actually painted against 0.230 to
0.261. The cheapest desktop and the most expensive engine are the same fact
seen twice. `cef` is the one to name for a desktop that should feel quick, and
`webkitgtk` for a machine short of memory rather than CPU — see
[`benchmarks.md`](benchmarks.md). The embedded `servo` backend has not been
measured at all.

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

If the shell process itself exits, the compositor restarts it. The first
restart is immediate and each one after it waits twice as long — 1s, 2s, 4s,
8s — and once five have gone by inside a minute it keeps trying every thirty
seconds instead. A process that lives longer than a minute ends the run, and
the next crash starts over from an immediate restart.

It is never given up on. Backing off is the point: the fault that this policy
was written for was a GPU that had run out of memory, where every client on
that GPU was dying at once and each restart asked it for another full-screen
buffer. Restarting harder makes that worse, and stopping altogether leaves the
desktop blank for the rest of the session even after the cause — a game, a
capture, a model loaded on the same card — has gone away. So the shell keeps
knocking, slowly. A page that genuinely cannot load costs a blank desktop and
one line in the log every thirty seconds.

There is one exit that is believed instead of answered. A degraded shell
reloads slowly and counts its tries, and when they have run out it exits with
status 88 rather than reload for ever. The compositor takes that at its word:
it logs the surrender and leaves that page down. Starting it again would put
the same page back on the same GPU and rebuild, at whatever pace, the storm
the cap exists to stop. The rest of the session keeps running, and the page
comes back only through something human — a monitor arriving while another
page is still running starts every planned page afresh — or a session restart.

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

**Servo has no CSS grid, and the shell used to need one.** Both Servo backends
run the same engine, so this is about the page rather than about either
backend: `display: grid` is dropped outright — `Unsupported property
declaration: 'display: grid'` in the log — and the box falls back to `block`.
The overview became a single column of thumbnails running off the bottom of the
screen and the empty state a mark in the top-left corner. Both are flexbox now
(`data/shell/shell.css`), which changes nothing for the other four engines: the
thumbnails were already sized in pixels by `renderOverview`, so a wrapping flex
row puts them where the grid tracks did. `tests/shell.test.js` fails if a grid
comes back.

Three further declarations are dropped and left in the sheet, because they cost
nothing where they are not understood and are what the other engines want:
`text-overflow: ellipsis` (a long window title in a thumbnail label is clipped
rather than ellipsised), `user-select: none` and `color-scheme: dark`.

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
  again with it. Every other backend calls `reload` on its engine and the
  instruction never enters the event stream at all; this is the only one where
  it does, and the only one that therefore has to make sure the page a reload
  creates does not read the instruction that created it. The poll carries
  `fresh=1` on a new page's first request for exactly that reason — without it
  the log is replayed whole, the reload is in it, and the shell reloads for as
  long as anyone watches while every window on screen stays untouched.

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

`VIEWPORT_CEF_ARGS` adds switches to the browser process's command line, the
way `VIEWPORT_CHROMIUM_ARGS` does for the other Blink backend. The switch it
was added for is `--force-renderer-accessibility`; see *The accessibility
tree* below.

The crate is outside the workspace: it does not build without `CEF_PATH`, and
`cargo test --workspace` is what the pre-commit hook and CI run. It carries its
own lock file and its own `[workspace]` table for the same reason.

Offscreen rendering is the piece still worth doing here, and the reason this
backend exists rather than only `chromium`: `OnAcceleratedPaint` hands over
DMA-BUF planes, a modifier and a format, which is very nearly
`viewport_web::Frame` — so CEF is the route to running Blink *in* the
compositor the way `wpe` runs WebKit. Today it is a Wayland client like the
others.

## The accessibility tree

The desktop is a web page, which means the engine has already built a real
accessibility tree out of it and there is no bespoke work to do on the page's
side beyond getting the markup right — which
[`data/shell/shell.md`](../data/shell/shell.md) covers and
[`data/shell/keys.js`](../data/shell/keys.js) is most of. What is left is the
last hop: whether the engine drawing the shell hands that tree to AT-SPI, where
Orca and everything like it will find it. The answer differs per backend, and
this is what it is.

**A session has to have an accessibility bus at all, and a compositor started
from a TTY has nothing behind it that would have supplied one.** `org.a11y.Bus`
is `at-spi2-core`'s, D-Bus activated, and on a machine with no desktop
environment installed it is not merely stopped — it is not activatable, so
every backend's tree goes nowhere and nothing in any log says why. The NixOS
module in `flake.nix` therefore sets `services.gnome.at-spi2-core.enable` by
default; anything else wants the distribution's equivalent. It costs nothing
until a screen reader asks, because the launcher is activated rather than run.
Check with `busctl --user list --activatable | grep a11y` before believing
anything below is broken.

**`webkitgtk` is the backend to name, and it needs no code at all.** The
`WebKitWebView` is the child of a real `gtk::ApplicationWindow` that is
`present()`ed inside a live GTK main loop
(`crates/viewport-shell-gtk/src/main.rs`), which is the one structural
condition everybody assumes is missing here — GTK4 publishes AT-SPI directly
for widgets in a realized, mapped root, and a view in no window has no
accessible to publish. Both halves speak the protocol themselves rather than
through a bridge library: `libgtk-4.so` carries the `org.a11y.atspi.*`
interfaces and the `Socket` the web content is embedded through, and
`libwebkitgtk-6.0.so` carries the same set plus `WEBKIT_A11Y_BUS_ADDRESS` for
pointing the web process at a bus it would not otherwise find. The compositor
builds the shell's environment with `.env()` and never clears it
(`crates/viewport/src/shell_client.rs`), so `DBUS_SESSION_BUS_ADDRESS` reaches
the shell process untouched. Nothing in this tree was in the way; the bus was.

**`chromium` and `cef` are the same engine and the same answer.** Blink builds
its accessibility tree lazily and publishes it once it notices a screen reader
on the bus, which is the behaviour to want — a tree built for a session nobody
is reading costs memory in every renderer for nothing. Both are ordinary
Wayland clients with a real toplevel, so there is nothing structural to fix.
Where the negotiation does not happen, `--force-renderer-accessibility` is the
lever: `VIEWPORT_CHROMIUM_ARGS` has always carried it, and `VIEWPORT_CEF_ARGS`
now exists for the same reason. Neither is passed by default. The one thing
worth writing down before it is forgotten: the offscreen `OnAcceleratedPaint`
mode described above would take the toplevel away, and with it Chromium's
ability to publish anything — that route needs `CefAccessibilityHandler`, which
serialises the tree to the embedder and leaves this tree to speak AT-SPI on its
behalf. That is a component, not a switch, and it should be costed before the
offscreen work starts rather than discovered by it.

**`wpe` cannot do this, and it is the backend furthest from being able to.**
There is no toolkit anywhere in its path. The `WebKitWebView` lives on a
`WPEDisplay` this project invented (`crates/viewport-web/shim/viewport-shim.c`)
whose only output is a DMA-BUF handed to the compositor, driven from a bare
`GMainContext` (`crates/viewport/src/shell.rs`) — a GLib main context, which is
enough for D-Bus and is not a toolkit. So there is no `GtkWidget`, no `GtkRoot`
and no window-system window, and therefore no accessible object on the
UI-process side for the web process's tree to be embedded under. WPE's web
process will publish its own tree perfectly happily; an AT walking the bus
finds an orphan with no application to hang it from. Making this work means
writing that root by hand against `org.a11y.atspi.Socket` — a new component in
the compositor, not a flag on an existing one. It has not been done because
the same effort spent anywhere else buys more, and because `webkitgtk` is one
`--shell-backend` away and is the same engine. Should an in-process accessible
root ever be written — most plausibly alongside the CEF offscreen work above,
which needs the identical thing — this paragraph should be replaced by it.

**`servo` and `servoshell` are blocked on a missing adapter rather than on
anything here.** Servo builds an AccessKit tree: `accesskit` is a dependency of
six Servo crates in `crates/viewport-shell-servo/Cargo.lock`. What is absent
from that lockfile is `accesskit_unix`, the platform adapter that turns an
AccessKit `TreeUpdate` into AT-SPI, and `accesskit_winit`, which would hang one
off the window. Nothing in this tree converts them either, so the updates are
built and dropped. The embedded backend could be unblocked with bounded work —
take the a11y updates off the embedder API and feed an `accesskit_unix::Adapter`
keyed on the winit window — behind a build that already costs hours. The driven
`servoshell` backend has no lever at all: its only channels to the browser are
its own loopback bridge and `--devtools`, and there is no upstream flag to pass
through `VIEWPORT_SERVOSHELL_ARGS`.

**Which means the default backend is the least accessible of the six**, and
that is the sentence to act on rather than the six paragraphs above it. A desk
that needs a screen reader wants:

```nix
programs.viewport.shellBackend = "webkitgtk";
```

or `--shell-backend=webkitgtk`. This is not a defect in `servoshell` — it is
the default for good reasons measured in
[`benchmarks.md`](benchmarks.md) — but a default chosen on CPU and memory is
not a default chosen on whether the desktop can be read aloud, and until Servo
grows an AT-SPI adapter those two answers are different.

## Building and installing

Each package is named for the engine that draws its shell, which is the only
thing that differs between them:

```
# Servo, in nixpkgs' servoshell; builds no engine at all
nix build .#servoshell      # and this is `.#default`

# the engine in-process; builds WebKit
nix build .#wpe

# the engine out of process; builds no WebKit at all
nix build .#webkitgtk

# no engine built or linked; runs nixpkgs' chromium
nix build .#chromium

# the same engine, embedded; builds a C++ wrapper and no engine
nix build .#cef
```

There is no `.#servo`. The embedded Servo shell is a cargo dependency on the
engine's source, so a package would be a Servo build inside every evaluation
that touched this flake; it is built by hand instead, once, and the section
above says how. That is the whole reason there are two Servo backends.

`.#viewport-smithay` is still an alias, because that is what any existing pin
says — for `.#default`, which means a pin to that name follows the default
rather than the backend that happened to be the only one when it was the only
name. Name a backend to be held to one.

On NixOS:

```nix
programs.viewport = {
  enable = true;
  shellBackend = "servoshell";   # the default; also "cef", "webkitgtk",
                                 # "chromium" or "wpe"
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
