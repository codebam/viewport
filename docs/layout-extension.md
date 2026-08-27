# User layout extensions

The shipped shell can load explicit local layout scripts without edits to
`data/shell/index.html` or compositor allowlists. Declare each script under the
name it registers, then select that name as `layout`:

```json
{
  "layout": "monocle",
  "layout_extensions": {
    "monocle": "layouts/monocle.js"
  }
}
```

Paths are local files. Relative paths resolve beside the config file; `~/` and
absolute paths also work. The compositor validates the manifest, resolves each
path to a `file://` URL, and accepts `layout` when it is either built in or
present in the manifest. Remote URLs are rejected. Existing paths are
canonicalised; a missing path reaches the loader so it can fall back without
aborting the compositor.
Extension scripts load in name order before
session state and mapped windows are replayed.
Changing the manifest or a script takes effect on the normal shell reload; a
removed entry becomes unavailable to layout selection immediately.

A script registers exactly one descriptor:

```js
registerLayout('monocle', {
  render({ root, views, focusedId, helpers }) {
    if (!root) return null;
    const ids = helpers.dynamicOrder(root).filter(
      (id) => views.has(id) && !helpers.isFloating(id));
    const id = ids.includes(focusedId) ? focusedId : ids[0];
    const view = views.get(id);
    if (!view) return null;
    helpers.markRendered(id);
    return view.el;
  },

  clear({ views }) {
    for (const view of views.values()) {
      view.el.classList.remove('my-layout-state');
    }
  },
});
```

`render(context)` runs once per output and returns one element to append under
that output's `.windows`, or `null`. It must call `helpers.markRendered(id)` for
every window it places. Windows not marked are hidden and reported invisible.
Floating windows are appended by the shell after the returned layout element.

`clear(context)` runs while another layout or overview is active. It must undo
classes, inline geometry, hidden state, opacity, and any per-view state the
extension added. It may be called repeatedly and must therefore be idempotent.

The context surface is:

- `output`: current output record, including `name`, `workspace`, `windowsEl`
- `root`: current workspace's shared tiling tree, or `null`
- `views`, `workspaces`, `outputs`: live maps owned by the shell
- `focusedId`, `workspace`, `renderedIds`: current layout state
- `plan`: reserved for built-in multi-output layouts; extensions receive `null`
- `helpers.renderTree(root)`, `helpers.renderStrip(root, output)`
- `helpers.idsOf(workspace)`, `helpers.dynamicOrder(root)`
- `helpers.isFloating(id)`, `helpers.isFullscreen(id)`
- `helpers.edgeGapPx(workspace)`, `helpers.measureOf(element)`
- `helpers.markRendered(id)`

`clear` receives the same maps and helpers, with `output`, `root`, and
`workspace` unset because it clears the layout across all outputs at once.

Read the tree unless the extension deliberately implements tree editing. The
overview, session restore, movement commands, scratchpads, and pinned windows
all rely on that shared model.

Names may contain ASCII letters, digits, `_`, and `-`. A registration must
provide both functions. Duplicate names and attempts to replace a built-in are
rejected. A script load error, rejected registration, missing expected
registration, or selected name not present falls back to `tiling` and logs the
reason; window replay still proceeds.

`data/layout-extensions/monocle.js` is a complete example. Extension-specific
CSS may be injected by the script or supplied through the config theme; no CSS
file is loaded implicitly.
