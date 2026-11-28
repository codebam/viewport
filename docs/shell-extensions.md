# User shell extensions & live hot-reload

## On this page

- [Directory layout and manifest](#directory-layout-and-manifest)
- [Slots](#slots)
- [Configuration](#configuration)
- [Extension JavaScript API](#extension-javascript-api)
- [Live hot-reload](#live-hot-reload)
- [Error isolation and security](#error-isolation-and-security)
- [Example](#example)

Because Viewport's desktop shell is a web page, extending the desktop does not
require writing Rust or recompiling the compositor. Drop a folder with a JSON
manifest, a JavaScript file, and an optional stylesheet into
`$XDG_CONFIG_HOME/viewport/extensions/<id>/` (default:
`~/.config/viewport/extensions/<id>/`), and Viewport discovers it, validates its
paths, mounts it into the requested shell slot, and live hot-reloads it on save
without restarting the compositor or losing any open windows.

## Directory layout and manifest

Each extension lives in its own subdirectory under an extensions root and
declares an `extension.json` manifest:

```text
~/.config/viewport/extensions/
└── workspace-pill/
    ├── extension.json
    ├── index.js
    └── style.css
```

Example `extension.json`:

```json
{
  "id": "workspace-pill",
  "name": "Workspace Pill",
  "version": "1.0.0",
  "description": "Shows the active workspace and open window count",
  "slot": "bar-center",
  "script": "index.js",
  "style": "style.css",
  "enabled": true,
  "options": {
    "prefix": "WS"
  }
}
```

| Field | Required | Meaning |
| --- | --- | --- |
| `id` | yes | Unique ASCII identifier (`[A-Za-z0-9][A-Za-z0-9_-]{0,63}`). |
| `slot` | yes | Target mount slot: `bar-left`, `bar-center`, `bar-right`, `desktop`, or `overlay`. |
| `script` | yes | Extension-relative path to the JavaScript entry point (e.g. `"index.js"`). |
| `style` | no | Extension-relative path to an optional CSS stylesheet (e.g. `"style.css"`). |
| `name` | no | Display name (defaults to `id`). |
| `enabled` | no | Boolean toggle (defaults to `true`). |
| `version` | no | Free-form version string for metadata/inspection. |
| `description` | no | Short human-readable summary. |
| `options` | no | Arbitrary JSON object passed as a frozen read-only `ctx.options`. |

## Slots

| Slot | Scope | Placement |
| --- | --- | --- |
| `bar-left` | Per output | Inside `.bar-left`, beside workspaces and taskbar. |
| `bar-center` | Per output | Centered in `.bar-center` (collapses when empty). |
| `bar-right` | Per output | Inside `.bar-end`, beside the right-side status modules (preserved even when `bar_items` overrides `.bar-right`). |
| `desktop` | Per output | Full-output layer inside `.desktop` above the wallpaper and below client windows (`pointer-events: none` on the slot container; mounted children receive pointer events). |
| `overlay` | Page-level | `#extensions-overlay`, lifted above client windows via `shell.overlay` rectangles and automatically hidden while the lock screen is active. |

## Configuration

Control discovery, enable/disable policy, and live hot-reload in
`~/.config/viewport/config.json`:

```jsonc
{
  "extensions": {
    "enabled": true,
    "hot_reload": true,
    "dirs": ["~/.config/viewport/extensions"],
    "enable": ["experimental-widget"],
    "disable": ["noisy-pill"]
  }
}
```

- `enabled` (default `true`): master switch for all user shell extensions.
- `hot_reload` (default `true`): watch extension directories via Linux `inotify`
  and push `extensions.changed` events whenever files are created, modified, or
  removed.
- `dirs` (optional): list of local directories to scan. Relative paths resolve
  beside `config.json`; `~/` expands against `$HOME`. When omitted, defaults to
  `$XDG_CONFIG_HOME/viewport/extensions` (or `~/.config/viewport/extensions`).
- `enable` / `disable` (optional): lists of extension IDs. Precedence is:
  `extensions.enabled == false` (all off) → `disable` (always off) → `enable`
  (turns on even if `extension.json` has `"enabled": false`) → manifest
  `enabled` (default `true`).

Runtime IPC commands (via `viewport msg` or the control socket):

```sh
viewport msg -t extensions.query --pretty
viewport msg -t extensions.reload
viewport msg -t extensions.reload --id workspace-pill
viewport msg -t config.extensions --hot_reload false
viewport msg -t config.extensions --disable workspace-pill
```

## Extension JavaScript API

An extension script registers itself through `window.viewportExtensions`:

```js
window.viewportExtensions.register('workspace-pill', {
  mount(container, ctx) {
    const el = document.createElement('span');
    el.className = 'vp-ext-workspace-pill';
    container.append(el);

    const unsubscribe = ctx.subscribe((state) => {
      el.textContent = `WS ${state.activeWorkspace} · ${state.windows.length} windows`;
    });

    // Returning a cleanup function is optional; subscriptions created through
    // ctx.subscribe() are also auto-cleaned on unmount.
    return () => unsubscribe();
  },

  unmount(container, ctx) {
    container.replaceChildren();
  },
});
```

### Context (`ctx`)

Every `mount(container, ctx)` and `unmount(container, ctx)` receives a frozen
`ctx` object:

- `ctx.id`, `ctx.name`, `ctx.slot`, `ctx.revision`, `ctx.output` (`null` for `overlay`).
- `ctx.options`: deeply frozen copy of the manifest's `options` object (`{}` when absent).
- `ctx.getState()`: returns a deeply frozen read-only snapshot of live shell state:
  - `activeOutput`, `activeWorkspace`, `focusedWindowId`, `mode`, `gameMode`
  - `outputs[]`: `{ name, workspace, active, width, height, hdr }`
  - `workspaces[]`: `{ id, name, output, active, windowCount }`
  - `windows[]`: `{ id, title, app_id, workspace, focused, floating, minimized, fullscreen, maximized }`
  - `clock`: `{ text, locale, hour12, format }`
  - `status`: `{ cpu, memory, load, net_rx, net_tx, disk_free, disk_total, volume, muted, mic_volume, mic_muted, brightness }`
  - `power`: latest `power.update` snapshot (or `null`)
  - `mpris`: latest `mpris.update` player snapshot (or `null`)
- `ctx.subscribe(listener)`: immediately invokes `listener(state)` and re-invokes
  it whenever shell state updates. Returns an `unsubscribe()` function. All
  subscriptions are automatically cleaned up on unmount.
- `ctx.onCleanup(fn)`: registers an additional teardown callback executed on unmount.
- `ctx.setOverlayOptions({ passthrough, blur, keepFocus })`: configures how an
  `overlay` slot extension is composited over windows.
- `ctx.log(...)`, `ctx.warn(...)`, `ctx.error(...)`: prefixed console helpers.

### Global helper methods

- `window.viewportExtensions.list()`: frozen array of active extensions and their
  mount status (`loading`, `mounted`, or `error` with `error` message).
- `window.viewportExtensions.getState()`: frozen read-only shell state snapshot.
- `window.viewportExtensions.reload(id?)`: triggers an `extensions.reload` request.

## Live hot-reload

When `extensions.hot_reload` is enabled (the default):

1. The compositor watches each configured extension root directory (and its
   subdirectories up to 4 levels deep, plus the parent directory if the root
   does not exist yet) with Linux `inotify` and debounces bursts by 200 ms.
2. Editor temp/swap files (`.swp`, `*~`, dotfiles) are ignored.
3. On settle, the compositor re-discovers extensions, computes a deterministic
   content fingerprint revision per extension, and broadcasts
   `extensions.changed` with cache-busted `file://...?v=<revision>` URLs and
   `changed` / `removed` ID lists.
4. The shell leaves untouched extensions mounted without flickering, and for
   each changed extension cleanly calls its cleanup callbacks and `unmount()`,
   removes its old `<link>` and `<script>` tags, injects the new cache-busted
   stylesheet and script, and calls `mount()` in place.

No compositor restart, no page reload, and no lost windows.

## Error isolation and security

- **Strict path containment**: `script`, `style`, and `extension.json` must be
  local relative paths with no `..` components, no URL schemes, and no NUL
  bytes. Every path is canonicalized on disk and verified with `starts_with`
  to remain strictly inside the extension's canonical directory (and the
  extension directory itself must stay inside the configured root). Escaping
  symlinks and invalid manifests are logged and skipped without failing startup.
- **Fault containment**: every `mount()`, `unmount()`, `onCleanup()`, and
  `subscribe()` callback executes inside an isolated `try/catch`. A broken
  extension records its error state, cleans up any partial DOM or listeners,
  and never blanks or crashes the desktop shell or other extensions.
- **Lock screen safety**: `overlay` extensions are automatically hidden and
  dropped from `shell.overlay` while the session is locked; `extensions.reload`
  and `config.extensions` are refused while locked.
- **Trust model**: like `layout_extensions` and `widget_extensions`, extension
  scripts run in the shell page's JavaScript realm. Only install extensions you
  trust.

## Example

A minimal, working extension is shipped at
[`examples/extensions/workspace-pill/`](../examples/extensions/workspace-pill/).
Copy it into `~/.config/viewport/extensions/workspace-pill/` to see it mount
live in the center of the bar.
