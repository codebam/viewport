# User wallpaper extensions

The shipped shell can draw the desktop background with a local script of your
own, without edits to `data/shell/`. It is the same shape as a
[user layout extension](layout-extension.md) or a
[custom bar widget](custom-widgets.md): a script the compositor never parses,
loaded by the shell because the config pointed at it — applied to the wallpaper
instead of the tiling tree or the bar.

Because the script renders **in the shell** (ordinary DOM and CSS), it works
under every compositor renderer, **Vulkan included**. There is no GPU shader
path to fall out of: a wallpaper extension is a web page drawing its own
background.

## The manifest and selecting one

Declare each script under the name it registers, then select that name with
`wallpaper_extension`:

```json
{
  "wallpaper_extensions": {
    "gradient": "wallpapers/gradient.js"
  },
  "wallpaper_extension": "gradient",
  "wallpaper_options": {
    "colors": ["#0f172a", "#1e3a8a", "#7c3aed", "#db2777", "#f59e0b"],
    "angle": 120,
    "duration": 24
  }
}
```

Paths are local files. Relative paths resolve beside the config file; `~/` and
absolute paths also work. Remote URLs are rejected: this is code the shell will
run inside the desktop, and a URL is a way to run somebody else's. The
compositor validates the manifest, resolves each path to a `file://` URL, and
sends the resolved list to the shell in the `config` event as
`wallpaper_extensions: [{name, url}]`. A path that does not exist is still sent
through, so the shell ends up with a name nothing registered: the wallpaper
falls back to the shell's own gradient rather than aborting the compositor, the
same bargain a missing layout script strikes.

Names may contain ASCII letters, digits, `_` and `-`.

While a wallpaper extension is selected on an output it **replaces** the
picture/film/colour wallpaper *on that output*: the two are never both in force
on one screen. Setting `wallpaper_extension` to `""` (or removing it) takes the
extension down and returns to the ordinary `wallpaper` path.

## Per-output wallpapers

`wallpaper_extension` is the page-level default: it applies to every output.
Each output can override it in the `outputs` block, so a multi-monitor desk can
run one continuous scene or several independent ones:

```json
{
  "wallpaper_extensions": { "gradient": "wallpapers/gradient.js" },
  "wallpaper_extension": "gradient",
  "outputs": {
    "DP-1": {
      "wallpaper_extension": "gradient",
      "wallpaper_options": { "colors": ["#0f172a", "#7c3aed"] }
    },
    "DP-3": {
      "wallpaper_extension": "gradient",
      "wallpaper_options": { "colors": ["#0f172a", "#db2777"], "duration": 30 }
    }
  }
}
```

An output that names no `wallpaper_extension` follows the page-level default.
An output whose `wallpaper_extension` is `""` runs no extension, so the ordinary
picture wallpaper shows there even while its neighbours run a scene. The
per-output `wallpaper_options` replaces the page-level options for that output
(it is not merged). The compositor carries the overrides to the shell in the
`config` event as `output_wallpapers: [{name, extension, options}]`.

The extension is applied to the output's own `.desktop` element, so `root` (see
below) is that element, not the document — a script paints one monitor and need
not know about the others.

## The OLED drive-budget guard

On an OLED panel, burn-in is exposure — light × time in one place — so a
wallpaper that holds a bright frame in the same pixels ages them. The shell can
check an extension against the panel-drive budgets the
[`wallpapers/oled/`](https://github.com/codebam/nixos) art is built to:

| metric    | meaning                              | budget   |
|-----------|--------------------------------------|----------|
| `apl`     | mean linear drive over the frame     | < 8%     |
| `drive50` | fraction of pixels over half drive   | < 0.8%   |
| `max`     | brightest pixel (sRGB)               | ≤ 0.95   |
| `over25`  | fraction over 25% grey (a wash)      | < 62%    |

Drive is measured in **linear** light, not sRGB: sRGB 0.5 is linear 0.214, so a
frame that looks half-bright is really a fifth of the drive.

`wallpaper_oled_guard` sets what happens when an applied extension is sampled
and a frame blows a budget:

```json
{ "wallpaper_oled_guard": "warn" }
```

- `warn` (the default) — log the offending frame and its metrics; the wallpaper
  stays up.
- `refuse` — log it and take the extension down, returning to the ordinary
  wallpaper.
- `off` — do not sample.

The sample is a still of the current frame, rasterised through an SVG
`foreignObject` with the computed styles inlined (the only way to read back what
CSS painted), at a bounded resolution so the cost stays small. It is taken once
when the extension is applied, not per frame. An animated wallpaper is measured
at one moment of its drift; a scene built to the budgets above passes at every
moment. The measurement lives in `data/shell/panel.js` and is shared with the
panel-drive bar widget.

## The panel-drive bar widget

The same measurement can sit on the bar as a live reading. Name `panel` in
`bar_items` to add it:

```json
{ "bar_items": ["net", "cpu", "memory", "panel", "clock"] }
```

The module shows the wallpaper's current mean linear drive as a percentage
(`󰍹 2.8%`); the tooltip carries the peak, the fraction over half drive and the
wash. It samples each output's wallpaper every five seconds, and only while a
bar actually carries the module — a desk that never asks for it pays nothing.
Unlike the cpu/memory modules, which the compositor feeds, this one feeds itself
from the shell's own rendering, so it reflects whatever the wallpaper extension
is drawing right now. It is a still of the current moment, refreshed on the
interval, not a per-frame meter.

## Writing one

A script registers exactly one wallpaper with `registerWallpaper`:

```js
registerWallpaper('gradient', {
  // Draw the background. `root` is the output's own `.desktop` element;
  // `options` is wallpaper_options, passed through untouched (an empty object
  // when absent).
  apply({ root, options }) {
    const style = document.createElement('style');
    style.id = 'my-wallpaper-style';
    style.textContent = `
      .my-wallpaper {
        background-image: linear-gradient(120deg, #1e3a8a, #7c3aed) !important;
      }
    `;
    document.head.appendChild(style);
    root.classList.add('my-wallpaper');
  },

  // Optional. Undo whatever apply set up; called when the wallpaper is taken
  // away or replaced.
  clear({ root }) {
    root.classList.remove('my-wallpaper');
    document.getElementById('my-wallpaper-style')?.remove();
  },
});
```

`apply` is the only required member. `root` is the output's `.desktop`, so
target `.<your-class>` — the class you add to `root` — to paint that monitor's
background. That is where the ordinary wallpaper lands, so you override it
cleanly; add `!important` if you need to beat a fitting class. If two outputs
may run your extension with different options, name the class, stylesheet and
keyframes per output (the demo keys them on `root.dataset.output`) so they do
not fight over one rule. Inject your own stylesheet and remove it in `clear` so
the extension is self-contained and leaves nothing behind.

A working demo lives at [`examples/wallpapers/gradient.js`](../examples/wallpapers/gradient.js):
an animated CSS gradient that slowly shifts across the desktop, with
`colors`, `angle` and `duration` options and a `prefers-reduced-motion`
fallback.

## Lifecycle

Extension scripts load in name order when the `config` event arrives, before
the selected one is applied. Changing the manifest or a script takes effect on
the normal shell reload; a removed entry becomes unavailable immediately. One
name has one owner for the life of the page — a script that claims a name
already registered is a mistake worth a throw rather than a silent takeover.
