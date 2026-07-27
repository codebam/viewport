# Configuration and keybindings

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
  "adaptive_sync": false,   // variable refresh rate, if the monitor will
  "idle": { "lock_after": 600, "lock_command": "swaylock -f",
            "blank_after": 900 },
  "terminal": "rio",
  "menu": "wmenu-run -i",
  "binds": {
    "Mod4+Return":  "exec rio",
    "Mod4+d":       "exec wmenu-run -i",
    "Mod4+Shift+q": "close",
    "Mod4+Shift+e": "exit",
    "Mod4+Shift+c": "reload"
  }
}
```

`reload` re-reads the config file *and* reloads the shell, so a changed
keybinding takes effect without a restart. Only keys the file actually contains
are applied, which means a key present in the file wins over the equivalent
command-line flag on reload — the file is what you just edited. Startup-only
settings (shell URL, backend, socket path) are re-read but do nothing until
restart.

Actions are `exec COMMAND`, `close`, `exit`, `reload`, `focus DIRECTION`,
`mode NAME`, `appearance toggle`, `lock`, `blank` and `shell COMMAND ARGS…`.
`lock` runs the same `idle.lock_command` the idle timer would, so there is one
place to configure what locking means; `blank` turns the outputs off until the
next input, exactly as the idle timer does. Chords use sway's
spelling — `Mod4`/`Super`/`Logo`, `Shift`, `Ctrl`, `Alt` — and any key
`xkb_keysym_from_name` accepts, including `XF86AudioRaiseVolume`. Caps and num
lock are masked out of matching. Bindings outrank both the focused client and
the shell, and fire on press only.

Asking for the workspace you are already on takes you back to the one before
it, so `Mod4+2` pressed twice from workspace 1 goes to 2 and then back to 1 —
sway's `workspace_auto_back_and_forth`, and the same toggle it gives you.
`Mod4+grave` goes back without naming a workspace. The previous workspace is
remembered per output and survives a restart along with the rest of the layout.

`focus` takes `next`, `prev`, `left`, `right`, `up` or `down`. Directional
moves compare window centres, so they follow what is on screen — including
across monitors — rather than stacking order.

`shell` forwards the rest of the line to the web shell as
`{"type":"shell.command","command":…,"args":[…]}` and does nothing else. This
is the seam for everything that is layout policy: the defaults bind
`Mod4+1..9` to `shell workspace.switch N` and `Mod4+Shift+1..9` to
`shell workspace.move N`, and *what a workspace is* is defined entirely in
`data/shell/commands.js`. Add your own commands by binding them and handling the
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

### Changing some of the keymap, or all of it

There are two keys, and which one you want depends on how much you are
replacing.

`binds_override` changes the chords it names and leaves every other built-in
alone. This is almost always the one to reach for:

```json
{
  "binds_override": {
    "Mod4+Return": "exec foot",
    "Mod4+d": null
  }
}
```

`Mod4+Return` now opens foot instead of the built-in terminal, `Mod4+d` reaches
the focused application instead of opening a launcher, and the other hundred
bindings are untouched. A `null` — or the action `"none"` — unbinds a chord.
That is not the same as leaving it out: leaving a chord out is exactly what asks
for its built-in, so removing one has to be said out loud.

`binds` replaces the built-in keymap entirely. Defining *any* `binds` object
suppresses the defaults, so an empty one means "no keybindings at all", which is
the point of it — it is how you start from nothing. Include an exit binding if
you use it, or the only way out of the session is a TTY.

Both accept the same `CHORD: ACTION` entries, and a chord written twice takes
its last definition. `data/config.example.json` is a fuller starting point.

Anything you bind beats a built-in for the same chord regardless of which key it
came from, and `--bind` on the command line beats both.

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
directory and reloads on change — so editing any of the shell scripts updates the running
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
  bindsOverride."Mod4+Shift+e" = "exit";
};
```

`bindsOverride` rather than `binds`: the two behave as they do in the config
file, so `binds."Mod4+Shift+e" = "exit"` would leave you with that one binding
and nothing else.

## Media keys

Bound by default, because a desktop where the play key does nothing is broken
in a way no application can fix from its own side: the key never reaches it.
`XF86AudioPlay` does not belong to the focused window — it belongs to whichever
player is playing, which is what `playerctl` resolves over MPRIS. Volume and
mute go to the sink through `wpctl`, since turning the volume down means the
machine and not whatever happens to be playing, and the brightness keys go to
`brightnessctl`.

Missing tools fail quietly per keypress rather than at startup, which is the
right trade for a binding nobody may ever press. As with every default, naming
any `binds` in the config replaces the whole set.

## Window rules

```jsonc
"rules": [
  { "app_id": "cs2", "workspace": 3 },
  { "app_id": "pavucontrol", "floating": true, "width": 600, "height": 400 },
  { "title": "Picture-in-Picture", "floating": true }
]
```

Matched on `app_id`, or on `title` for applications that give every window the
same `app_id` and differ only in what they show. Both are substring matches: an
exact one would need the application's internal name known exactly. A rule is
applied before the window is inserted anywhere, so it goes straight where it
belongs rather than appearing in one place and jumping.

The compositor passes these to the shell without reading them. Which workspace
a window opens on and whether it floats are layout decisions, and the
compositor has no opinion about either.

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
| `Mod4`+right-drag, or dragging a column edge | set the column width freely |
| `Mod4+Shift+r` | cycle the window's share of the column height |
| `Mod4+Home` / `Mod4+End` | jump to either end of the strip |
| three-finger swipe ←→ | scroll the strip under your fingers |
| three-finger swipe ↑↓ | previous / next workspace |

Column widths are fractions of the space a window may occupy — the tiling area
minus its padding — and the dividers between columns come out of those
fractions rather than being added on top. Both details matter: measured against
the padded box a full-width column is two gaps too wide and runs off the right
edge, and with dividers added on top two half-width columns plus the divider
between them are wider than the screen, so moving focus from one to the other
scrolls the strip and everything visibly shifts. Taking the (N-1) dividers out
of N columns makes fractions summing to 1 fill the width exactly.

Resizing means changing a column's own width. Columns do not share space, so
widening one takes nothing from its neighbours — it makes the strip longer and
shifts everything after it along. Nothing you are not touching changes size,
which is the point of the model and the one place it will surprise a sway user.
Vertical resizing inside a column works the ordinary way, since windows stacked
in a column *do* share it.

Directional focus moves to the shell in this mode: the compositor decides
direction from where windows are on screen, and the column you are reaching for
is usually scrolled off it. Both models share one tree — the strip's columns are
the workspace root's children — so switching `layout` and reloading rearranges
what is open rather than discarding it.
