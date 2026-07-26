# Viewport

A Wayland compositor whose entire shell — wallpaper, dock, window frames,
titlebars — is a web page, composited zero-copy alongside native Wayland
clients.

wlroots handles DRM/KMS, input and the `xdg-shell` protocol. WPE WebKit renders
the UI to a DMA-BUF. Neither ever hands a pixel to the CPU.

```
        ┌──────────────────────────────────────────┐
        │  shell (HTML/CSS/JS, loaded from a URL)  │
        └───────────────┬──────────────────────────┘
                        │  WPEBufferDMABuf     ▲ JSON over
                        ▼  (render_buffer)     │ script handler
        ┌──────────────────────────────────────┴───┐
        │  viewport (C)                            │
        │    wlr_scene   layer_web   ← shell       │
        │                layer_apps  ← xdg clients │
        └───────────────┬──────────────────────────┘
                        ▼  wlr_scene_output_commit
                     DRM/KMS
```

## How it fits together

**The shell is the bottom layer.** A single `wlr_scene_buffer` covering the
whole output layout, fed by WebKit's DMA-BUF. Client windows are stacked above
it in `layer_apps`.

**Window placement is JavaScript's job.** The shell draws a frame with a hole in
it — a `<div class="viewport">` — measures where that hole landed on screen, and
sends the rect over IPC. The compositor moves the client's scene node there.
There is no layout policy in the C code at all.

**Tiling is a tree of flexboxes.** Split containers hold leaves, i3-style, and
the tree renders to nested CSS flex containers — so the browser computes every
rectangle and the shell only measures the result. Splitting, moving, resizing
and fullscreen restructure the tree or adjust weights; no code calculates a
window position. The gap between windows is a real element, which is why
dragging a window edge needs no compositor support at all.

**Hit-testing falls out of the layering.** `wlr_scene_node_at()` returns a client
surface when the pointer is over a window and the shell's buffer everywhere
else, so "click went to the titlebar" versus "click went to the app" needs no
geometry bookkeeping and cannot go stale mid-animation.

**Frame pacing is real vblank.** WebKit will not paint frame N+1 until we
acknowledge frame N; we acknowledge from the output's frame handler.

**Explicit sync throughout.** WebKit attaches a rendering fence to each frame;
we import it into a `drm_syncobj` timeline and let `wlr_scene` wait on that
point rather than blocking the compositor. Wayland clients get
`zwp_linux_drm_syncobj_v1`.

## Why WPEPlatform and not libwpe

nixpkgs ships `libwpe` and `libwpe-fdo`, and neither is a browser engine —
`libWPEBackend-fdo-1.0.so` contains zero WebKit symbols. They are the backend
ABI and one implementation of it. The engine, `libWPEWebKit`, is not packaged in
nixpkgs at all, and the `webkitgtk` tarball cannot be rebuilt with `-DPORT=WPE`
because it ships the GTK port only. So the flake builds WPE WebKit from the
separate upstream `wpewebkit-2.52.5.tar.xz`.

Since we compile the engine either way, we build it with
`-DENABLE_WPE_PLATFORM=ON`. That drops `libwpe` and `libwpe-fdo` entirely and
replaces the old `wpe_view_backend` indirection with GObject subclassing:
`WPEDisplay` advertises our DRM device and formats, `WPEView::render_buffer`
delivers each frame as a `WPEBufferDMABuf` whose fds, offsets, strides and
modifier map field-for-field onto `wlr_dmabuf_attributes`.

The legacy path would have meant standing up our own EGL display on a `gbm`
device and pulling fds out with `EGL_MESA_image_dma_buf_export` — several
hundred lines of glue to arrive at the same dma-buf.

## Build

```sh
nix develop          # meson, ninja, wlroots 0.20, WPE WebKit, test clients
meson setup build
ninja -C build
```

The first `nix develop` compiles WPE WebKit from source. It is a full WebKit
build: expect hours and tens of gigabytes. There is no binary cache for it.

```sh
nix build .#wpewebkit   # do this once, deliberately, before anything else
```

Run nested inside an existing compositor:

```sh
./build/viewport --url http://localhost:3000 --startup foot
```

On a TTY it takes the DRM session directly (needs `seatd` or logind).

## Configuration

Two tiers, because a settings UI cannot run on a display that is not working
yet.

