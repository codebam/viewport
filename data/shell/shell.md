# The reference shell

Tiling, workspaces, and a status bar — the desktop UI, as a web page the
compositor composites underneath every client window.

```
we receive   view.added / view.props / view.removed / view.focused
             output.layout / status.update / shell.command / error
we send      view.layout / view.visible / view.focus / view.close
             view.query / output.query / shell.focus
```

The full message set is in [docs/ipc.md](../../docs/ipc.md).

## Three things worth understanding before changing anything

**Layout is CSS, not arithmetic.** The tiling tree renders to nested flexboxes
and the browser computes every rectangle. Splitting, moving and fullscreen only
restructure the tree; no code here calculates a window position. `solar.js`,
`matrix.js` and `canvas.js` are the documented exceptions and the only ones —
neither an orbit, nor a column that halves at every step, nor a coordinate on
an unbounded plane is expressible as rows and columns, so they position
absolutely. Both still write rectangles as style and let the rule
below measure the result.

**Geometry is measured, never assumed.** A hole's screen rect changes for
reasons no message announces — transitions, font loading, a reflow three
ancestors up — so a ResizeObserver watches each hole and reports what it
actually measures.

**Multi-monitor.** The WebKit view is one canvas spanning the whole output
layout; two 2560x1440 screens are a single 5120x1440 page. Each output gets an
absolutely-positioned desktop and everything happens inside it.

## The files, in load order

These are **ordered classic scripts, not modules.** They share one global
scope: a `let` declared in `state.js` is the same binding everywhere, and any
file may assign to it. That is deliberate. The shell is built around a dozen
pieces of state that most of the code reads and writes — `focusedId`,
`layoutMode`, `overviewActive`, `activeOutput` — and ES modules forbid
assigning to an imported binding, so real modules would mean routing every one
of those through a shared mutable object. The split below was made without
changing a single line of behaviour; that conversion would not be.

The consequence to remember: **order matters, and it is defined in
`index.html`.** `tests/shell.test.js` reads the `<script>` tags out of that file
rather than keeping its own list, so the two cannot drift apart.

