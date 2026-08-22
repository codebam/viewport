/* SPDX-License-Identifier: MIT
 *
 * The cascade, for a test suite that has no browser.
 *
 * The shell's window frames are CSS and the harness beside this file stubs the
 * DOM, so it can prove that a window element ends up with `focused` on it and
 * nothing whatever about whether that class is what paints the border blue.
 * The class could be spelled one way in geometry.js and another in shell.css,
 * or a rule further down the file could quietly win, and every existing test
 * would still pass. Asserting that a class name is present is asserting that
 * the JavaScript agrees with itself.
 *
 * So this reads shell.css and answers the question a browser answers: given
 * this element, in this tree, which declaration wins for this property? That
 * means selector matching, specificity, source order, `!important`, inline
 * styles and `var()` — the parts of CSS that decide *which* declaration
 * applies.
 *
 * It is emphatically not a rendering engine. It computes no boxes, runs no
 * flexbox, resolves no lengths and paints nothing. `flex: 0 0 var(--gap)` on a
 * divider is checked to be the declaration in force and to resolve to `8px`;
 * that this produces an eight pixel gap on screen is flexbox's job and is not
 * checked here or anywhere else without a browser.
 *
 * Dependency-free on purpose: the shell job on CI is node and nothing else —
 * no npm install, no lockfile, no network.
 *
 * Elements are duck-typed against the harness's stub: `tagName`, `className`,
 * `parentElement`, `hidden`, `dataset` and a `style` object. A real DOM node
 * satisfies all of that too, so the same assertions would run unchanged inside
 * a browser if one ever became available.
 */
'use strict';

/* Everything the selectors in shell.css actually use. A selector containing
 * anything else matches nothing rather than being ignored, so an unsupported
 * construct shows up as a failing assertion instead of as a rule that silently
 * stopped being consulted. */
const PSEUDO_CLASSES = new Set(['root', 'hover', 'empty']);
const PSEUDO_ELEMENTS = new Set(['before', 'after']);

const BORDER_STYLES = new Set(['none', 'hidden', 'dotted', 'dashed', 'solid',
  'double', 'groove', 'ridge', 'inset', 'outset']);
const BORDER_WIDTHS = new Set(['thin', 'medium', 'thick']);
const SIDES = ['top', 'right', 'bottom', 'left'];

/* --- parsing ---------------------------------------------------------- */

function stripComments(text) {
  return text.replace(/\/\*[\s\S]*?\*\//g, '');
}

/* Split on a delimiter that is not inside brackets or quotes, which is what
 * separates `transition: a var(--x) 1ms, b var(--y) 1ms` into two layers
 * rather than into six pieces. */
function splitTop(text, sep) {
  const out = [];
  let depth = 0;
  let quote = null;
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quote !== null) {
      if (c === quote && text[i - 1] !== '\\') quote = null;
      continue;
    }
    if (c === '"' || c === '\'') { quote = c; continue; }
    if (c === '(' || c === '[') depth++;
    else if (c === ')' || c === ']') depth--;
    else if (depth === 0 && (sep === ' ' ? /\s/.test(c) : c === sep)) {
      out.push(text.slice(start, i));
      start = i + 1;
    }
  }
  out.push(text.slice(start));
  return out.map((s) => s.trim()).filter((s) => s !== '');
}

function matchBrace(text, open) {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === '{') depth++;
    else if (text[i] === '}' && --depth === 0) return i;
  }
  return text.length;
}

function matchParen(text, open) {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === '(') depth++;
    else if (text[i] === ')' && --depth === 0) return i;
  }
  return text.length;
}

/* camelCase is how a stub — and the DOM — spells an inline property that was
 * set through the object rather than through setProperty. Custom properties
 * keep their leading dashes and are already the name they will be looked up
 * by. */
