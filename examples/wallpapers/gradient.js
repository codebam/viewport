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
 */
registerWallpaper('gradient', {
  apply({ root, options }) {
    const opts = options || {};
    const colors = Array.isArray(opts.colors) && opts.colors.length >= 2
      ? opts.colors
      : ['#0f172a', '#1e3a8a', '#7c3aed', '#db2777', '#f59e0b'];
    const angle = Number.isFinite(opts.angle) ? opts.angle : 120;
    const duration = Number.isFinite(opts.duration) && opts.duration > 0
      ? opts.duration : 24;

    /* A gradient larger than the screen, whose position is animated: the
     * colours slide through one another in a loop. This is the plain-CSS way
     * to get a moving gradient — no @property / Houdini needed, so it runs in
     * every engine the shell uses. */
    const style = document.createElement('style');
    style.id = 'wallpaper-gradient-style';
    style.textContent = `
      html.wallpaper-gradient body,
      html.wallpaper-gradient .desktop {
        background-image: linear-gradient(${angle}deg, ${colors.join(', ')}) !important;
        background-size: 400% 400% !important;
        background-position: 0% 50%;
        animation: wallpaper-gradient-shift ${duration}s ease-in-out infinite !important;
      }
      @keyframes wallpaper-gradient-shift {
        0%   { background-position: 0% 50%; }
        50%  { background-position: 100% 50%; }
        100% { background-position: 0% 50%; }
      }
      @media (prefers-reduced-motion: reduce) {
        html.wallpaper-gradient body,
        html.wallpaper-gradient .desktop {
          animation: none !important;
        }
      }
    `;
    document.head.appendChild(style);
    root.classList.add('wallpaper-gradient');
  },

  clear({ root }) {
    root.classList.remove('wallpaper-gradient');
    const style = document.getElementById('wallpaper-gradient-style');
    if (style) style.remove();
  },
});