| File | What is in it |
| --- | --- |
| `state.js` | The bridge to the compositor, and every piece of shell state. Loaded first because the rest is declarations and these are the values they act on. |
| `motion.js` | The animations that cannot be written as CSS — an element leaving, a stagger, an opacity that is an IPC message. Nothing else in the shell starts a tween. |
| `tiling.js` | The i3-style tree, and rendering it to nested flexboxes. |
| `scrolling.js` | niri's endless strip of columns, and the overview. |
| `solar.js` | one window in the middle, the rest in orbit. Layout arithmetic rather than flexbox; see [docs/solar.md](../../docs/solar.md). |
| `matrix.js` | the focused window large, the focus history halving away beside it. The other layout that computes rectangles; see [docs/matrix.md](../../docs/matrix.md). |
| `canvas.js` | an unbounded plane per workspace, panned and zoomed over. The third layout that computes rectangles; see [docs/canvas.md](../../docs/canvas.md). |
| `session.js` | Saving and restoring the layout, window rules, notifications. |
| `resize.js` | Dragging the gap between two windows, and resize mode. |
| `geometry.js` | Measuring what the browser laid out and reporting it to the compositor. The hinge of the whole design. |
| `outputs.js` | Outputs, workspaces, and moving between them. |
| `windows.js` | The window lifecycle: added, focused, closed, floated, fullscreened. |
| `bar.js` | The status bar, its widgets, the system tray and its menus. |
| `clipboard.js` | The clipboard history picker. |
| `launcher.js` | The launcher, opened from `Mod4+d`. The list is the compositor's — the page cannot read `XDG_DATA_DIRS` — so this draws the rows it is sent and sends the filter back on every keystroke; launching names an `id`, and the compositor starts what it scanned with an activation token. Like the network picker's passphrase box, the filter field is real typed text: it asks for the keyboard with `shell.focus` and gives it back. |
| `notifications.js` | The notification centre, opened from `Mod4+Shift+m`: what has been notified, after the popups have gone. The list itself is the compositor's — this draws it and sends back forget-one and forget-all. The popups are `session.js`, which is the same notifications and a different job. |
| `power.js` | The power-profile picker, opened from the battery widget. |
| `network.js` | The Wi-Fi and Bluetooth pickers, opened from `Mod4+Shift+n`, `Mod4+Shift+t` and the bar's network module. Clicked rather than steered, like the clipboard picker — and the one place in the shell that receives real typed text, because a passphrase field does: it asks for the keyboard with `shell.focus` and gives it back. |
| `screencast.js` | The screen-share chooser, the remote-control one, and the shortcut one. Drawn here, steered from the compositor: the shell receives no input of its own, so the highlight arrives in the message. A `screencast.pick` carrying `devices` is the RemoteDesktop portal rather than ScreenCast, and the dialog asks that question instead. A `shortcuts.pick` is the third question — which keys an application may hear while something else has focus — and has no highlight at all, because the answer is yes or no to the whole list. |
| `settings.js` | The settings panel, opened from `Mod4+Shift+comma`: dark mode, the wallpaper, the gaps, the window border and the monitors. A dialog rather than a dropdown, because it is five sections and one of them is a row per monitor with a mode list in it. Every switch sends a runtime setter and takes effect at once; Save sends `config.save`, which is what makes a change survive a restart. A display change raises a Keep-or-Revert bar, because the compositor puts the monitors back unless somebody says they can see them — see [docs/ipc.md](../../docs/ipc.md). Like the launcher, its fields are real typed text and it asks for the keyboard with `shell.focus`. |
| `osk.js` | The on-screen keyboard. Docked to the bottom of an output rather than centred like the pickers above it, and the one part of the shell whose taps are not clicks on the DOM but instructions to the compositor: every key sends an `osk.key` keysym, pressed and released like a real one, and the seat's own keyboard repeat does the rest. Comes up on its own when the focused client's text-input is enabled (`osk.wanted`), or by hand with `Mod4+Shift+k`. |
| `commands.js` | Commands from the compositor and the inbound message loop. Loaded last: its bottom asks for the state the shell starts from, so everything handling the answer must already exist. |

`shell.css` styles all of it, and `index.html` is the document.

Editing any of them takes effect without restarting the compositor:
`Mod4+Shift+c` reloads the page, and starting the compositor with
`--watch-shell` reloads it whenever a file here is saved. A reload resets shell
state — windows come back through the `view.query` replay, workspace
assignments do not. See [docs/configuration.md](../../docs/configuration.md).

`vendor/` holds built dependencies, checked in rather than fetched: this is a
`file://` page with no bundler and no network, and the packaging installs the
directory with `cp -r`. Today that is GSAP, which `motion.js` uses as a tween
engine and nothing else. The version is declared in `package.json` at the
repository root and refreshed with `npm install && npm run vendor`; its licence
is not this repository's, and `vendor/README.md` says so.

## Motion

Most of what moves here is the stylesheet, and it should stay that way: a
window sliding between two layouts is `transition: flex-grow`, and the browser
does it on the compositor's behalf for nothing. `motion.js` holds the
remainder — an element being *removed*, which no rule can style; a stagger,
which in CSS is a delay per child written against a count the markup owns; and
a window's own opacity, which is not a style at all but a message, because the
contents of a window are a surface the compositor draws.

Two rules apply to anything animated, either way:

**Nothing measured may be transformed.** Window frames, `.viewport` holes,
overview thumbnails, the strip and anything handed to `setOverlay` are read
back with `getBoundingClientRect` and sent to the compositor. A transform on
one of those, or on any ancestor of one, is not an effect — it is a stream of
new geometry for every client on the screen, for as long as it runs. Where
something measured needs an entrance, it fades.

**Nothing repeats forever.** Every frame the shell paints is a composited frame
for the whole desktop, so an idle loop is a permanent cost paid by a machine
that is doing nothing.