**Bootstrap** — `~/.config/viewport/config.json` (or `--config PATH`), read
before any web content loads. This tier must stay in C. The shell is fetched
over the network; if it 404s, throws, or hangs, anything it owned dies with it.
A keybinding defined here still works in that state, which is the difference
between a broken desktop UI and a machine you cannot quit without switching to
a TTY.

```jsonc
{
  "url": "http://localhost:3000",
  "timeout_ms": 5000,
  "layout": "tiling",       // or "scrolling"
  "terminal": "ghostty",
  "menu": "wmenu-run -i",
  "binds": {
    "Mod4+Return":  "exec ghostty",
    "Mod4+d":       "exec wmenu-run -i",
    "Mod4+Shift+q": "close",
    "Mod4+Shift+e": "exit",
    "Mod4+Shift+c": "reload"
  }
}
```

Actions are `exec COMMAND`, `close`, `exit`, `reload`, `focus DIRECTION`,
`mode NAME`, `appearance toggle` and `shell COMMAND ARGS…`. Chords use sway's
spelling — `Mod4`/`Super`/`Logo`, `Shift`, `Ctrl`, `Alt` — and any key
`xkb_keysym_from_name` accepts, including `XF86AudioRaiseVolume`. Caps and num
lock are masked out of matching. Bindings outrank both the focused client and
the shell, and fire on press only.

`focus` takes `next`, `prev`, `left`, `right`, `up` or `down`. Directional
moves compare window centres, so they follow what is on screen — including
across monitors — rather than stacking order.

`shell` forwards the rest of the line to the web shell as
`{"type":"shell.command","command":…,"args":[…]}` and does nothing else. This
is the seam for everything that is layout policy: the defaults bind
`Mod4+1..9` to `shell workspace.switch N` and `Mod4+Shift+1..9` to
`shell workspace.move N`, and *what a workspace is* is defined entirely in
`data/shell/shell.js`. Add your own commands by binding them and handling the
name in `handleShellCommand()`; no compositor change is needed.

A chord may be scoped to a mode by writing `mode/chord`, mirroring sway's
`mode "resize"` blocks — `resize/h` only fires while resize mode is active, so
`h` keeps meaning "focus left" everywhere else. `mode NAME` switches; the bar
shows the active mode.

Only a few things live in C on purpose: focus and pointer grabs, because they
need the seat; spawning and exit, because they must work when the shell is
broken; `/proc` sampling and the settings portal, because a web page cannot do
either. Everything else — tiling, workspaces, fullscreen, resizing, the bar —
is the shell's.

### Default bindings

| Chord | Action |
| --- | --- |
| `Mod4+Return` / `Mod4+d` | terminal / launcher |
| `Mod4+h j k l`, arrows | focus, crossing monitors at the edge |
| `Mod4+Shift+h j k l` | move window, carrying it to the next monitor at the edge |
| `Mod4+1‑9` / `+Shift` | switch / move to workspace |
| `Mod4+b` / `Mod4+v` | next window splits horizontally / vertically |
| `Mod4+e` | flip the focused container's layout |
| `Mod4+f` | fullscreen |
| `Mod4+r` | resize mode; then `hjkl`, `Escape` to leave |
| `Mod4` + right-drag | resize; dragging the gap between windows also works |
| `Mod4+n` | toggle the bar |
| `Mod4+Shift+d` | toggle dark mode |
| `Mod4+Shift+q` / `+e` / `+c` | close / exit / reload the shell |

## Dark mode

Styling the shell cannot make client applications dark. Firefox, GTK and Qt
each ask the same question — `color-scheme` in the `org.freedesktop.appearance`
namespace, over D-Bus through xdg-desktop-portal — and with nothing answering
they all default to light.

That answer normally comes from a desktop environment. There isn't one here, so
the compositor implements `org.freedesktop.impl.portal.Settings` itself. The
GSettings route is deliberately avoided: it needs dconf and GNOME's schemas
installed, and silently does nothing when they are absent.

Three things must line up, and `start.sh` handles all of them — running from a
build tree gets none of them for free:

| | |
| --- | --- |
| `XDG_CURRENT_DESKTOP=viewport` | the portal picks a backend by matching this against each `.portal` file's `UseIn=` |
| `XDG_DATA_DIRS` | it only finds `viewport.portal` if that directory is on the search path |
| `dbus-update-activation-environment` | the portal is D-Bus activated and inherits its environment from the session, not from the compositor |