function kebab(name) {
  if (name.startsWith('--')) return name;
  return name.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`);
}

/* Shorthands are expanded to longhands, because that is the only way a later
 * `border: 0` can be seen to undo an earlier `border-color`. The omitted parts
 * of a shorthand are written out at their initial values for the same reason:
 * in a browser `border: 0` resets the colour, and a model that left the colour
 * alone would report a fullscreen window as still having a blue frame.
 *
 * Only the shorthands the shell's window styling actually leans on are
 * expanded. Anything else is kept whole, which is accurate for a sheet that
 * does not also set its longhands — and shell.css does not. */
function expand(prop, value) {
  if (prop === 'border' || /^border-(top|right|bottom|left)$/.test(prop)) {
    const sides = prop === 'border' ? SIDES : [prop.slice('border-'.length)];
    let width = 'medium';
    let style = 'none';
    let color = 'currentcolor';
    for (const token of splitTop(value, ' ')) {
      const lower = token.toLowerCase();
      if (BORDER_STYLES.has(lower)) style = lower;
      else if (BORDER_WIDTHS.has(lower) || /^[\d.]/.test(token)) width = token;
      else color = token;
    }
    return sides.flatMap((side) => [
      [`border-${side}-width`, width],
      [`border-${side}-style`, style],
      [`border-${side}-color`, color],
    ]);
  }

  /* `outline` is `border`'s shape with one box instead of four. Expanded for
   * the same reason: the shell declares the focus ring as a shorthand and
   * asserts against `outline-style`, and a resolver that answered the empty
   * string for that would satisfy an assertion phrased as "there is no ring"
   * on a page that has one. `outline: none` is the style keyword alone, which
   * the loop below already reads as a style. */
  if (prop === 'outline') {
    let width = 'medium';
    let style = 'none';
    let color = 'currentcolor';
    for (const token of splitTop(value, ' ')) {
      const lower = token.toLowerCase();
      if (BORDER_STYLES.has(lower)) style = lower;
      else if (BORDER_WIDTHS.has(lower) || /^[\d.]/.test(token)) width = token;
      else color = token;
    }
    return [['outline-width', width], ['outline-style', style],
      ['outline-color', color]];
  }

  if (prop === 'border-width' || prop === 'border-style' ||
      prop === 'border-color') {
    const part = prop.slice('border-'.length);
    return box(value).map(([side, v]) => [`border-${side}-${part}`, v]);
  }

  if (prop === 'padding' || prop === 'margin' || prop === 'inset') {
    const name = (side) => prop === 'inset' ? side : `${prop}-${side}`;
    return box(value).map(([side, v]) => [name(side), v]);
  }

  if (prop === 'flex') {
    const parts = splitTop(value, ' ');
    /* One value is a grow factor and a zero basis; the two- and three-value
     * forms are grow, then shrink or basis, then basis. */
    if (parts.length === 1) return /^[\d.]+$/.test(parts[0])
      ? [['flex-grow', parts[0]], ['flex-shrink', '1'], ['flex-basis', '0']]
      : [['flex-grow', '1'], ['flex-shrink', '1'], ['flex-basis', parts[0]]];
    if (parts.length === 2) return /^[\d.]+$/.test(parts[1])
      ? [['flex-grow', parts[0]], ['flex-shrink', parts[1]],
        ['flex-basis', '0']]
      : [['flex-grow', parts[0]], ['flex-shrink', '1'],
        ['flex-basis', parts[1]]];
    return [['flex-grow', parts[0]], ['flex-shrink', parts[1]],
      ['flex-basis', parts[2]]];
  }

  /* A single-token background is a colour; a layered one is left whole rather
   * than guessed at, and is queried as `background`. */
  if (prop === 'background' && splitTop(value, ' ').length === 1 &&
      splitTop(value, ',').length === 1) {
    return [['background-color', value]];
  }

  return [[prop, value]];
}

/* The 1-to-4 value box notation: one value is every side, two are vertical and
 * horizontal, three leave the left to mirror the right. */
function box(value) {
  const v = splitTop(value, ' ');
  const [top, right, bottom, left] = v.length === 1
    ? [v[0], v[0], v[0], v[0]]
    : v.length === 2 ? [v[0], v[1], v[0], v[1]]
      : v.length === 3 ? [v[0], v[1], v[2], v[1]]
        : v;
  return [['top', top], ['right', right], ['bottom', bottom], ['left', left]];
}

function parseDeclarations(body) {
  const out = [];
  for (const piece of splitTop(body, ';')) {
    const colon = piece.indexOf(':');
    if (colon === -1) continue;
    const prop = piece.slice(0, colon).trim().toLowerCase();
    let value = piece.slice(colon + 1).trim();
    const important = /!\s*important$/i.test(value);
    if (important) value = value.replace(/!\s*important$/i, '').trim();
    if (prop === '') continue;
    for (const [name, v] of expand(prop, value)) {
      out.push({ prop: name, value: v, important });
    }
  }
  return out;
}

/* A compound selector: everything between two combinators. `supported` is
 * false when it contains something this file does not model, which makes the
 * whole rule match nothing. */
function parseCompound(text) {
  const compound = {
    tag: null, id: null, classes: [], attrs: [],
    pseudoClasses: [], pseudoElement: null, supported: true,
  };
  const token =
    /^(?:\*|[a-zA-Z][\w-]*|#[\w-]+|\.[\w-]+|\[[^\]]*\]|::?[\w-]+)/;

  let rest = text;
  while (rest !== '') {
    const m = token.exec(rest);
    if (m === null) { compound.supported = false; break; }
    const t = m[0];
    rest = rest.slice(t.length);

    if (t === '*') continue;
    else if (t.startsWith('::')) {
      const name = t.slice(2);
      if (!PSEUDO_ELEMENTS.has(name)) compound.supported = false;
      compound.pseudoElement = name;
    } else if (t.startsWith(':')) {
      const name = t.slice(1);
      if (!PSEUDO_CLASSES.has(name)) compound.supported = false;
      compound.pseudoClasses.push(name);
    } else if (t.startsWith('#')) compound.id = t.slice(1);
    else if (t.startsWith('.')) compound.classes.push(t.slice(1));
    else if (t.startsWith('[')) {
      const inner = t.slice(1, -1);
      const eq = inner.indexOf('=');
      compound.attrs.push(eq === -1
        ? { name: inner.trim(), value: null }
        : { name: inner.slice(0, eq).trim(),
          value: inner.slice(eq + 1).trim().replace(/^["']|["']$/g, '') });
    } else compound.tag = t.toLowerCase();
  }
  return compound;
}

function parseSelector(selector) {
  const parts = [];
  let rest = selector.trim();
  let combinator = null;
  while (rest !== '') {
    const m = /^[^\s>+~]+/.exec(rest);
    if (m === null) return null;
    parts.push({ compound: parseCompound(m[0]), combinator });
    rest = rest.slice(m[0].length);
    const c = /^\s*([>+~])\s*|^\s+/.exec(rest);
    if (c === null) break;
    combinator = c[1] ?? ' ';
    rest = rest.slice(c[0].length);
  }
  return parts;
}

function specificityOf(parts) {
  const spec = [0, 0, 0];
  for (const { compound } of parts) {
    if (compound.id !== null) spec[0]++;
    spec[1] += compound.classes.length + compound.attrs.length +
      compound.pseudoClasses.length;
    if (compound.tag !== null) spec[2]++;
    if (compound.pseudoElement !== null) spec[2]++;
  }
  return spec;
}

function collect(text, rules, media, counter) {
  let i = 0;
  while (i < text.length) {
    const open = text.indexOf('{', i);
    if (open === -1) break;
    const prelude = text.slice(i, open).trim();
    const close = matchBrace(text, open);
    const body = text.slice(open + 1, close);
    i = close + 1;

    if (prelude.startsWith('@')) {
      /* Only @media carries rules that can apply to an element. @keyframes
       * describes an animation's own timeline, which no element ever matches
       * against, so descending into it would produce rules whose selectors are
       * `from` and `to`. */
      if (/^@media\b/.test(prelude)) {
        collect(body, rules, prelude.replace(/^@media\s*/, ''), counter);
      }
      continue;
    }

    const declarations = parseDeclarations(body);
    const order = counter.n++;
    for (const selector of splitTop(prelude, ',')) {
      const parts = parseSelector(selector);
      if (parts === null) continue;
      rules.push({
        selector, parts, declarations, order, media,
        specificity: specificityOf(parts),
      });
    }
  }
}

/* --- matching --------------------------------------------------------- */

function classesOf(el) {
  return String(el.className ?? '').split(/\s+/).filter(Boolean);
}

function attributeOf(el, name) {
  if (name === 'hidden') return el.hidden === true ? '' : null;
  if (name.startsWith('data-')) {
    const key = name.slice(5).replace(/-(\w)/g, (_, c) => c.toUpperCase());
    return el.dataset?.[key] ?? null;
  }
  const own = el[name];
  return own === undefined || own === null || own === false
    ? null : String(own);
}

function matchCompound(compound, el, subject, ctx) {
  if (!compound.supported) return false;
  if (compound.tag !== null &&
      String(el.tagName ?? '').toLowerCase() !== compound.tag) return false;
  if (compound.id !== null && el.id !== compound.id) return false;

  const classes = new Set(classesOf(el));
  for (const c of compound.classes) if (!classes.has(c)) return false;

  for (const attr of compound.attrs) {
    const value = attributeOf(el, attr.name);
    if (value === null) return false;
    if (attr.value !== null && value !== attr.value) return false;
  }

  for (const name of compound.pseudoClasses) {
    if (name === 'root' && el !== ctx.root) return false;
    /* Hover is a state the caller asserts about the element it asked about,
     * since nothing in a stubbed DOM has a pointer over it. An ancestor
     * :hover is therefore never true, which is why `.thumb:hover .window`
     * would have to be asked for by hovering the thumb itself. */
    if (name === 'hover' && !(ctx.hover === el)) return false;
    if (name === 'empty' &&
        ((el.children ?? []).length > 0 || (el.textContent ?? '') !== '')) {
      return false;
    }
  }

  /* A pseudo-element is a different box from the element that originates it,
   * so a rule for one applies only when that box is what was asked about. */
  const wanted = subject ? (ctx.pseudo ?? null) : null;
  return (compound.pseudoElement ?? null) === wanted;
}

/* Right to left, the way a browser does it: the rightmost compound is the one
 * that has to match the element, and the rest are walked up the tree. */
function matchParts(parts, index, el, ctx) {
  if (!matchCompound(parts[index].compound, el, index === parts.length - 1,
    ctx)) return false;
  if (index === 0) return true;

  const combinator = parts[index].combinator;
  if (combinator === '>') {
    const parent = el.parentElement;
    return parent ? matchParts(parts, index - 1, parent, ctx) : false;
  }
  if (combinator === ' ') {
    for (let up = el.parentElement; up; up = up.parentElement) {
      if (matchParts(parts, index - 1, up, ctx)) return true;
    }
    return false;
  }
  /* Sibling combinators are not used by shell.css; a rule that grows one is
   * better failing loudly here than matching by accident. */
  return false;
}

/* --- the cascade ------------------------------------------------------- */

/* Author `!important` beats an inline declaration, which beats every ordinary
 * author rule however specific — the ordering `.window.fullscreen { inset: 0
 * !important }` depends on to win over the inline rect a floating window
 * carries. */
const TIER_RULE = 0;
const TIER_INLINE = 1;
const TIER_IMPORTANT = 2;

function beats(a, b) {
  if (a.tier !== b.tier) return a.tier > b.tier;
  for (let i = 0; i < 3; i++) {
    if (a.specificity[i] !== b.specificity[i]) {
      return a.specificity[i] > b.specificity[i];
    }
  }
  return a.order >= b.order;
}

function inlineDeclarations(el) {
  const style = el.style;
  if (!style) return [];
  const out = [];
  for (const key of Object.keys(style)) {
    const value = style[key];
    if (typeof value !== 'string' || value === '') continue;
    for (const [prop, v] of expand(kebab(key), value)) {
      out.push({ prop, value: v, important: false });
    }
  }
  return out;
}

class Sheet {
  constructor(rules, root) {
    this.rules = rules;
    this.root = root ?? null;
  }

  /* Every property in force on `el`, as a Map of property to winning value —
   * the value still holding its var() references, which `value()` resolves. */
  declarations(el, options = {}) {
    const ctx = {
      root: options.root ?? this.root,
      pseudo: options.pseudo ?? null,
      hover: options.hover === true ? el : null,
      media: options.media ?? [],
    };

    const winners = new Map();
    const consider = (decl, candidate) => {
      const held = winners.get(decl.prop);
      if (held === undefined || beats(candidate, held)) {
        winners.set(decl.prop, { ...candidate, value: decl.value });
      }
    };

    for (const rule of this.rules) {
      if (rule.media !== null && !ctx.media.includes(rule.media)) continue;
      if (!matchParts(rule.parts, rule.parts.length - 1, el, ctx)) continue;
      for (const decl of rule.declarations) {
        consider(decl, {
          tier: decl.important ? TIER_IMPORTANT : TIER_RULE,
          specificity: rule.specificity,
          order: rule.order,
        });
      }
    }

    /* An inline style belongs to the element rather than to a pseudo-element
     * of it, which is why ::after gets none of it. */
    if (ctx.pseudo === null) {
      for (const decl of inlineDeclarations(el)) {
        consider(decl, {
          tier: TIER_INLINE, specificity: [0, 0, 0], order: Infinity,
        });
      }
    }

    const out = new Map();
    for (const [prop, held] of winners) out.set(prop, held.value);
    return out;
  }

  /* The winning value for one property, with var() substituted. Empty string
   * when nothing sets it, which is a distinguishable answer: a test can assert
   * that `.split` has no `gap` at all. */
  value(el, prop, options = {}) {
    const held = this.declarations(el, options).get(prop);
    return held === undefined ? '' : this.resolve(held, el, options);
  }

  /* Custom properties inherit, so this is the same walk a browser does: the
   * nearest ancestor that sets one supplies it.
   *
   * The chain ends at the root explicitly. A stubbed tree is not rooted at
   * <html> — the harness builds each output under a detached container — so
   * without this the `:root` block, where every colour and the gap live, would
   * be unreachable from any window. */
  custom(el, name, options = {}) {
    for (let node = el; node; node = node.parentElement) {
      const held = this.declarations(node, { ...options, pseudo: null })
        .get(name);
      if (held !== undefined) return this.resolve(held, node, options);
    }
    if (this.root && el !== this.root) {
      const held = this.declarations(this.root, { ...options, pseudo: null })
        .get(name);
      if (held !== undefined) return this.resolve(held, this.root, options);
    }
    return '';
  }

  resolve(value, el, options, depth = 0) {
    if (depth > 16 || !value.includes('var(')) return value;
    const at = value.indexOf('var(');
    const end = matchParen(value, at + 3);
    const [name, ...rest] = splitTop(value.slice(at + 4, end), ',');
    const found = this.custom(el, name, options);
    const replacement = found !== ''
      ? found
      : this.resolve(rest.join(',').trim(), el, options, depth + 1);
    return this.resolve(
      value.slice(0, at) + replacement + value.slice(end + 1),
      el, options, depth + 1);
  }
}

function parse(text, options = {}) {
  const rules = [];
  collect(stripComments(text), rules, null, { n: 0 });
  return new Sheet(rules, options.root);
}

module.exports = { parse };
