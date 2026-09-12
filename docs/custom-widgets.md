# Custom bar widgets

The bar ships with a fixed set of modules and a handful of widgets it already
knows how to draw, but a user can add one of their own without editing
`data/shell/`. A local JavaScript file registers a named widget, the config
names that file and says where the widget goes, and the shell draws it beside
the built-ins. It is the same shape as a
[user layout extension](layout-extension.md): a script the compositor never
parses, loaded by the shell because the config pointed at it, applied to the
bar instead of the tiling tree.

A `bar_widgets` entry has always meant a widget the shipped shell knows how to
draw — `{ "type": "disk", "path": "/home" }`, `{ "type": "weather",
"location": "New York" }` — with an option set it already understands. A
**custom** widget is the third kind of entry: `{ "type": "custom", "name":
"world-clock" }` names a script instead of a type. The `options` object beside
it is passed through untouched, so the shell has no opinion about what is in
it; the script reads it. Nothing about the built-in widgets changes: you still
get `disk`, `weather` and the rest exactly as they were, and a custom widget is
simply another thing in the same list.

## The manifest and where the widget goes

Declare each script under the name it registers, then place that name as a
widget of type `custom`:

```json
{
  "widget_extensions": {
    "world-clock": "widgets/world-clock.js"
  },
  "bar_widgets": [
    { "type": "custom", "name": "world-clock",
      "options": { "zone": "Asia/Tokyo" } }
  ]
}
```

`bar_widgets` appends the widget after the shipped modules, exactly as it
appends a `disk` or `weather` entry. To put a custom widget anywhere in the
bar's ordered list — between the network and the clock, say — name it in
`bar_items` instead; a `custom` entry is the same object there and does not
care which list carried it. The two keys otherwise work as
[the configuration guide](configuration.md#overriding-the-whole-bar)
describes, and when both are present `bar_items` wins as it always did.

Paths are local files. Relative paths resolve beside the config file; `~/` and
absolute paths also work. Remote URLs are rejected: this is code the shell will
run inside the desktop, and a URL is a way to run somebody else's. The
compositor validates the manifest, resolves each path to a `file://` URL, and
sends the resolved list to the shell in the `config` event as
`widget_extensions: [{name, url}]`. A path that does not exist is still sent
through, so the shell ends up with a name nothing registered: the widget draws
nothing rather than aborting the compositor, the same bargain
[a missing layout script](layout-extension.md) strikes.

Names may contain ASCII letters, digits, `_` and `-`. A name may not be a
built-in widget type (`disk`, `weather`, `volume`, `mic`, `mpris`, `battery`,
`ai`) or a built-in module (`mode`, `tray`, `net`, `disk`, `cpu`, `load`,
`memory`, `clock`): those already mean something to the bar, and a script that
shadowed one would be a second thing wearing the same name.

Changing the manifest or a script takes effect on the normal shell reload. A
removed entry becomes unavailable to the bar immediately; a `custom` widget
still naming it draws nothing and logs an error.

## The script

A script registers exactly one widget:

```js
registerWidget('world-clock', {
  mount(el, ctx) { /* build the widget into `el` */ },
  update(el, ctx) { /* optional — after mount and on every status tick */ },
  destroy(el, ctx) { /* optional — before the element is removed */ },
});
```

`mount(el, ctx)` is required. It runs once, the first time the bar places the
widget, and is handed the element the shell made for it — a
`<span class="module widget">`, empty. The script builds its own markup inside
that span; the shell stamps the span with `data-widget="custom:<name>"`, so a
stylesheet can reach one widget by name, and owns its tooltip. The shell
fetches the manifest's scripts while it applies the bar, and mounts the widget
as soon as its script has run, so the widget does not wait for a status tick to
appear; a script still in flight leaves its slot empty for that instant rather
than holding up the rest of the bar.

`update(el, ctx)` is optional. It runs once immediately after `mount`, then on
every status tick — the same `status.update` cadence at which the built-in
modules re-read the machine — so a widget that only re-renders data needs no
timer of its own. It gets the same `el` and `ctx` as `mount`. Writing the same
text into `textContent` every tick dirties the element whether or not the
string changed, and a dirty element is a repaint, so a script that watches one
value should compare before it assigns.

`destroy(el, ctx)` is optional. It runs before the element is removed, whether
because the bar's list changed, a `bar_items` override took over, or the shell
is reloading. It is where a widget undoes anything `mount` did that outlives
the element — a timer, an observer, a live subscription. It may run while the
element is already off screen, and it should be safe to call once.

A mounted widget is held by its `name` and its `options`, so changing either in
the config is a different widget: the old one is destroyed and a fresh one
mounted, rather than updated in place. A widget that wants to react to a new
option without a remount should re-read `ctx.options` in `update`, which runs
on every tick anyway.

## The context

Every callback receives the same `ctx` object:

- `name` — the registered name, as a string. A widget drawn twice under two
  names can tell which one it is; otherwise it is there to be logged.
- `options` — the `options` object beside the config entry, or `{}` when the
  entry named none. It is never null, so `ctx.options.zone` is safe to read
  unguarded; a missing option is simply `undefined`.
- `get status()` — the latest `status.update` sample, the one the compositor
  most recently sent. Its keys are listed below.
- `send(message)` — an IPC message to the compositor, the same objects
  [`docs/ipc.md`](ipc.md) documents. This is how a widget changes something
  rather than only reporting it.
- `exec(command)` — shorthand for `send({ "type": "shell.exec", "command":
  command })`, which runs `command` on the host, down the same spawn path a
  keybinding's `exec` uses.
- `tick()` — asks the shell to re-run the custom widgets' `update` now (the
  same pass a status tick runs), for a script whose own work — a `fetch`, a
  timer — produced something new before the next tick would have come round.

