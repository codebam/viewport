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
  "shell_backend": "wpe",   // or "webkitgtk"; see docs/shell-backends.md
  "timeout_ms": 5000,
  "layout": "tiling",       // or "scrolling", or "solar"
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

## A terminal as the wallpaper

`background_terminal` runs a terminal emulator and draws it behind everything —
under the windows, under the bar, in place of the desktop background.

```jsonc
{
  "background_terminal": true,              // the "terminal" configured above
  "background_terminal": "foot -e btop",    // or something specific
}
```

or `--background-terminal` on the command line, bare for the configured
terminal and `--background-terminal='foot -e btop'` for a command of its own.
Absent or `false` is off, which is the default.

**It is never given keyboard or pointer input, and that is the point.** The
terminal is not a window: it is not registered as a view, so `view.focus`
cannot name it and the shell is never told it exists; it is not in the window
space, so a click passes over it; and it is not the shell, so the key path that
delivers to the desktop does not reach it either. There is no setting to turn
that off.

The reason is that a wallpaper is the one surface on the screen that is always
present, always unobscured at the edges, and never deliberately focused. A
terminal there that could be typed into is a shell prompt underneath every
window, and every way focus can go wrong — a window closing between two
keystrokes, a race while the next window is focused, a password typed a moment
after its prompt disappeared — becomes a way for input to arrive at a command
line. Read-only costs nothing here, because what this is for is `btop`,
`journalctl -f`, a clock, a log. An interactive shell will run; it will also
sit at its prompt forever.

What the compositor gains is nothing: it spawns the command with `/bin/sh -c`,
which is what an `exec` keybinding already does, and the emulator owns its pty
exactly as it does in a window. No IPC verb runs a program, and none was added
for this.

Two other consequences worth knowing:

- **It is in screenshots and screen shares.** The wallpaper is part of every
  frame, so whatever is on it is part of anything that captures the screen.
- **The shell stops painting its own background.** The page is the bottom layer
  of the desktop and its gradient *is* the wallpaper, so with this on the
  compositor tells the shell to leave it transparent and the terminal shows
  through. A custom shell that paints an opaque background of its own will
  cover it; honour `background_terminal` in the `config` event.

It is restarted if it exits, up to five times a minute, and then left down with
a line in the log.

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

### Running one application: kiosk mode

`"startup": "COMMAND"` runs one command once the compositor is up, and is how a
kiosk names the application it exists to run. Nothing restarts it if it exits —
that belongs to a service manager, which can back off, log and give up.

`"vt_switching": false` stops `Ctrl+Alt+F1..F12` leaving the session. **This is
the escape hatch and it is the last thing to turn off.** It is checked before
the config file, before the keymap and before the shell, so it is the one thing
that still works when a shell never paints or the compositor wedges; without it
the only ways back are SSH and the power button. A public kiosk usually does
want it gone — a visitor reaching a login prompt is the threat — but disable the
getty units on the other VTs too, since this stops the compositor handing over
the keys rather than removing the consoles they would have reached. Only an
explicit `false` turns it off: a missing file, a malformed value or a `null`
all leave it on, and turning it off is logged at startup.

`examples/kiosk/` is a complete worked example — a shell that draws one
application fullscreen and ignores everything else, a config that locks the
machine down, and a README about what that does and does not achieve.

Precedence is flags > config file > defaults.

