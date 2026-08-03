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
restructure the tree; no code here calculates a window position. `solar.js` is
the documented exception and the only one — an orbit is not expressible as rows
and columns, so it positions absolutely. It still writes rectangles as style and
lets the rule below measure the result.

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
| `solar.js` | one window in the middle, the rest in orbit. The only layout arithmetic in the shell; see [docs/solar.md](../../docs/solar.md). |
| `session.js` | Saving and restoring the layout, window rules, notifications. |
| `resize.js` | Dragging the gap between two windows, and resize mode. |
| `geometry.js` | Measuring what the browser laid out and reporting it to the compositor. The hinge of the whole design. |
| `outputs.js` | Outputs, workspaces, and moving between them. |
| `windows.js` | The window lifecycle: added, focused, closed, floated, fullscreened. |
| `bar.js` | The status bar. |
| `screencast.js` | The screen-share chooser. Drawn here, steered from the compositor: the shell receives no input of its own, so the highlight arrives in the message. |
| `commands.js` | Commands from the compositor and the inbound message loop. Loaded last: its bottom asks for the state the shell starts from, so everything handling the answer must already exist. |

`shell.css` styles all of it, and `index.html` is the document.

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