`ctx.status` carries the sample as the compositor wrote it:

- `cpu`, `memory` — the machine's processor and memory use, as percentages;
  both are `-1` until there are two samples to compare.
- `load` — the one-minute load average.
- `net_rx`, `net_tx` — network throughput in bytes per second, as the last
  sample measured it.
- `disk_free`, `disk_total` — free and total bytes on the root filesystem.
- `mounts` — an array of `{ "path", "free", "total" }`, one entry per extra
  mount a `disk` widget asked about. It is absent when no `disk` widget is on
  the bar, because an unconfigured sample does no mount statistics at all.
- `volume`, `muted` — the default audio sink's level in `0.0..=1.0` and whether
  it is muted. `mic_volume`, `mic_muted` are the same pair for the default
  source (the microphone).
- `brightness` — the panel's brightness in `0.0..=1.0`.
- `osd` — present only on a sample answering a hardware-key change, naming the
  one that should get transient feedback.

A numeric level the compositor could not read is `-1`, never absent, so
`ctx.status.volume >= 0` is the test for "there is a number to show" — the
built-in audio widgets test the value the same way. A widget that reads
`ctx.status` alone needs no IPC and no host access; one that also asks the page
for something new uses the shell's own network reach. That is how the built-in
`weather` widget works: the page can `fetch()` from the network even where it
cannot read `/proc`.

## Lifecycle and errors

The shell loads every widget script named in the manifest, in name order, as
it applies the bar configuration, and looks the descriptor up afresh on each
render, so a script that registers only after the bar has already laid out
mounts on the next one. A script registers its widget while it is being
evaluated; anything it does outside a callback happens once, at load, and
anything it needs per tick belongs in `update`.

`registerWidget` throws when it is given something it cannot honour: an invalid
name or a descriptor missing `mount`, a name that belongs to a built-in, a name
already registered by an earlier script, or a script whose load is stale — one
still running against an older manifest. The shell catches that throw, logs it,
and carries on, so one bad script does not take the bar with it. A widget entry
of type `custom` whose name no script registered draws nothing and logs an
error, which is the same visible outcome as a manifest path that was already
missing when the compositor read it.

## A worked example

`data/widget-extensions/world-clock.js` is a complete widget: the time in an
IANA timezone named by `options.zone`, defaulting to the machine's own zone. It
builds one inner span in `mount` and rewrites that span's text in `update`,
which is the whole of the lifecycle for a widget whose only input is the clock:

```js
/* Example user widget: the time in an IANA timezone. Loaded only when a
   bar_widgets or bar_items entry of type "custom" names 'world-clock' and
   widget_extensions points that name at this file. */
registerWidget('world-clock', {
  /* Called once, when the bar first places the widget. The shell has already
     made the outer <span class="module widget">; build the widget's own
     markup inside it here. */
  mount(el) {
    const time = document.createElement('span');
    time.className = 'world-clock-time';
    el.appendChild(time);
  },

  /* Called right after mount, then on every status tick — about every two
     seconds, which is often enough for a clock. Re-reading ctx.options each
     tick means a reload picks up a changed zone with no state to reset. */
  update(el, ctx) {
    const time = el.querySelector('.world-clock-time');
    if (!time) return;
    const zone = ctx.options.zone
      || Intl.DateTimeFormat().resolvedOptions().timeZone;
    let text;
    try {
      text = new Intl.DateTimeFormat(undefined, {
        timeZone: zone,
        hour: '2-digit',
        minute: '2-digit',
        hourCycle: 'h23',
      }).format(new Date());
    } catch {
      /* Intl throws a RangeError for a zone it does not know. Show the bad
         name rather than a blank widget, and keep the error out of the bar. */
      text = `${zone} ?`;
    }
    if (time.textContent !== text) time.textContent = text;
  },
});
```

Placed with `{ "type": "custom", "name": "world-clock", "options": { "zone":
"Asia/Tokyo" } }`, it reads the zone from the config; with no `options` at all
it shows local time. The same file can be registered under a second name for a
second zone by adding another manifest entry and another bar entry.

## Trust

A widget script runs inside the shell page, with the shell's own power. It can
`send()` any IPC message the shell can send — including `shell.exec`, which
runs a command on the host — and `exec()` is only shorthand for exactly that.
It is user-authored, local-only code: the same trust level as a
[layout extension](layout-extension.md), and the same trust level as the config
file that points at it. There is no sandbox and no capability list; the file
path is the permission. A script you did not write is a script you should not
load, and a manifest path you cannot account for is worth reading before the
next reload.
