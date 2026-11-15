/* OLED panel-drive measurement: the numbers the wallpapers/oled/ art is built
 * to, made available to the shell so a wallpaper extension can be checked
 * against them and a widget can show what the desk is costing the panel.
 *
 * Burn-in is exposure — light × time in one place — and the unit that ages a
 * subpixel is emitted light, which is LINEAR. Pixel values are sRGB, which is
 * not: sRGB 0.5 is linear 0.214. So every frame is measured in linear drive:
 *
 *   apl      mean linear drive over the frame — the light emitted
 *   drive50  fraction of pixels over half drive — a pixel running hard
 *   max      brightest pixel in sRGB — whether the frame reads at all
 *   over25   fraction over 25% grey — a wash lights the whole panel
 *
 * The budgets are the ones the baked pool keeps (see wallpapers/oled/README in
 * the nixos config): apl < 8%, drive50 < 0.8%, peak 0.40..0.95, over25 < 62%.
 *
 * Sampling rasterises the live DOM through an SVG foreignObject — the only way
 * to read back what CSS actually painted — with the computed styles inlined
 * property by property. (getComputedStyle().cssText is empty in some engines,
 * CEF among them, so it cannot be used.) The result is a still of the current
 * frame; an animated wallpaper is sampled again to see another moment.
 */

/* The drive budgets a wallpaper should keep. `max` is a ceiling here; the
 * "reads at all" floor is reported separately, not treated as a violation. */
const OLED_BUDGETS = { apl: 0.08, drive50: 0.008, max: 0.95, over25: 0.62 };

/* Sampling is a one-off read, but a 4K panel is a lot of pixels to walk; cap
 * the longest side so the cost stays bounded. The metrics are fractions, so a
 * smaller sample is representative. */
const OLED_SAMPLE_MAX = 1024;

function srgbToLinear(c) {
  c /= 255;
  return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

/* The four metrics from raw RGBA bytes. */
function measureImageData(data, width, height) {
  let sum = 0, over50 = 0, over25 = 0, max = 0;
  const n = width * height;
  for (let i = 0; i < data.length; i += 4) {
    const r = data[i], g = data[i + 1], b = data[i + 2];
    const l = 0.2126 * srgbToLinear(r) + 0.7152 * srgbToLinear(g)
      + 0.0722 * srgbToLinear(b);
    sum += l;
    if (l > 0.5) over50++;
    if ((r + g + b) / 3 / 255 > 0.25) over25++;
    const mx = Math.max(r, g, b) / 255;
    if (mx > max) max = mx;
  }
  return { apl: sum / n, drive50: over50 / n, over25: over25 / n, max };
}

/* Which budget lines a measurement crosses, as human-readable reasons. Empty
 * means the frame is within budget. */
function oledBudgetViolations(m) {
  const v = [];
  if (m.apl >= OLED_BUDGETS.apl) {
    v.push(`mean drive ${(m.apl * 100).toFixed(1)}% >= 8%`);
  }
  if (m.drive50 >= OLED_BUDGETS.drive50) {
    v.push(`pixels over half drive ${(m.drive50 * 100).toFixed(2)}% >= 0.8%`);
  }
  if (m.max > OLED_BUDGETS.max) {
    v.push(`peak ${m.max.toFixed(2)} > 0.95`);
  }
  if (m.over25 >= OLED_BUDGETS.over25) {
    v.push(`wash ${(m.over25 * 100).toFixed(0)}% >= 62%`);
  }
  return v;
}

/* Copy the live computed styles onto a clone, property by property. The clone
 * is detached, so its own computed style is empty — the styles have to come
 * from the live element, matched by document order (cloneNode preserves it). */
function inlineComputedStyles(liveEl, clone) {
  const src = [liveEl, ...liveEl.querySelectorAll('*')];
  const dst = [clone, ...clone.querySelectorAll('*')];
  for (let i = 0; i < src.length; i++) {
    const cs = getComputedStyle(src[i]);
    for (const prop of cs) dst[i].style.setProperty(prop, cs.getPropertyValue(prop));
  }
}

/* Rasterise a styled clone through a foreignObject and measure it. */
function rasterizeClone(clone, width, height) {
  return new Promise((resolve, reject) => {
    clone.style.position = 'relative';
    clone.style.left = '0';
    clone.style.top = '0';
    clone.style.width = width + 'px';
    clone.style.height = height + 'px';
    clone.style.margin = '0';
    clone.setAttribute('xmlns', 'http://www.w3.org/1999/xhtml');
    const xml = new XMLSerializer().serializeToString(clone);
    const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}">`
      + `<foreignObject width="100%" height="100%">${xml}</foreignObject></svg>`;
    const img = new Image();
    img.onload = () => {
      try {
        const cv = document.createElement('canvas');
        cv.width = width;
        cv.height = height;
        const ctx = cv.getContext('2d');
        ctx.drawImage(img, 0, 0, width, height);
        const data = ctx.getImageData(0, 0, width, height).data;
        resolve(measureImageData(data, width, height));
      } catch (error) {
        reject(error);
      }
    };
    img.onerror = () => reject(new Error('could not rasterise the wallpaper'));
    img.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(svg);
  });
}

/* Measure one element as it currently renders. */
function sampleElement(el, width, height) {
  const clone = el.cloneNode(true);
  inlineComputedStyles(el, clone);
  return rasterizeClone(clone, width, height);
}

/* The shell's own children that are not wallpaper: windows, the bar and the
 * rest of the furniture. Stripped from the sample so the measurement is the
 * background alone, not the windows drawn over it. The film element goes too —
 * a video frame cannot be read back through a foreignObject. */
const WALLPAPER_SAMPLE_STRIP = [
  '.windows', '.bar', '.notifications', '.empty', '.workspaces',
  '.taskbar', '.mode', '.status-osd', '.wallpaper-media',
];

/* Measure an output's wallpaper: its desktop element with the shell furniture
 * removed, at a bounded sample size. */
function sampleWallpaper(rootEl) {
  const cw = rootEl.clientWidth || rootEl.offsetWidth || 1;
  const ch = rootEl.clientHeight || rootEl.offsetHeight || 1;
  const scale = Math.min(1, OLED_SAMPLE_MAX / Math.max(cw, ch));
  const width = Math.max(1, Math.round(cw * scale));
  const height = Math.max(1, Math.round(ch * scale));
  const clone = rootEl.cloneNode(true);
  inlineComputedStyles(rootEl, clone);
  for (const sel of WALLPAPER_SAMPLE_STRIP) {
    for (const node of clone.querySelectorAll(sel)) node.remove();
  }
  return rasterizeClone(clone, width, height);
}