Setting `XDG_CURRENT_DESKTOP` with `setenv()` in the compositor is not enough:
the portal is a separate, already-running process. Verify with:

```sh
gdbus call --session --dest org.freedesktop.portal.Desktop \
  --object-path /org/freedesktop/portal/desktop \
  --method org.freedesktop.portal.Settings.ReadOne \
  org.freedesktop.appearance color-scheme     # (<uint32 1>,) means dark
```

Defining *any* `binds` object replaces the built-in defaults, so an empty one
means "no keybindings". Include an exit binding if you override them.
`data/config.example.json` is a fuller starting point.

Precedence is flags > config file > defaults.

```
-u, --url URL          shell endpoint (default http://localhost:3000)
-f, --fallback URL     used when the shell fails (default: bundled fallback.html)
-t, --timeout MS       first-paint deadline before falling back (default 5000)
-s, --socket PATH      control socket
-c, --config PATH      config file (default ~/.config/viewport/config.json)
-T, --terminal CMD     command bound to Mod4+Return
-M, --menu CMD         command bound to Mod4+d
-b, --bind CHORD=ACT   add a keybinding; repeatable
-e, --startup CMD      command to run once up
-H, --headless         headless backend instead of DRM
-d, --debug            verbose logging, and mirror the shell's console
```

`--debug` also disables WebKit's cache and, for a `file://` shell, watches its
directory and reloads on change — so editing `shell.js` updates the running
desktop without restarting the compositor. Saves are debounced, since editors
write-then-rename and emit several events per save. A shell served over HTTP is
left alone: that is a dev server's job, and watching it would mean polling.

Reloading resets shell state, so windows return via the `view.added` replay but
workspace assignments do not survive.

**Runtime** — the shell renders display settings in HTML and drives the
compositor over the same JSON channel it uses for window layout, and may
register further bindings with `bind.add`. That layer is additive and
expendable by design: keep `exit` and a terminal in the config file so they
survive the shell being unreachable. Everything else — wallpaper, dock
contents, theming — is pure shell state and never reaches the C side.

On NixOS the flake's module renders all of this for you:

```nix
programs.viewport = {
  enable = true;
  url = "http://localhost:3000";
  terminal = "${pkgs.ghostty}/bin/ghostty";
  menu = "${pkgs.wmenu}/bin/wmenu-run -i";
  binds."Mod4+Shift+e" = "exit";
};
```

## IPC

Two transports, one dispatch table.

The page uses the script message handler, which works regardless of the shell's
origin and is unaffected by CORS or mixed-content rules:

```js
window.webkit.messageHandlers.viewport.postMessage(JSON.stringify(msg));
window.addEventListener('viewport', e => handle(e.detail));
```

External tooling uses a UNIX socket of newline-delimited JSON:

```sh
socat - UNIX:$VIEWPORT_SOCKET
```

### Compositor → shell

| Message | Payload |
| --- | --- |
| `config` | `layout` (`"tiling"` or `"scrolling"`) |
| `view.added` | `id`, `title`, `app_id`, `output` (name of the output it opened on), `floating`, `width`, `height`, `min_width`, `min_height` |
| `view.props` | `id`, `title`, `app_id` |
| `view.removed` | `id` |
| `output.layout` | `outputs[]` with `name`, `make`, `model`, `serial`, `enabled`, `x`, `y`, `width`, `height`, `usable_x`, `usable_y`, `usable_width`, `usable_height`, `scale`, `transform`, `modes[]` |
| `error` | `context`, `message` |

### Shell → compositor

| Message | Payload |
| --- | --- |
| `view.layout` | `id`, `x`, `y`, `width`, `height`, optional `clip{x,y,width,height}` |
| `view.visible` | `id`, `visible` |
| `view.focus` | `id` |
| `view.close` | `id` |
| `view.query` | — |
| `shell.focus` | — |
| `bind.add` | `chord`, `action` |
| `output.configure` | `name`, `enabled`, `mode{width,height,refresh}`, `x`, `y`, `scale`, `transform`, `adaptive_sync` |
| `output.query` | — |
| `output.confirm` | — |
| `quit` | — |

`output.configure` runs `wlr_output_test_state` before committing, so a mode the
hardware cannot drive is reported back as an `error` instead of blanking the
screen you are configuring from. A configuration that *does* commit is still
provisional: it reverts after twelve seconds unless an `output.confirm` arrives,
because a wrong mode blanks the very screen you would need in order to undo it.