```
-u, --url URL          shell endpoint (default http://localhost:3000)
    --shell-backend NAME which engine draws the shell: wpe, webkitgtk, servo
                       or cef. The last two are recognised and refused; see
                       docs/shell-backends.md
-f, --fallback URL     used when the shell fails (default: bundled fallback.html)
-t, --timeout MS       first-paint deadline before falling back (default 5000)
-s, --socket PATH      control socket
-c, --config PATH      config file (default ~/.config/viewport/config.json)
    --layout NAME      tiling, scrolling or solar; overrides the file's "layout"
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
is usually scrolled off it.

**`solar`** — the focused window in the middle at 60% of the screen, the rest
in orbit around it: eight warm slots in the margin at full size, and everything
after that pushed to the edges and corners, drawn at 40% and dimmed. Focus is
what makes a window the middle one, so the window being typed into is never
shrunk, dimmed or covered. Two monitors run either as two independent systems
or with the second holding the first's background applications.

| Key | Solar layout |
| --- | --- |
| `Mod4+h/j/k/l` | focus by casting a ray from the middle window |
| `Mod4+bracketleft` / `Mod4+bracketright` | rotate which window is in which slot |
| `Mod4+Shift+s` | throw the focused window at the other monitor, where it lands in the middle |
| `Mod4+equal` / `Mod4+minus` | grow / shrink the middle window |
| `Mod4+Shift+g` | two independent systems, or one plus a field of background applications |

It has no resize mode and no dividers: a satellite's size is a function of the
middle window's, so growing that one is the only dimension the layout has. The
full model, its formulas and its tunables are in [solar.md](solar.md).

All three models share one tree — the strip's columns and the orbits' order are
both the workspace root's children — so switching `layout` and reloading
rearranges what is open rather than discarding it. `shell layout.model`
switches at runtime, with a name or with no argument to cycle:

```json
"binds_override": { "Mod4+Shift+m": "shell layout.model" }
```


## Dynamic tiling arrangements

`"layout": "tiling"` builds a tree of splits: a window opens beside the focused
one, and the shape is whatever the splits you made say it is. That is the
default and it is called `manual`.

```json
"tiling_mode": "master-stack"
```

replaces the manual part with an arrangement derived from *which windows are
open*, so opening one rearranges what is already there.

| Mode | What it does |
| --- | --- |
| `manual` | the tree of splits you make. The default |
| `master-stack` | the first window takes one side, the rest share a column beside it |
| `spiral` | each window takes half of what is left, turning ninety degrees every time |
| `bsp` | the same nest, but each cut goes along the region's longer side, so nothing is driven to a silly shape |

`Mod4+Shift+h` and `Mod4+Shift+l` reorder rather than resplit, so promoting a
window to master is moving it to the front — there is no separate command for
it. `layout.mode` switches at runtime, with a name or with no argument to cycle:

```json
"binds_override": { "Mod4+space": "shell layout.mode" }
```

These are arrangements, not new kinds of tree. What comes out is the same node
structure the manual mode builds, so resizing, moving, tabbed containers, the
overview and the saved session all keep working — a dynamic mode is a rule for
what the tree *should* be, applied when the set of windows changes.

That last part is the one thing to know: resize weights are reset whenever the
arrangement is rebuilt, which is whenever the shape it asks for differs from
the shape that is there — a window opening or closing, mostly, and also a
monitor rotating under `bsp`, which cuts along whichever side is longer. Every
dynamic tiler behaves this way, and it is why `manual` is still the default.

None of this touches `"layout": "scrolling"` or `"layout": "solar"`, each of
which is its own model. Solar in particular has no sub-arrangements: where a
window goes is decided by its position in the order and nothing else.


## Focus at the edge of a monitor

`Mod4+h` and `Mod4+l` step focus left and right, and by default running out of
windows on one monitor carries on to the next one — sway's behaviour, and what
this has always done.

```json
"focus_crosses_outputs": false
```

turns that off, so the edge of a monitor is where directional focus stops. It
is worth having on a wide pair of screens, where the rightmost window on the
left monitor is one keypress away from putting focus somewhere you were not
looking.

It covers `Mod4+j` and `Mod4+k` too. Vertical focus falls off the end the same
way, and a setting that held for two of the four directions would be a
different surprise rather than a fix.

Both layouts honour it, but neither owns it. Tiling asks the compositor, which
works out direction from where windows are on screen; the scrolling layout asks
the shell, because the column being reached for is usually scrolled off screen.
So the setting travels to the shell with the rest of the config, and `false`
stops both — otherwise the same keypress would cross in one layout and not the
other.

What it does not touch is asking for a monitor by name. A binding on
`shell output.focus right` still moves there: the setting is about falling off
the end of one screen, not about ever leaving it.


## Outputs

The `outputs` block is keyed by connector name — `DP-1`, `HDMI-A-1`, what
`wlr-randr` prints — and says how that screen should be brought up.

```json
"outputs": {
  "DP-1": { "mode": "2560x1440@240" },
  "DP-3": { "max_refresh": true, "transform": "90" }
}
```

| Key | Meaning |
| --- | --- |
| `mode` | `"WIDTHxHEIGHT"` or `"WIDTHxHEIGHT@RATE"`. A string, always: `"mode": 5` reads back as absent rather than being rounded into something |
| `max_refresh` | The fastest mode at the largest size the display offers |
| `scale`, `transform`, `hdr`, `x`, `y` | As the same names elsewhere |

**A preferred mode is often not the fastest one.** A 240Hz panel commonly
advertises 120Hz as preferred, and a compositor that takes the display's word
for it runs at half the rate the monitor was bought for — which is what this
one did until these keys were honoured. `"max_refresh": true` is the short way
to say "as fast as it goes"; `mode` with a rate is the exact way.

The rate is matched to the nearest whole hertz, because the kernel reports
239765 millihertz where a person writes 240. A size that exists at no such rate
uses the fastest mode of that size and says so in the log — the resolution is
the part you can see.
