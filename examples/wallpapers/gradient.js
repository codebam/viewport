/* A wallpaper extension: an animated CSS gradient that slowly shifts across
 * the desktop. It renders entirely in the shell (a stylesheet + a class), so
 * it works under every compositor renderer — Vulkan included — with no GLSL
 * and no GPU shader path.
 *
 * Enable it in the config file:
 *
 *   {
 *     "wallpaper_extensions": { "gradient": "wallpapers/gradient.js" },
 *     "wallpaper_extension": "gradient",
 *     "wallpaper_options": {
 *       "colors": ["#0f172a", "#1e3a8a", "#7c3aed", "#db2777", "#f59e0b"],
 *       "angle": 120,
 *       "duration": 24
 *     }
 *   }
 *
 * The path resolves beside the config file. `wallpaper_options` is passed
 * through untouched; every key is optional and falls back to a default.
 *
 * `root` is the output's own `.desktop` element, so the gradient paints that
 * monitor alone. Select it per output to give each screen its own scene:
 *
 *   "outputs": {
 *     "DP-1": { "wallpaper_extension": "gradient",
 *               "wallpaper_options": { "colors": ["#0f172a", "#7c3aed"] } },
 *     "DP-3": { "wallpaper_extension": "gradient",
 *               "wallpaper_options": { "colors": ["#0f172a", "#db2777"] } }
 *   }
 *
 * The class, stylesheet and keyframes are named per output so two monitors can
 * run different colours at different speeds without fighting over one rule.
 */
registerWallpaper('gradient', {
  /* Declared so the settings panel can draw these as real controls (see
   * docs/wallpaper-extension.md). `colors` is left to the config file — a list
   * of colours is not a single control. */
  options: [
    { key: 'angle', label: 'Angle', type: 'number', default: 120, min: 0, max: 360, step: 1 },
    { key: 'duration', label: 'Loop seconds', type: 'number', default: 24, min: 1, max: 240, step: 1 },
  ],

  apply({ root, options }) {
    const opts = options || {};
    const colors = Array.isArray(opts.colors) && opts.colors.length >= 2
      ? opts.colors
      : ['#0f172a', '#1e3a8a', '#7c3aed', '#db2777', '#f59e0b'];
    const angle = Number.isFinite(opts.angle) ? opts.angle : 120;
    const duration = Number.isFinite(opts.duration) && opts.duration > 0
      ? opts.duration : 24;

    /* One namespace per output (the page-level root has no output name), so
     * each monitor's rule, class and keyframes are its own. */
    const suffix = (root.dataset && root.dataset.output) || 'page';
    const cls = `wallpaper-gradient-${suffix}`;
    const styleId = `wallpaper-gradient-style-${suffix}`;
    const anim = `wallpaper-gradient-shift-${suffix}`;

    /* A gradient larger than the screen, whose position is animated: the
     * colours slide through one another in a loop. This is the plain-CSS way
     * to get a moving gradient — no @property / Houdini needed, so it runs in
     * every engine the shell uses. */
    let style = document.getElementById(styleId);
    if (!style) {
      style = document.createElement('style');
      style.id = styleId;
      document.head.appendChild(style);
    }
    style.textContent = `
      .${cls} {
        background-image: linear-gradient(${angle}deg, ${colors.join(', ')}) !important;
        background-size: 400% 400% !important;
        background-position: 0% 50%;
        animation: ${anim} ${duration}s ease-in-out infinite !important;
      }
      @keyframes ${anim} {
        0%   { background-position: 0% 50%; }
        50%  { background-position: 100% 50%; }
        100% { background-position: 0% 50%; }
      }
      @media (prefers-reduced-motion: reduce) {
        .${cls} { animation: none !important; }
      }
    `;
    root.classList.add(cls);
  },

  clear({ root }) {
    const suffix = (root.dataset && root.dataset.output) || 'page';
    root.classList.remove(`wallpaper-gradient-${suffix}`);
    const style = document.getElementById(`wallpaper-gradient-style-${suffix}`);
    if (style) style.remove();
  },
});
