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
  "url_span": true,         // that page on every monitor; see below
  "shell_backend": "wpe",   // or "webkitgtk"; see docs/shell-backends.md
  "timeout_ms": 5000,
  "layout": "tiling",       // or "scrolling", "solar", "matrix", "canvas"
  "adaptive_sync": false,   // variable refresh rate, if the monitor will
  "pixel_format": "auto",   // or "10" / "8" bits per channel; see below
  "idle": { "lock_after": 600, "lock_command": "swaylock -f",
            "blank_after": 900 },
  "lid": "lock",            // or "blank" / "suspend" / "ignore"; see below
  "cursor": { "theme": "Bibata-Modern-Classic", "size": 24,
              "hide_after_ms": 3000 },   // see below; absent is never
  "wallpaper": "~/Pictures/wall.png",  // the desktop background; see below
  "wallpaper_mode": "fill",            // or "fit" / "stretch" / "center" / "tile"
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
`mode NAME`, `appearance toggle`, `lock`, `blank`, `background` and
`shell COMMAND ARGS…`. `background` is the wallpaper terminal's only way in —
see below.
Neither deadline fires while something is holding the screen awake, and
holding it is not only Wayland's `idle-inhibit`: `org.freedesktop.ScreenSaver`
and the inhibit portal are answered too, which between them is what a browser
playing video, a video player and a presentation actually use. Nothing to
configure — see [Idle](protocols.md#idle) for what a hold is and when it ends.

`lock` runs the same `idle.lock_command` the idle timer would, so there is one
place to configure what locking means; `blank` turns the outputs off until the
next input, exactly as the idle timer does. `lid` is what the laptop hinge
does: `"lock"`, `"blank"`, `"suspend"` (via logind) or `"ignore"`. Absent is
lock when `idle.lock_command` is set, otherwise blank. A desktop has no lid,
so the setting never fires. Chords use sway's
spelling — `Mod4`/`Super`/`Logo`, `Shift`, `Ctrl`, `Alt` — and any key
`xkb_keysym_from_name` accepts, including `XF86AudioRaiseVolume`. Caps and num
lock are masked out of matching. Bindings outrank both the focused client and
the shell, and fire on press only.

A chord's key may be a mouse button instead: `Mod4+Mouse4=shell workspace.next`
and `Mod4+Mouse5=shell workspace.prev` are the usual thumb-button gestures (the
shell defines those commands, so name your own in `handleShellCommand()`).
Buttons are named `Mouse1`–`Mouse5`, `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE`/
`BTN_SIDE`/`BTN_EXTRA`, or `XButton1`/`XButton2`. A button binding fires on the
press while the modifier is held, and consumes the click — it is not passed on
to the window under the pointer.

A name the keymap knows is always the key: `Mod4+Left` is the arrow key, not
the left button. Write `Mod4+Mouse1` or `Mod4+BTN_LEFT` for the button. This
matters because a button binding consumes the press — `Mod4+Left` read as a
button swallows every Mod4-held click, which is all of them on a `bar: auto`
that is only on screen while Mod4 is down.

The scroll wheel binds the same way: `Mod4+WheelUp=exec …` and
`Mod4+WheelDown=…` (also `ScrollUp`/`ScrollDown`). A wheel binding fires once
per notch of a physical wheel while the modifier is held and consumes the
scroll; a touchpad's two-finger scroll is never bound — it keeps scrolling
whatever is under the pointer.

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

The shell is sent this keymap — the real one, after a config file has had its
say — and the empty desktop lists it under the mark. That is why the listing
follows a layout: a few chords only exist in one, and a shell showing a table
of its own would be describing a keyboard nobody has. Both halves of each row
are spelled the way a config file spells them, so a chord you want to change
can be copied straight into `binds`. Turn the listing off with
`"tutorial": false`, which is the same key that hides the rest of it, and the
mark it sits under with `"logo": false`. With both off an empty workspace is
empty, which on an OLED panel is two fewer things sitting in fixed pixels.

### Default bindings

| Chord | Action |
| --- | --- |
| `Mod4+Return` / `Mod4+d` | terminal / launcher |
| `Mod4+h j k l`, arrows | focus, crossing monitors at the edge |
| `Mod4+Shift+h j k l` | move window, carrying it to the next monitor at the edge |
| `Mod4+Tab` / `Mod4+Shift+Tab` | step through the windows on this workspace |
| `Mod4+1‑9` / `+Shift` | switch / move to workspace |
| `Mod4+b` / `Mod4+v` | next window splits horizontally / vertically |
| `Mod4+e` | flip the focused container's layout |
| `Mod4+f` | fullscreen |
| `Mod4+r` | resize mode; then `hjkl`, `Escape` to leave (scrolling and canvas bind it to something of their own; solar leaves it unbound) |
| `Mod4` + right-drag | resize, from the corner nearest where the drag started; dragging the gap between windows also works |
| `Mod4+n` | toggle the bar |
| `Mod4+Shift+d` | toggle dark mode |
| `Mod4+Shift+q` / `+e` / `+c` | close / exit / reload the shell |
| `Mod4+Shift+Return` | give the keyboard to the wallpaper terminal, and take it back |
| `Mod4+Shift+v` | the clipboard history |
| `Mod4+Shift+m` | the notification centre |
| `Mod4+Shift+n` / `Mod4+Shift+t` | the Wi-Fi picker / the Bluetooth picker |
| `Mod4+Shift+k` | the on-screen keyboard, which otherwise comes up on its own |

## The pointer

`cursor.theme` and `cursor.size` set `XCURSOR_THEME` and `XCURSOR_SIZE` for the
compositor and for everything it starts, which is what makes the pointer drawn
over a window and the one drawn over the desktop the same picture. A theme name
with spaces is also looked for hyphenated, because the directory on disk
usually is.

`cursor.hide_after_ms` takes the pointer off the screen once it has been still
for that long — an arrow parked in the middle of a film. Zero or absent is off,
which is the default. Any use of the pointer brings it straight back: motion, a
button, the scroll wheel, a touchpad gesture or a tablet pen. Typing does not,
deliberately — someone writing with the mouse pushed aside is exactly who asked
for this, and a cursor that came back on every keystroke would never leave.

Only the drawn image goes. The pointer has not moved, keeps its focus, and
clients are told nothing, so a hidden cursor cannot make a page think the mouse
left it.

## The keyboard

`keyboard.layout`, `keyboard.variant` and `keyboard.options` are xkb's, spelled
as `setxkbmap` spells them, and they are the first thing anyone outside a US
layout needs:

```jsonc
{
  "keyboard": {
    "layout": "de",
    "variant": "nodeadkeys",
    "options": "ctrl:nocaps,compose:ralt",
    "repeat_rate": 25,
    "repeat_delay": 200
  }
}
```

`repeat_rate` is repeats a second and `repeat_delay` is how long a key is held
before they start, in milliseconds. Absent is 25 and 200, which is sway's pair
and the compositor's.

The block is read once, at startup. A keymap is set by handing the seat a new
keyboard and there is no way to change the layout of one that already exists,
so this happens before any client has seen the seat — changing it means
restarting the compositor. An entire block left out changes nothing, and a
layout xkb refuses is logged by name and the current keymap kept, because an
unknown layout that silently left the built-in one in place looks exactly like
a config file that was never read.

Chords are separate: `binds` names them by keysym, so the same binding follows
the layout rather than the key's position. See [Changing some of the keymap, or
all of it](#changing-some-of-the-keymap-or-all-of-it).

## A picture as the wallpaper

`wallpaper` names what to draw as the desktop background, and `wallpaper_mode`
says how a picture is fitted to the screen.

```jsonc
{
  "wallpaper": "~/Pictures/wall.png",  // a path, or a URL of its own
  "wallpaper_mode": "fill",            // fill, fit, stretch, center or tile
}
```

It does not have to be a picture. A CSS value is passed through to the page
untouched, so a colour scheme with no photograph in it is one line:

```jsonc
{
  "wallpaper": "#1a1b26",                              // a flat colour
  "wallpaper": "linear-gradient(#1a1b26, #24283b)",    // or a gradient of yours
  "wallpaper": "url(/pic/wall.png)",                   // the CSS spelling works too
}
```

A value is read as CSS when it is `#rrggbb`, `transparent`, or anything of the
form `name(...)` — `rgb()`, `hsl()`, every `gradient()`, `url()`. Everything
else is a path, which is why a colour has to be written as `#000000` or
`rgb(0,0,0)` rather than `black`: a bare word is a relative path as far as this
can tell, and there is no way to have both. `wallpaper_mode` applies to
pictures and gradients; a flat colour has nothing to fit.

or on the command line, which wins over the file:

```console
$ viewport --wallpaper ~/Pictures/wall.png --wallpaper-mode fit
```

or at runtime, over the control socket, which is what a wallpaper cycler or a
settings panel uses — no config reload, nothing written to disk:

```console
$ viewport msg -t config.wallpaper --path ~/Pictures/other.png
$ viewport msg -t config.wallpaper --mode tile      # the picture stays
$ viewport msg -t config.wallpaper --path ''        # and this removes it
```

The five fittings, which are `background-size` under the skin:

| mode | what it does |
| --- | --- |
| `fill` | covers the screen, cropping whatever overflows. The default |
| `fit` | the whole picture, letterboxed against the desktop colour |
| `stretch` | covers the screen, distorting the picture to do it |
| `center` | at its own size, in the middle |
| `tile` | at its own size, repeated |

`cover` and `contain` are accepted as other names for `fill` and `fit`, because
that is what sway's `output bg` calls them.

**The shell paints it, and the compositor resolves it.** The page is the bottom
layer of the desktop, so an image there is one the page loads: what the
compositor does is turn the path into a URL, check the file is actually there,
and send it with the rest of the config. A CSS value has no file to find and is
handed over as written. A path that is missing is a line in
the log for the config file, an error on the socket, and a refusal to start for
the flag — never a background that silently does not change, which is the one
failure worth engineering against here.

A consequence: the picture is loaded by whichever engine draws the shell, so it
can be any format that engine reads — PNG, JPEG, WebP, SVG, an animated GIF —
and a `https://` URL works as well as a path. A custom shell has to honour
`wallpaper` in the `config` event; the shipped one does.

**A terminal wins over a picture.** With `background_terminal` on, the page
goes transparent so the terminal behind it can be seen, and a picture in the
page would be painted straight over it. Nothing is refused — the setting is
simply not in force while a terminal is behind — so turning the terminal off
brings the picture back.

**A wallpaper program still wins over both**, per screen, in the way described
below: swaybg and the rest draw on the background layer, which is over the
whole desktop including this.

### stylix

[stylix](https://github.com/danth/stylix) themes a NixOS or home-manager
session from one image, and this reads its two settings unchanged — the mode
names here *are* `stylix.imageScalingMode`, which is why they are spelled that
way. Point the compositor at them:

```nix
# NixOS, wherever the compositor is launched from
programs.viewport.extraArgs = [
  "--wallpaper" "${config.stylix.image}"
  "--wallpaper-mode" config.stylix.imageScalingMode
];
```

or, if the config file is generated rather than the command line:

```nix
xdg.configFile."viewport/config.json".text = builtins.toJSON {
  wallpaper = "${config.stylix.image}";
  wallpaper_mode = config.stylix.imageScalingMode;
  # And the colours, which are the other half of a themed desktop.
  theme = with config.lib.stylix.colors.withHashtag; {
    background = base00;
    foreground = base05;
    accent = base0D;
  };
};
```

`config.stylix.image` is a store path, so the file is there for as long as the
generation is — which is the case the existence check was written for, since a
garbage-collected picture would otherwise be a desktop that came up grey.

Stylix's own targets do not know about this compositor, so nothing sets it
behind your back; if `stylix.targets.swaybg` (or another wallpaper program) is
enabled in the same session, that program takes the background layer and wins
per screen, as above. Turn it off, or leave `wallpaper` unset and let it do the
job.

## A terminal as the wallpaper

`background_terminal` runs a terminal emulator on every monitor and draws each
behind everything — under the windows, under the bar, in place of the desktop
background.

```jsonc
{
  "background_terminal": true,              // the "terminal" configured above
  "background_terminal": "foot -e btop",    // or something specific
}
```

or `--background-terminal` on the command line, bare for the configured
terminal and `--background-terminal='foot -e btop'` for a command of its own.
Absent or `false` is off, which is the default.

**Input reaches it only when you ask, by name.** `Mod4+Shift+Return` — the
`background` action, or `{"type":"background.focus"}` on the control socket —
gives the keyboard to the wallpaper terminal on the monitor you are looking at,
and the same chord gives it back to the window that had it. That is the whole
of the way in.

Nothing else can put focus there. The terminal is not a window: it is not
registered as a view, so `view.focus` cannot name it and the shell is never
told it exists; it is not in the window space, so a click on the desktop passes
over it; and it is not the shell, so the key path that delivers to the desktop
does not reach it either. There is no click-to-focus and no focus-follows-mouse
for it, deliberately.

The reason for the asymmetry is that a wallpaper is the one surface on the
screen that is always present, always unobscured at the edges, and never
deliberately focused. A terminal there that *drifted* into focus would be a
shell prompt underneath every window, and every way focus can go wrong — a
window closing between two keystrokes, a race while the next window is focused,
a password typed a moment after its prompt disappeared — becomes a way for
input to arrive at a command line. A chord you pressed is not one of those
ways, which is why that one is allowed and the accidents are not.

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
- **It needs `wpe` or `webkitgtk`.** Those two composite the page over nothing.
  Chromium does not, in either of the two ways this ships it: with CEF's
  `background_color` set to transparent on the browser settings, the view *and*
  the window, the composited output is still Chromium's own `#1f1f1f` across
  the whole screen — windowed Chromium has no translucent-surface path on
  Wayland, and its transparent-painting one is windowless rendering, which is a
  different backend to this. On `cef` or `chromium` the terminal is therefore
  not started at all, and the log says so: an invisible terminal under an
  opaque desktop is a process nobody can see spending a core. Since `cef` is
  the default package, using this means `--shell-backend=webkitgtk`.

**One per screen, not one across the layout.** A terminal is a grid of cells
and the cells have to land on a monitor: stretched across two, half the columns
are on the other one and every line is cut down the middle by the gap between
them. So each output gets a process of its own, sized to that output, plugged
in and unplugged with it. Each is told which screen it is on through
`VIEWPORT_OUTPUT`, which is how one setting can do something different per
monitor:

```jsonc
{
  "background_terminal":
    "sh -c 'case $VIEWPORT_OUTPUT in DP-1) foot -e btop;; *) foot -e journalctl -f;; esac'"
}
```

Each is restarted if it exits, up to five times a minute, and then left down
with a line in the log.

**A wallpaper program wins, per screen.** swaybg, hyprpaper, wbg and the rest
are layer-shell clients on the background layer, which is drawn over everything
the terminal puts on that monitor — so when one appears the terminal on *that*
monitor is asked to close, and is killed five seconds later if it ignores that.
`swaybg -o DP-1` therefore takes one screen and leaves the terminal on the
other. When the program goes, that screen's terminal comes back. Running both is otherwise a program painting
frames nobody can see, which on a laptop is a core's worth of battery spent on
a picture that is covered up.

## Dark mode

Styling the shell cannot make client applications dark. Firefox, GTK and Qt
each ask the same question — `color-scheme` in the `org.freedesktop.appearance`
namespace, over D-Bus through xdg-desktop-portal — and with nothing answering
they all default to light.

`"dark_mode": false` is what the session starts on; absent is dark, which is
what the shell is drawn for. `Mod4+Shift+d` flips it at runtime, and either way
the portal signals the change, so applications already running follow it.

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
the point of it — it is how you start from nothing.

One binding survives that: if nothing in the resulting keymap quits, and
`Mod4+Shift+E` is free, `Mod4+Shift+e=exit` is added back. A config file that
did not think about it cannot produce a session with no way out of it, and a
`--url` session — a web page and nothing else, no launcher and no terminal to
reach — is where that matters most. A file that binds `Mod4+Shift+E` to
something *else* is left alone and warned about: `Ctrl+Alt+Backspace` is the
chord underneath that no config file can take away.

Both accept the same `CHORD: ACTION` entries, and a chord written twice takes
its last definition. `data/config.example.json` is a fuller starting point.

Anything you bind beats a built-in for the same chord regardless of which key it
came from, and `--bind` on the command line beats both.

### A web page on one monitor, the desktop on the rest

`url` — or `--url` — names the page the session shows. With one monitor it is
the desktop: it gets the whole screen, and whether it lays windows out is up to
what the page does. That is the shell being developed (`--url
http://localhost:3000`) and it is also a kiosk (`--url https://example.com`,
one screen, one site, nothing else).

With **two monitors or more** the two readings come apart, so they are split:

* the page takes the **first monitor** — first by the order the outputs were
  detected, which is the order the session came up in, not by position;
* the **shipped desktop** runs on the rest, in a second shell process of its
  own.

So `--url https://example.com` on a two-monitor desk is that site on the main
screen and a working desktop — bar, windows, workspaces — on the other, rather
than one page stretched across both and no window manager anywhere.

`"url_span": true`, or `--url-span`, puts it back to one page across every
screen. That is what a shell under development wants: it *is* the desktop, it
just is not the shipped one.

Monitors can arrive and leave at runtime and the arrangement follows. Plugging
a second screen into a `--url` session starts the desktop on it without
restarting the page — same process, same document, a resize — and unplugging it
hands the page the whole desk back.

Only the desktop page is sent window events, given the keyboard by
`shell.focus`, or drawn above windows for `shell.overlay`. The other is a web
page: it receives pointer and keyboard input on its own screen like any client,
and nothing else.

**Each page's world is its own rectangle.** A page lays out in a document that
starts at (0, 0) however far across the desk the page itself begins, so:

* `output.layout` sent to a page lists only the screens it covers, with
  positions relative to its own top-left. A desktop confined to the second
  monitor is told about that monitor, at `+0+0`, and is not told the first one
  exists — it must not place a window on a screen it does not cover.
* `view.layout`, `shell.overlay` and `screencast.rect` coming back from a page
  are read in that page's coordinates and moved into the layout's.

A script on the control socket has no page to speak in, so it speaks layout
coordinates and is told the layout as it really is. Which connection is a page
is decided by the pid the kernel reports for it (`SO_PEERCRED`), matched
against the processes the compositor started — a client cannot claim to be the
desktop, because it does not choose its own pid.

`viewport=debug` logs both halves: `shell N: its screens are …` for what each
page was told, and `view N: placed at …; the page asked for …` for where a
window ended up and what was asked for.

This is implemented for the out-of-process backends — `webkitgtk` and
`chromium`, and `cef` when it lands. The in-process `wpe` backend still runs one
engine across the whole layout, so `--url` there behaves as it always did.

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
--drm                        take the screens and the seat: a session of its own, from a TTY
--headless                   no renderer and no window: everything but drawing, for tests
--width N                    the headless output's width (default 1920)
--height N                   and its height (default 1080)
--config PATH                the config file, instead of the default one
--layout NAME                tiling, scrolling, solar, matrix or canvas, over the config
--renderer NAME              vulkan or gles, over $VIEWPORT_RENDERER
--pixel-format N             scanout bits per channel: 8, 10 or auto, over $VIEWPORT_PIXEL_FORMAT
--shell-backend NAME         which engine draws the desktop; see docs/shell-backends.md
--url URL                    a page to run instead of the bundled desktop
--url-span                   give that page every monitor, not just the first
--watch-shell                reload the shell when its files change
--watch-config               reload the configuration file when it changes
--background-terminal [CMD]  a terminal for a wallpaper, running CMD if one is given
--wallpaper PATH             a picture for the desktop background, over the config file
--wallpaper-mode NAME        how it is fitted: fill, fit, stretch, center or tile
--socket PATH                the control socket, instead of the one named after the display
--exit-after SECS            stop after this long, in case stopping is what is broken
-h, --help                   this
```

That is the whole list — `OPTIONS` in `crates/viewport/src/main.rs` is both what
`--help` prints and what an argument is checked against, so there is nowhere
else for a flag to hide. There are no short forms other than `-h`, and an
option that is not on this list is not an error: it is logged as `unknown
option …; it has been ignored` and the compositor carries on. `--url` has no
default of its own — with nothing given, the shell is the bundled desktop,
loaded from a `file://` URL beside the binary.

`viewport msg` takes its own options; `viewport msg --help` lists every message
and its fields, and [IPC](ipc.md) is what they do.

## Screen sharing and remote control

Both are portals, both are answered by the compositor itself, and both put the
same dialog on screen before anything happens.

`org.freedesktop.impl.portal.ScreenCast` is why a browser can share a *window*
here. The usual backend is xdg-desktop-portal-wlr, which can only offer
monitors — it captures through wlr-screencopy, which captures outputs and
nothing else — so an application asking for a window was handed a whole screen.
The chooser lists every window and every monitor, plus three sources that name
whatever is in front at the time: all monitors at once, the focused window, and
the active monitor.

`org.freedesktop.impl.portal.RemoteDesktop` is the same interface with input
added — a remote-support tool, or a call where the other end takes the mouse.
The dialog says which devices are being asked for (`keyboard`, `mouse`,
`touchscreen`), because that is the part being agreed to, and Enter allows all
of them while Esc refuses. An application that also asked for a picture gets
the source list underneath, and the one choice covers both.

Two things it deliberately will not do. It never remembers a grant: a screen
share can be restored from a token so that OBS finds the same monitor months
later, and the same trick applied to a keyboard would be a process that can
type into this machine on the strength of a blob in the permission store. And
it refuses outright when the desktop page is not drawing, so a broken shell
cannot become a way in — a screen share falls back to the focused window in
that case, which is a much smaller thing to get wrong.

Registering both needs the same three environment variables the Settings
portal needs, listed under **Dark mode** above; `start.sh` sets them.
`data/portal-config` and `data/portal-share` are the two files that name this
compositor as the backend, and they have to name it for *both* interfaces —
the frontend uses one session handle across the two, so a configuration that
sends ScreenCast here and RemoteDesktop somewhere else cannot work.

## Gaps

The space around and between windows. Every layout model reads the same
values: the tiling tree's dividers, the scrolling strip's columns and the
matrix's slots all come out of them, so a few numbers space the whole
desktop.

```jsonc
{
  "gaps": { "inner": 8, "outer": 0, "smart": false }
}
```

`gaps.inner` is the gap between adjacent windows, in pixels. `gaps.outer` is
extra space around the edge of the output, added *on top of* the inner gap —
so the space where the desktop meets the screen edge is `inner + outer`,
while between two windows it is still just `inner`. `gaps.smart`, when true,
drops the inner gap on a workspace that holds a single window filling the
tiling area, so a lone window does not sit far from its own screen edge. In
the scrolling layout a column keeps the width it was given, so this applies
only to a lone column at full width: a half-width column does not reach the
screen edge anyway, and collapsing the gap around it would only move it.
Absent fields keep the shell's defaults: inner 8, outer 0, smart off.

The same custom properties are what the theme keys `gap` and `gap-outer` set
when you want to choose the units yourself; `gaps.inner` / `gaps.outer` are
the explicit options and win when both are present. Either way they land on
`--gap` and `--gap-outer`, which the layouts read back at layout time, so a
change takes effect on the next reload.

**At runtime.** `config.gaps` on the control socket sets the gaps without
touching the file on disk — for a keybinding, a settings panel, or trying
values before editing the config:

```sh
viewport msg -t config.gaps --inner 8 --outer 0 --smart false
```

Each field is optional; only the ones given change. It updates the
compositor's config and re-announces it over the same channel a config
reload uses, so the change applies immediately. A gap of zero is accepted
deliberately; a negative gap is refused.

## Window borders

The frame drawn around a window.

```jsonc
{
  "border": { "radius": 6, "width": 2, "smart": false }
}
```

`border.radius` is the corner radius in pixels, measured on the *outside* of
the border. Absent keeps the shell's default of 6; zero is a square desktop.
`border.width` is how thick the border is, in pixels. Absent keeps the
shell's default of 2; zero draws no border at all, leaving the gap between
windows to separate them.

`border.smart` squares the corners of a workspace's lone window, the way
`gaps.smart` drops its inner gap — sway's `smart_borders` for the same window.
Absent is **off**, and it is its own setting: `gaps.smart` says nothing about
it. As with the gaps, the scrolling layout counts only a lone full-width
column — a half-width one does not reach its own screen edge to begin with.

It used to follow `gaps.smart`, on the argument that smart gaps push a lone
window against the edge of the screen and a rounded corner there is a notch of
wallpaper in the corner of the monitor. The argument holds and the default did
not: a radius that had been asked for went unhonoured on every desktop with a
single window on it, and came back when a second one opened, which reads as a
broken setting rather than as a rule. Anyone who wants that behaviour asks for
it with `"border": { "smart": true }`.

Which window is the lone one is a question about the layout, so the shell
answers it and sends `square` on that window's `view.layout`; the compositor
takes the corner off the crop when it is told to, and works nothing out for
itself.

These are the appearance settings the compositor reads as well as the shell.
A window's contents are a client surface the compositor draws itself, not part
of the page — so a rounded frame with the client's square corner sitting on top
of it is a rounded frame nobody can see. The compositor crops each client to
the same corner the page drew, tighter by the border's width so the two curves
are concentric. Fullscreen windows keep their square corners, as they
lose their border.

The corner is cut rather than shaded: there is no antialiasing on it, because
the DRM path draws through a Vulkan renderer with no shader hook of its own and
rounding only the nested backend would be worse than rounding neither. What
steps is the edge between the client and the border behind it, both of which
are drawn — not the outline of the window against the wallpaper, which is the
page's own antialiased curve.

The theme key `radius` sets `--radius`, which `--window-radius` follows, so a
theme still rounds the whole desktop at once. The compositor cannot read CSS,
though: a theme that changes `radius` and leaves `border.radius` alone moves
the frame the page draws without moving the crop, and the client is cut to a
corner that is no longer there. Set both, or set `border.radius`.

**At runtime.** `config.border` on the control socket, the same way
`config.gaps` works:

```sh
viewport msg -t config.border --radius 12 --width 3 --smart true
```

Each field is optional; only the ones given change. Zero is accepted for
either; a negative value is refused.

`"decorations": "client"` hands the frame back to the client instead: the
compositor tells xdg-decoration and the KDE server-decoration manager alike
that the application should draw its own titlebar. Any other value, and
absence, keeps the frame here — which is what the shell's border is. Both
protocols are answered with the same setting, since a client that probes the
manager and one that asks per surface must not be told different things.

## Bar widgets

The bar ships with a fixed set of modules — clock, CPU, memory, load, root
disk, network. `bar_widgets` *adds* to that set without touching it: leave the
key out and the bar is exactly as it shipped. Each entry names a widget and
its options:

```jsonc
{
  "bar_widgets": [
    { "type": "disk", "path": "/home" },
    { "type": "volume" },
    { "type": "weather", "location": "New York" }
  ]
}
```

`disk` shows the free space on a mount. `path` defaults to `/`. Several can be
listed, one per mount you want to watch. Free and total bytes are sampled by
the compositor with the rest of the status — the page cannot read `statvfs`
any more than it can read `/proc` — so they appear as soon as the status does,
and update with it.

`volume` shows the default audio sink's volume and mute state. The compositor
asks the session's PipeWire, via `wpctl` (`wpctl get-volume @DEFAULT_AUDIO_SINK@`),
once per status sample; when there is no `wpctl`, no session bus or no sink,
the widget is simply left empty rather than failing. Like the other modules,
it uses the V-shaped audio glyph `󰕾` and `󰝟` when muted.

`mic` is the same widget aimed at the microphone: it shows the default audio
*source's* volume and mute state, read through
`wpctl get-volume @DEFAULT_AUDIO_SOURCE@`. It uses the microphone glyphs `󰋛`
and `󰋜` when muted. A muted node keeps showing its percentage — muting silences
output, it does not zero the knob — only the glyph changes.

`weather` shows the current conditions (temperature and a condition glyph) for
a location. It is the one widget the shell fetches for itself rather than the
compositor sampling: the page can reach the network even where it cannot read
`/proc`. It uses [open-meteo](https://open-meteo.com) — no API key, answers
from any origin — resolving the location through its geocoding service, then
reading `temperature_2m` and `weather_code`. A bare `"location": "New York"`
is the form to use; several weather widgets may list different places. Refreshed
every fifteen minutes, and left empty on failure rather than crashing the bar.

`mpris` shows what is playing, with the buttons to drive it. Every media
player on a Linux desktop publishes MPRIS — a bus name beginning
`org.mpris.MediaPlayer2.`, an object, and metadata behind it — which is why
`playerctl` works everywhere; the compositor reads it, because the page has no
bus and a widget that shelled out twice a second would be two processes a
second on an idle desktop. It is the one widget that is not a line of text: a
cover, a previous, a play/pause and a next button, and the track. Only the
buttons the player answers for are drawn — `CanPause` is false on a live
stream that can only be stopped, and a button that does nothing is worse than
no button. Clicking the widget anywhere else is play/pause and scrolling it
skips, up for the track before and down for the one after. Where several
players are running, the one that is playing wins over one that is paused,
which is the rule `playerctl` uses. Nothing playing draws nothing at all.

A volume widget is optional plumbing on the compositor: since the default bar
does not show volume, the `wpctl` subprocess is only spawned when a `volume`
or `mic` widget is present, and only then does a status sample pay for it (one
subprocess per audio widget kind). The same
goes for the extra mounts — a bar with no widgets stats nothing extra. `mpris`
is the same rule taken further: with no media widget on the bar, the
compositor opens no connection for it, starts no thread and follows no player.

`battery` shows the charge, from UPower's DisplayDevice. The compositor reads
it — the page has no bus — and only while this widget is on the bar. Clicking
it opens a picker of power profiles (`power-saver`, `balanced`, `performance`)
from the power-profiles daemon, the same way the clipboard picker lists
history. No battery, or no UPower, draws nothing at all. Lid policy can still
talk to UPower with no widget on the bar; see `lid` below.

## Overriding the whole bar

`bar_widgets` adds to the shipped set, but it cannot move a widget into the
middle of the modules or drop a module you do not want. `bar_items` replaces
the entire right side of the bar with an explicit, ordered list, where each
entry is either a module the bar already draws or a widget:

```jsonc
{
  "bar_items": [
    "net",
    { "type": "disk", "path": "/games" },
    "clock",
    { "type": "weather", "location": "Pickering, ON, Canada" }
  ]
}
```

A bare string names a built-in module — `mode`, `tray`, `net`, `disk`, `cpu`,
`load`, `memory` or `clock` — and an object names a widget, taking exactly the same
options as a `bar_widgets` entry. The bar draws only what the list names, in
the order given, so a widget can sit between the network and the clock, and a
module you leave out does not appear. Present but empty draws no right side at
all. Leave the key out entirely and the bar is the shipped default plus any
`bar_widgets`.

`bar_items` supersedes `bar_widgets` when both are present: the override wins,
and the widget list drives the same status sampling (mounts and volume) as the
additions would have.

## Interacting with the bar

Widgets are not passive — the bar is a web page, so the pointer already rests on
it, and each widget turns that into a command the compositor runs on the host
through `shell.exec`, the same spawn path a keybinding's `exec` uses.

- **`volume`** — scrolling raises or lowers the default sink's volume in 5%
  steps; right-click toggles mute.
- **`mic`** — the same controls on the microphone: scrolling adjusts the
  default source's volume, right-click toggles its mute.

Those two go through `status.volume` rather than `shell.exec`: the compositor
runs the same `wpctl` it samples with, waits for it, and re-samples — so the
number on the bar is the new one immediately. Sending the command and asking
for a refresh separately is a race the refresh wins, which showed as a volume
that moved and a bar that did not until the next two-second tick.
- **`disk`** — clicking opens the mount in the default file manager
  (`xdg-open <path>`), so for a terminal-first setup it drops you at the
  directory.
- **`weather`** — clicking opens the place in a browser.

- **`net`** — the one built-in module that is not read-only: clicking it opens
  the Wi-Fi picker, the same one `Mod4+Shift+n` opens. Its tooltip names the
  network in use once the picker has been opened at least once — before that
  the compositor has not spoken to NetworkManager at all, so there is nothing
  truthful to say.

The other built-in modules (a bare string in `bar_items`) carry none of this —
they are read-only. The workspace pills and the taskbar are: clicking a number
switches to that workspace, clicking a title focuses that window.

Under `bar: auto` the bar is on screen only while Mod4 is held, so every click
it receives arrives with that modifier down. The compositor declines its own
Mod4 gestures — move, resize, pan — over anything the shell has drawn in front
of the windows, so those clicks reach the bar rather than dragging what is
behind it. The cost is the few pixels of a window that has been moved under the
floating bar: grab it anywhere else.

## Reloading the shell while it runs

The shell is a web page, and the loop of working on one is editing a file and
looking at the result. Two ways to close that loop without restarting the
compositor — which would take the windows with it, and on DRM the screen.

**The keybinding.** `reload` — `Mod4+Shift+c` in the shipped config — reloads
the page now, bypassing the engine's cache. Always available, nothing to turn
on.

**The watch.** `--watch-shell`, or `VIEWPORT_WATCH_SHELL=1`, watches the
directory the page was loaded from and reloads when a file in it changes, so
saving *is* the reload. `--watch-config`, or `VIEWPORT_WATCH_CONFIG=1` (or
running with `--watch-shell`), watches the active configuration file and
reloads its settings on save. Off by default: a reload throws the shell's state away,
and an installed desktop's files do not change under it.

What is watched is the directory holding the `file://` URL that was loaded —
`--url`'s, and the shipped shell's — and its subdirectories, four levels down.
A shell served over HTTP is left alone: that is a dev server's job, and
watching it would mean polling.

Changes are debounced by 200ms, and only files a browser would fetch count
(`.html`, `.css`, `.js`, images, fonts). Both matter for the same reason: an
editor saving one file writes a temporary beside it and renames it over the
top, and vim leaves a `.swp` and a numeric probe file behind before a character
is typed. Without the filter, opening a file would reload the desktop; without
the debounce, a `git checkout` would reload it once per file.

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

The volume keys move the sink 5% a press and the brightness keys the backlight
by the same, because a binding fires on press and does not repeat while the key
is held — `data/config.example.json` shows the same five at 1% for anyone who
wants a finer adjustment, and a `binds_override` entry replaces one of these
without touching the rest.

Missing tools fail quietly per keypress rather than at startup, which is the
right trade for a binding nobody may ever press. As with every default, naming
any `binds` in the config replaces the whole set.

The key names are the ones xkb gives the keys themselves. The play/pause key is
`XF86AudioPlay`: xkb maps it to `[XF86AudioPlay, XF86AudioPause]` and chords
are matched on the unshifted keysym, so `XF86AudioPause` names the *dedicated*
pause key (`KEY_PAUSECD`, rare on a keyboard) and not the one on the media row.
`wev` prints what any key really sends.

## Notification sounds

The compositor claims `org.freedesktop.Notifications` itself, so the sound a
notification makes is configured here rather than in mako or dunst. There is
no notification daemon left to configure.

```jsonc
{
  "notifications": {
    "sound_file": "/run/current-system/sw/share/sounds/…/message.oga",
    "sound_name": "message-new-instant",  // ignored when sound_file is set
    "history": 50                         // 0 keeps no record at all
  }
}
```

`sound_file` is a path, played as given. `sound_name` is a name from the
[sound naming specification][sound-naming], resolved against the installed
sound theme — `bell`, `message`, `complete`, `dialog-warning`. A path always
resolves and a name may resolve to nothing on a machine with a thin theme, so
`sound_file` wins when both are set. Blanking a value is the same as leaving
the key out.

Absent is silence, and there is deliberately no built-in default: the
freedesktop sound theme every distribution installs has no notification event
of its own, so a name chosen here would be one desktop's taste imposed on
every other.

**What a sender asks for wins.** The specification gives programs three hints
and all three are honoured. `sound-file` and `sound-name` mean the same as the
config keys above and override them for that one notification, and
`suppress-sound` silences it entirely — that hint means "I am playing my own",
and a server that played anyway would make two sounds for one event. So the
config setting is what a notification sounds like when the program sending it
expressed no opinion, which is nearly all of them: `notify-send` sets none of
these hints.

Playback is PipeWire directly — the same library the screencast portal streams
through, so this costs the compositor no new dependency — with [symphonia][]
decoding the `.oga` and `.wav` a sound theme ships. The stream is described in
the file's own rate and channel count and PipeWire converts, and it carries
`media.role = Notification` so a session manager that ducks other audio for an
alert can see what it is.

Each sound decodes and plays on a thread of its own, so a sender is never
blocked for the length of one; decoded files are kept, because the same short
sound plays for the life of the session. A session with no sound server plays
nothing, says so once at startup, and drops `sound` from the capabilities it
reports so a program that would have suppressed our sound in favour of its own
knows to go ahead.

The `sound_name` lookup is the sound-theme search written out: `$XDG_DATA_HOME`
and each `$XDG_DATA_DIRS` entry's `sounds/`, the theme in `$XDG_SOUND_THEME`
(default `freedesktop`), its `stereo/` profile before its flat directory, `.oga`
before `.ogg` before `.wav`, and `Inherits` in each `index.theme` followed
breadth-first with `freedesktop` as every theme's implicit parent.

[sound-naming]: https://specifications.freedesktop.org/sound-naming-spec/latest/
[symphonia]: https://github.com/pdeljanov/Symphonia

## The system tray

On unless a file turns it off. The compositor claims
`org.kde.StatusNotifierWatcher` and a host name beside it, and forwards each
registered item to the shell, which draws the icons on the bar — the same
arrangement as notifications, and for the same reason.

```jsonc
{
  "tray": true,           // false: claim neither name, and draw nothing
  "icon_theme": "Papirus" // searched before hicolor, which is always searched
}
```

`"tray": false` is for a session that would rather run waybar's tray, or one
that wants none at all. It takes effect on reload as well as at startup: the
bus names are released, the bar empties, and applications see the tray go away
exactly as they would if this program had exited. Turning it back on claims
them again.

Even with it on, a KDE session or a GNOME extension that already holds the
watcher name keeps it — the name is queued for, never taken, and the log says
which happened. What that means in practice is that running this beside another
tray is not a conflict, it is a decision made by whichever started first.

`icon_theme` is the theme an item's icon name is resolved against. There is no
way to ask a Wayland session what its icon theme is — GTK keeps it in dconf and
Qt in an ini file, and neither is something a compositor should be reading — so
it is named here. An item that ships its own icons and points at them with
`IconThemePath` is searched there first, whatever this says, and applications
that publish raw pixmaps rather than names need no theme at all.

Clicks: left activates, right opens the menu, middle is the secondary action,
and the wheel scrolls — which is what a volume applet in a tray expects. An
item that publishes a `com.canonical.dbusmenu` object has its menu read by the
compositor and drawn by the shell, so it is styled by the same stylesheet as
the rest of the desktop; one that implements `ContextMenu` instead is asked to
draw its own window, as it would be under any other tray. Submenus open in
place rather than flying out beside the menu — everything the shell draws over
a window is one rectangle it has to name to the compositor, and a panel outside
that rectangle would be drawn behind the window it is meant to be over. See
[`docs/protocols.md`](protocols.md#the-system-tray).

Where the tray sits on the bar is the shell's business, like every other bar
item: it is `tray` in `bar_items`, and it draws nothing at all when there is
nothing registered.

## The notification centre

On by default, keeping the last 50 notifications.

A notification is a popup and then it is nothing. One that arrived over a
fullscreen game, or while the screens were blanked, or in the ninety seconds
somebody was making coffee, was never seen and cannot be gone back to — which
is what every desktop's notification centre is for, and what a second daemon is
usually installed to keep. The compositor is already that daemon: it claims
`org.freedesktop.Notifications`, so the only copy that ever existed was already
in this process on its way to the shell.

`Mod4+Shift+m` opens the centre. It is drawn by the shell, so it is styled by
the stylesheet already open in the editor, like the popups themselves. A row's
✕ forgets that notification, the footer forgets all of them, and a row whose
application offered buttons still has them: pressing one tells the application
exactly what pressing it on the popup would have.

What stays and what goes:

| | |
| --- | --- |
| the popup timed out | kept — this is the case the centre exists for |
| dismissed with its ✕ | kept: the popup went, which is not the same as dealt with |
| an action was pressed | gone — a mail opened is not something to go back to |
| the application withdrew it | gone: it says the message is no longer true |
| forgotten from the centre | gone, and the application is not told |

The record is the compositor's rather than the shell's, and deliberately: the
shell is a web page, restarted when it crashes and reloaded when its stylesheet
changes, and a history kept there would be lost by both. It is memory only —
nothing is written to disk, so a compositor that exits takes the list with it.

`"history": 0` turns it off: popups are drawn and nothing is kept, which is
what a session that would rather not hold a list of everything that notified it
asks for. The setting applies on reload, and lowering it drops the oldest
entries there and then rather than waiting for them to age out.

## The clipboard history

On by default, keeping the last 25 things copied.

A Wayland selection is not a buffer anywhere: it is an offer from the client
that owns it, and the data exists only while that client is running. Close the
terminal you copied from and the clipboard is empty — which is why every
desktop grows a clipboard manager, and why one is usually a second daemon
holding a `wlr-data-control` connection open. The compositor is already that
daemon: every selection on the session passes through it, so keeping the last
few is reading what is already being offered.

```jsonc
{
  "clipboard_history": 25   // 0 keeps nothing at all
}
```

`Mod4+Shift+v` opens the picker, which the shell draws — so it is styled by the
stylesheet already open in the editor, like the notifications. Choosing an
entry puts it back on the clipboard with the compositor as the selection's
owner, which is the whole point: the application that copied it may have exited
hours ago. A row's ✕ forgets that entry and the footer forgets everything,
which is what somebody asks for after copying a password.

Text only, and only the clipboard. Recording the primary selection would mean
an entry for every word dragged over with a mouse, and images are megabytes
each with nowhere to draw them in a list of lines. Entries are capped at 256
KiB — a clipboard holds what a person copied, and what a person copied fits on
a screen.

`"clipboard_history": 0` turns it off: nothing is read and nothing is kept, for
a session that would rather run cliphist or one that does not want a copy of
every password that passes through the clipboard. The setting applies on
reload, and turning it off empties what was already there.

## Wi-Fi and Bluetooth

`Mod4+Shift+n` opens the Wi-Fi picker and `Mod4+Shift+t` the Bluetooth one;
clicking the bar's `net` module opens the first as well. Both are drawn by the
shell — so they are styled by the stylesheet already open in the editor, like
the notifications — and both are answered by the compositor, because the page
has no bus and NetworkManager and BlueZ are both on it.

There is nothing to configure and no daemon to add. Whatever already manages
the machine's networking is what these talk to: a network joined here is a
NetworkManager connection that `nmcli` and every other applet can see, and the
passphrase goes into NetworkManager's own secret store rather than anywhere in
this compositor.

Neither radio is touched until a picker is opened. Reading the access point
list is several bus round trips per access point and a scan is the radio
transmitting, so the compositor connects to NetworkManager on the first press
and stops reading the moment the picker closes; the Bluetooth picker starts a
discovery when it opens and stops it when it closes. A picker left open is a
radio left scanning, which is why a click anywhere else takes both down.

In the Wi-Fi picker, a row is a network rather than an access point — a mesh
publishing one name from three radios is one row — and clicking it does the
obvious thing: leave the one in use, join one that is already saved or open,
ask for a passphrase for one that is neither. The passphrase box is a real
text field, and the shell asks the compositor for the keyboard while it is up
(`shell.focus`) and hands it back to the window that had it afterwards. An
enterprise network is named as one and not offered a box: it wants a
certificate and an identity, which is `nmcli` work.

In the Bluetooth picker, tapping a device connects it — pairing and trusting
first when it has never been paired, so it comes back on its own next time —
and tapping a connected one disconnects it. The ✕ on a paired device forgets
it, which is the way out of a pairing that went wrong.

Pairing needs an agent, which is the piece `bluetoothctl` makes you type
`agent on` for. The compositor registers one the first time something is
paired, with the `NoInputNoOutput` capability — there is no dialog here to
show a six-digit code in — which is the mode every headset, mouse and speaker
uses. It is deliberately *not* made the session's default agent: that would
also answer incoming pairing requests, and answering those automatically is
how a stranger pairs with your laptop. A device that insists on a displayed
code cannot be paired from here; BlueZ says so and the picker shows what it
said.

## The on-screen keyboard

It is always available, the way the clipboard picker and the two radios above
are — `Mod4+Shift+k` raises and lowers it by hand regardless of anything
below, for a desk that has a keyboard already and wants to test it, or to type
into something that never asked. What `"osk"` in the config file controls is
only whether it also comes up *on its own*, when the focused client enables a
`zwp_text_input_v3` — the protocol's own way of saying a text field wants
typing into — and goes back down when that stops being true.

* `"auto"`, the default, does that, but only once the seat has actually seen a
  touch device: a desk with just a keyboard and a mouse never has, so on one
  of those it behaves exactly like `"manual"` below without a config file
  having to say so. This is what a touch panel wants and a desktop with a
  hardware keyboard does not, without either one having to name itself —
  raising a keyboard nobody asked for on top of whatever is being typed into
  with a real one is the problem this exists to avoid.
* `"manual"` never raises it on its own; `Mod4+Shift+k` is the only way it
  comes up. For a desk that would rather decide every time, or that hits the
  rare window a hardware keyboard cannot reach at all — a login prompt drawn
  before the session's own input method exists, say.
* `"off"` does neither. `Mod4+Shift+k` is bound to nothing else while this is
  set, so pressing it does exactly nothing — a deliberate consequence of
  asking for the keyboard gone entirely, not a chord left unfinished. Changing
  the setting takes effect immediately on a reload, including hiding a
  keyboard that was already pinned open by hand.

It types by pressing keys, not by handing text to the client directly. This
compositor has both paths wired up — a fork of smithay's
`zwp_text_input_v3`/`zwp_input_method_v2` machinery, with an escape hatch that
lets the compositor stand in for a real input method, and `inject_keysym`,
the same call remote-desktop keyboard injection already goes through — and it
uses the second. `commit_string` only reaches a client that has bound
text-input and enabled it, which every toolkit does for a real text field but
a terminal emulator or a login prompt typically does not, so a keyboard that
only worked that way would go silent on exactly the desks that most need one.
Key presses reach anything with a keyboard, the ordinary way. See the doc
comment on `Request::OskKey` in `crates/viewport-ipc/src/request.rs` for the
whole of that reasoning, including how Shift and Caps Lock are made to work
over a path that can only press keys rather than glyphs.

Held keys repeat the way a hardware key does — the seat's own repeat, from
`repeat_rate` and `repeat_delay` above, and nothing timed by the keyboard
itself — because a tap is `pressed: true` immediately followed by
`pressed: false`, and a key held down is `true` once and `false` on release,
exactly like a real one.

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
| `Mod4+Tab` / `Mod4+Shift+Tab` | step through every window on the strip, wrapping at its ends — including the columns scrolled out of view |
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

**`matrix`** — the focused window at 60% of the width on the left, and the rest
of your focus history halving away down the right: the window you were in
before this one takes half the column, the one before that a quarter, and so on
until a slot would be shorter than 100px. Everything past that point is stacked
in the last slot with a count badge, most recently used on top.

The order is Alt+Tab order, so nothing is ever laid out by where it happens to
sit in the tree — the window you keep returning to keeps a large slot, and the
one you have not touched since this morning sinks to the stack. Focusing a
window promotes it to the primary and pushes the one it displaced to the top of
the column.

It has no keys of its own and no resize mode: focus is the only input the model
takes, so `Mod4+h/j/k/l`, `Mod4+Tab` and clicking a window are the whole
interaction. Its ratios are in [matrix.md](matrix.md).

**`canvas`** — every workspace an unbounded plane. Windows sit at coordinates
you gave them and keep them; the view pans and zooms over the top. Nothing an
already-placed window does moves any other, opening one never reflows what is
open, and zooming out draws windows smaller without resizing them — no client
is asked to relayout itself for any view change.

| Key | Canvas layout |
| --- | --- |
| `Mod4+bracketleft` / `Mod4+bracketright` | pan left / right |
| `Mod4+Prior` / `Mod4+Next` | pan up / down |
| `Mod4+minus` / `Mod4+equal` | zoom out / in |
| `Mod4+Shift+f` | fit the whole plane on screen |
| `Mod4+Home` | back to 1:1 on the focused window |
| `Mod4+r` | size the focused window to the screen, less the gaps, without fullscreen |
| `Mod4+Shift+h/j/k/l` | move the focused window across the plane |
| `Mod4+Tab` / `Mod4+Shift+Tab` | step through every window on the plane, panning the view onto each — including the ones parked out of sight |
| `Mod4` + left drag | move a window, or pan the plane when the drag starts on the desktop |
| `Mod4` + right drag | resize a window, from the corner nearest where the drag started |

Zoom stops at 1:1 because past it the compositor would be enlarging a buffer
the client painted smaller, and the way round that is to reconfigure every
client on every step — the resize storm the layout exists to avoid. Zooming out
is unrestricted and fully usable, pointer included. The whole model is in
[canvas.md](canvas.md).

All five models share one tree — the strip's columns and the orbits' order are
both the workspace root's children — so switching `layout` and reloading
rearranges what is open rather than discarding it. `shell layout.model`
switches at runtime, with a name or with no argument to cycle:

```json
"binds_override": { "Mod4+Shift+m": "shell layout.model" }
```

Adding a sixth model is [docs/layout-extension.md](layout-extension.md).


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
| `grid` | every window the same size, in rows. Four come out 2x2 on an ordinary screen, nine 3x3, and the row count follows the shape of the monitor |

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
monitor rotating under `bsp` or `grid`, both of which read the shape of the
screen: one cuts along whichever side is longer, the other picks the row count
whose cells come out closest to square. Every dynamic tiler behaves this way,
and it is why `manual` is still the default.

None of this touches `"layout": "scrolling"`, `"layout": "solar"`,
`"layout": "matrix"` or `"layout": "canvas"`, each of which is its own model.
Solar and the matrix in particular have no sub-arrangements: where a window
goes is decided by its position in the order and nothing else — and on the
canvas by nothing but where you put it.


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


## Pixel format

How many bits per colour channel the buffers that reach the screen carry.

```json
"pixel_format": "10"
```

`--pixel-format 10` and `VIEWPORT_PIXEL_FORMAT=10` say the same thing; the flag
wins over the variable, and the variable over the file. `8`, `10` and `auto`
are the values, and `8bit` / `10-bit` are accepted for whoever types them.

| Value | What it asks for |
| --- | --- |
| `auto` | Ten bits where the display takes it, eight where it does not. The default, and what every release before this one did |
| `10` | Ten bits and nothing else — an output whose plane will not take a ten-bit format **does not come up** |
| `8` | Eight bits, whatever the display can do |

**`10` is deliberately unforgiving.** It exists to answer "am I actually
getting ten bits", which `auto` cannot: `auto` falls back silently, so a
display quietly serving eight bits looks exactly like one serving ten. A dark
screen is the answer, and so is the line each output logs as it comes up:

```
DP-1: 2560x1440 at x=0, Abgr2101010
```

That format is what the display agreed to, not what was asked for, so it is
worth reading even under `auto`.

Eight bits is worth choosing for the opposite reason: a ten-bit buffer is
wider, so it costs more bandwidth to scan out and more memory to composite, and
on a display that only shows eight of the bits that cost buys nothing. HDR
needs more than eight bits per channel, though, so `"pixel_format": "8"` and an
output's `hdr` are working against each other.

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

**A monitor that comes back goes back where it was.** A connector reappearing —
a display waking from its own standby, a cable replugged, a KVM switching over —
is a brand new output to the scan that finds it: it would be mapped to the right
of everything else, in whatever order the connectors are enumerated, turned the
way it left the factory. Two identical panels switched off overnight therefore
came back swapped, and a rotated one came back landscape.

So the position, rotation, scale and mode of every output are remembered by
connector name, and put back after each scan. What is remembered is what was
asked for — by this block, by `output.configure`, or by wlr-output-management,
which is what `wlr-randr` and a settings app's display panel use — and the last
of those wins, so moving a monitor with any of them is not undone by the next
unplug. This block is applied after the restore, so a position written here
still has the final say. A mode is only restored if the display advertises it,
since the connector is the only identity there is and what comes back on a port
need not be the panel that left it.
