# Debugging

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

`DISPLAY` is exported the moment the X11 socket exists, not when Xwayland
reports itself ready. Starting lazily only defers *running* the server until
something connects; `ready` does not fire until that has happened, so setting
`DISPLAY` there means the first client to launch finds none — it fails, and the
second launch works because by then something else has woken Xwayland.

Override-redirect surfaces that ask for focus are given the keyboard. An X11
menu takes focus and closes when it loses it, so left unfocused it sits there
inert — the menu appears and nothing in it can be clicked. Only surfaces that
ask are given it, since a tooltip must not steal the keyboard, and focus goes
back to the previously focused window when the menu closes. They also follow
their own `set_geometry`, because a submenu moves itself beside its parent
rather than opening where it was first placed.

Xwayland starts lazily: a session that never runs an X11 client pays nothing.

Touchpad gestures are split by finger count: three fingers belong to the
compositor (swipe to scroll the strip or change workspace), everything else is
forwarded to the focused client through `pointer-gestures-v1`, so pinch-to-zoom
in a browser keeps working. A gesture that starts as the compositor's stays
that way until it ends.

Client scale is reported over both `fractional-scale-v1` and the `wl_surface`
preferred buffer scale, taken from the largest scale among the outputs in the
layout — a client renders at whatever it is told and the compositor stretches
whatever it gets, so saying nothing means everything paints at 1x and is scaled
up. Whole-number scales are rounded up, so a client that only understands those
overshoots rather than blurs.

## When the shell breaks

The entire layout lives in a web page, which is the point of this compositor
and also its one structural risk: a JavaScript error or an unreachable shell
means no window is ever placed, and the session becomes a black screen with a
working keyboard.

So placement is watched. A window that maps and is not given a rect within two
and a half seconds is laid out by a deliberately stupid built-in tiler — equal
columns across the output, ignoring workspaces and the tiling tree, since the
thing that maintains them is what stopped responding. It is not a layout anyone
would choose; it exists so a broken shell leaves a desktop usable enough to open
a terminal and fix it. The moment the shell does answer, the watchdog is
disarmed for that window, so a merely slow shell costs nothing.

Not yet implemented: tablet and stylus input.

## Fallback

The load deadline is on the **first painted frame**, not on the load event. A
server that accepts the connection and then stalls, and a shell whose JS throws
before rendering, both leave the user at a black screen and both are invisible
to `load-failed`. On timeout the compositor loads `fallback.html`, which speaks
the same IPC — windows opened while offline are still tiled, listed and
closable.