A window is a real Wayland surface, so nothing the shell draws can crop it —
CSS `overflow` bounds the shell's own painting and no more. `clip` on
`view.layout` is how a window is cropped to the part of it that is on its
output, which is what keeps a column scrolled off the left of one monitor from
being drawn on the monitor beside it. Only the surface is clipped, never the
container: a popup is entitled to extend past the window it belongs to.

### Testing the shell

The layout engine lives in `shell.js`, and running it under a headless
compositor proves nothing: the web view renders, but nothing drives the layout,
so a broken tree looks exactly like a working one. `tests/shell.test.js` stubs
the DOM far enough to run the real file unmodified and checks structure — four
windows make four columns, consume and expel are inverses, a tabbed container
shows exactly one window and it is the focused one.

```sh
timeout 20 node tests/shell.test.js data/shell/shell.js tiling
timeout 20 node tests/shell.test.js data/shell/shell.js scrolling
```

`timeout` because the shell sets a live-reload interval and so never exits.

## Layout models

`"layout"` in the config file picks which one the shell runs.

**`tiling`** — i3 and sway. Windows split the space they are given; containers
can be `split`, `tabbed` or `stacked`. `Mod4+w` and `Mod4+s` set the last two,
`Mod4+e` returns to a split. Tabs are the one place this shell draws a window
title, because a collapsed tab cannot be identified without one.

**`scrolling`** — niri. A workspace is an endless horizontal strip of columns;
each column holds one or more windows stacked vertically, and columns keep the
width they were given, so opening a window never reflows what is already there.
The view scrolls the minimum needed to keep the focused column on screen.

| Key | Scrolling layout |
| --- | --- |
| `Mod4+h` / `Mod4+l` | focus the column left / right |
| `Mod4+j` / `Mod4+k` | focus within the column |
| `Mod4+Shift+h/l` | move the column along the strip, or to the next monitor at its end |
| `Mod4+comma` / `Mod4+period` | consume the next window into this column / expel it back out |
| `Mod4+r` | cycle the column width (⅓, ½, ⅔, full) |
| `Mod4+Shift+r` | cycle the window's share of the column height |
| `Mod4+Home` / `Mod4+End` | jump to either end of the strip |

Directional focus moves to the shell in this mode: the compositor decides
direction from where windows are on screen, and the column you are reaching for
is usually scrolled off it. Both models share one tree — the strip's columns are
the workspace root's children — so switching `layout` and reloading rearranges
what is open rather than discarding it.

## Writing a shell

Style the frame however you like; leave the hole alone.

```css
.viewport { background: transparent; }  /* the compositor paints here */
```

```js
const rect = viewportEl.getBoundingClientRect();
send({ type: 'view.layout', id,
       x: Math.round(rect.left),  y: Math.round(rect.top),
       width: Math.round(rect.width), height: Math.round(rect.height) });
```

Measure with a `ResizeObserver` rather than pushing rects when you think
something moved — CSS transitions, font loading and ancestor reflows all change
that rect without firing anything you can subscribe to. `data/shell/` is a
working reference.

## Fallback

The load deadline is on the **first painted frame**, not on the load event. A
server that accepts the connection and then stalls, and a shell whose JS throws
before rendering, both leave the user at a black screen and both are invisible
to `load-failed`. On timeout the compositor loads `fallback.html`, which speaks
the same IPC — windows opened while offline are still tiled, listed and
closable.

## Status

The architecture is grounded in the real 2.52.5 headers: the `WPEDisplay` and
`WPEView` vfunc tables, `wpe_buffer_dma_buf_get_*`, `WebKitWebView`'s `display`
construct property, and wlroots 0.20's `wlr_buffer_impl` and
`wlr_scene_buffer_set_buffer_with_options`.

**Built and running on real hardware** — DRM/KMS on an RX 7900 XTX (radv, Mesa
26.1.5), dual 2560x1440, launched from a TTY via `./start.sh`. Confirmed
working:

- the web shell renders as the desktop; `xdg-shell` clients composite inside
  the frames it draws, one desktop per output
- keybindings: terminal, launcher, close, exit, shell reload
- `wmenu` and other layer-shell launchers, including keyboard focus
- server-side decorations, so clients drop their own titlebars
- VT switching away and back, with the outputs repainting on resume
- `grim` screenshots from inside the session
- clean shutdown: ~1s, no assertions

