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
restructure the tree; no code here calculates a window position.

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
| `tiling.js` | The i3-style tree, and rendering it to nested flexboxes. |
| `scrolling.js` | niri's endless strip of columns, and the overview. |
| `session.js` | Saving and restoring the layout, window rules, notifications. |
| `resize.js` | Dragging the gap between two windows, and resize mode. |
| `geometry.js` | Measuring what the browser laid out and reporting it to the compositor. The hinge of the whole design. |
| `outputs.js` | Outputs, workspaces, and moving between them. |
| `windows.js` | The window lifecycle: added, focused, closed, floated, fullscreened. |
| `bar.js` | The status bar. |
| `commands.js` | Commands from the compositor and the inbound message loop. Loaded last: its bottom asks for the state the shell starts from, so everything handling the answer must already exist. |

`shell.css` styles all of it, and `index.html` is the document.
