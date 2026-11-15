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

While a wallpaper extension is selected it **replaces** the picture/film/colour
wallpaper: the two are never both in force. Setting `wallpaper_extension` to
`""` (or removing it) takes the extension down and returns to the ordinary
`wallpaper` path.

## Writing one

A script registers exactly one wallpaper with `registerWallpaper`:

```js
registerWallpaper('gradient', {
  // Draw the background. `root` is the document element the wallpaper
  // properties land on; `options` is wallpaper_options, passed through
  // untouched (an empty object when absent).
  apply({ root, options }) {
    const style = document.createElement('style');
    style.id = 'my-wallpaper-style';
    style.textContent = `
      html.my-wallpaper body,
      html.my-wallpaper .desktop {
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

`apply` is the only required member. Target `html.<your-class> body` and
`html.<your-class> .desktop` to paint the background — that is where the
ordinary wallpaper lands, so you override it cleanly. Add `!important` if you
need to beat a fitting class. Inject your own stylesheet and remove it in
`clear` so the extension is self-contained and leaves nothing behind.

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