Earlier, nested inside sway:

- Vulkan renderer initialises; GBM allocator binds `/dev/dri/renderD128`
- `WPEDisplay` hands WebKit that same render node, and WebKit's frames arrive
  as `WPEBufferDMABuf` and composite through `wlr_scene` — the shell's HTML is
  visibly rendered as the desktop
- an `xdg-shell` client (`foot`) maps, is reported over IPC as
  `{"type":"view.added","id":1,"title":"foot","app_id":"foot"}`, and is
  composited inside the frame the shell drew for it
- the UNIX control socket serves `output.layout` with real mode data

Also verified: `wlr_scene` reports `Direct scan-out enabled` for the shell
buffer when no client overlays it, and keybindings parse and reject bad input
with precise errors.

### Protocols

Beyond `xdg-shell` and `linux-dmabuf`, several protocols turned out to be
load-bearing rather than optional, each for a non-obvious reason:

| Protocol | Why |
| --- | --- |
| `wlr-layer-shell` | every Wayland launcher is a layer-shell client |
| `xdg-activation` | `wmenu-run` *asserts* on its absence and aborts before drawing |
| `xdg-decoration` + KDE `server-decoration` | both are needed: GTK4 ignores the former and honours the latter, so a GTK client keeps its own titlebar unless you offer both |
| `ext-image-copy-capture` + `wlr-screencopy` | screenshots from inside the session — the only way to see the desktop once it owns the display |
| `virtual-keyboard` | `wtype`, accessibility tools, and injecting keystrokes in tests |
| `primary-selection` | middle-click paste |

## Debugging from inside

Both capture protocols are implemented — `ext-image-copy-capture-v1` and
`zwlr-screencopy-v1` — so `grim` works as an ordinary client:

```sh
grim ~/shot.png          # from a terminal inside the session
```

This is load-bearing rather than a nicety. Once the compositor is driving a TTY
there is no outer compositor to screenshot it from, so without this there is no
way to see what the desktop looks like, and debugging a *visual* shell becomes
guesswork.

Pair it with `--debug`, which mirrors the page console into the compositor log
and disables WebKit's cache:

```
CONSOLE INFO  outputs [object Object]
CONSOLE ERROR TypeError: ...        # a shell exception, otherwise silent
```

### Pointer capture

Games need two things a desktop pointer does not provide, and they only work
together:

| Protocol | Why |
| --- | --- |
| `zwp_relative_pointer_v1` | mouselook is driven by how far the mouse moved, not where the cursor landed — an absolute position saturates at the screen edge, so a game without it can only turn so far |
| `zwp_pointer_constraints_v1` | the cursor must stop moving, so it neither escapes onto the other monitor mid-fight nor feeds the client absolute motion it is no longer expecting |

While a lock is active the compositor stops moving its cursor entirely and the
client is driven purely by deltas. Unaccelerated values are forwarded
separately so a game can apply its own sensitivity. A constraint belongs to a
surface, so it lapses when that surface loses the pointer — otherwise a game
would keep the mouse captured after you focused something else.

This covers X11 games too. `XGrabPointer` has no direct equivalent on Wayland;
Xwayland implements it by taking out these same two protocols on the client's
behalf, so a native game and CS2 under Proton reach identical code.

### XWayland

X11 clients are tiled like any other window. The difference is confined to
`src/view.c`: an X11 window is not an `xdg_toplevel`, so title, class, sizing,
activation and closing go through accessors, and everything above — tiling,
workspaces, focus, the IPC view model — is unaware of which kind it has.

Three things about X11 shape that file:

- a surface exists before it has a `wl_surface`, so mapping is two-stage and
  the scene tree is built on `associate` rather than at creation;
- position and size are a single operation in absolute screen coordinates, so
  placement is repeated on every resize;
- override-redirect windows — X11 menus and tooltips — bypass the window
  manager entirely. They are never tiled and appear exactly where the client
  asks, on the overlay layer. Tiling them would break every X11 menu.

Xwayland starts lazily: a session that never runs an X11 client pays nothing.

Still unimplemented: tablet input, and per-window opacity rules.

Not yet implemented: tablet and stylus input.

## Licence

MIT, matching wlroots. WPE WebKit and GLib are LGPL-2.1+ and dynamically
linked, which imposes no licence condition on this code.
