// WCAG 2.x text-contrast walker for the desktop UX audits (local #277, from audit 4 / #262).
//
// `contrastExpression(options)` returns ONE JavaScript expression for `page.eval` (headless Chrome, a WKWebView, any
// DOM). It walks every visible text node, takes the element's computed colour (its alpha composited over the
// background) and the nearest opaque background (translucent layers composited from the page up), and reports each
// element under the WCAG AA minimum: 4.5:1, or 3:1 for large text (>= 24 px, or >= 18.66 px and bold).
//
// The maths are pure functions in this file. The expression is built from their SOURCE (Function#toString), so the
// browser runs the very functions the unit tests run in Node: there is no second copy to drift.
//
// Limits (an audit reads them, never a gate): a text drawn over an image or gradient is judged against the colour
// under it, not the pixels; disabled controls and `inert` or hidden subtrees are exempt (WCAG 1.4.3 exempts inactive
// components); placeholder, ::selection and pseudo-element text are not text nodes and are not walked.

/** Parse a computed CSS colour (`rgb()`, `rgba()`, or `color(srgb r g b / a)`) to `{ r, g, b, a }` (0..255, a 0..1), or null. */
export function parseColor(text) {
  if (typeof text !== 'string') return null;
  const s = text.trim().toLowerCase();
  if (s === 'transparent') return { r: 0, g: 0, b: 0, a: 0 };
  const num = (v) => (v.endsWith('%') ? parseFloat(v) / 100 : parseFloat(v));
  let m = s.match(/^rgba?\(\s*([^)]+)\)$/);
  if (m) {
    const parts = m[1].split(/[\s,/]+/).filter(Boolean);
    if (parts.length < 3) return null;
    const ch = (v) => (v.endsWith('%') ? (parseFloat(v) / 100) * 255 : parseFloat(v));
    const out = { r: ch(parts[0]), g: ch(parts[1]), b: ch(parts[2]), a: parts[3] === undefined ? 1 : num(parts[3]) };
    return [out.r, out.g, out.b, out.a].every(Number.isFinite) ? out : null;
  }
  // `color-mix()` and `color(srgb …)` serialize this way in computed style.
  m = s.match(/^color\(\s*srgb\s+([^)]+)\)$/);
  if (m) {
    const parts = m[1].split(/[\s/]+/).filter(Boolean);
    if (parts.length < 3) return null;
    const out = { r: num(parts[0]) * 255, g: num(parts[1]) * 255, b: num(parts[2]) * 255, a: parts[3] === undefined ? 1 : num(parts[3]) };
    return [out.r, out.g, out.b, out.a].every(Number.isFinite) ? out : null;
  }
  return null;
}

/** `top` painted over an opaque `under`: the colour that reaches the screen. */
export function compose(top, under) {
  const a = top.a;
  return { r: top.r * a + under.r * (1 - a), g: top.g * a + under.g * (1 - a), b: top.b * a + under.b * (1 - a), a: 1 };
}

/** WCAG relative luminance of an opaque `{ r, g, b }` (0..255). */
export function luminance(c) {
  const lin = (v) => { const x = v / 255; return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4; };
  return 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
}

/** WCAG contrast ratio of two opaque colours, 1..21. */
export function ratioOf(a, b) {
  const la = luminance(a);
  const lb = luminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

/** The AA minimum for text of this computed size (px) and weight. */
export function neededRatio(fontPx, weight) {
  const large = fontPx >= 24 || (fontPx >= 18.66 && Number(weight) >= 700);
  return large ? 3 : 4.5;
}

/**
 * Judge one text: `fg` is the text colour, `layers` are the background colours from the element up to the root (each
 * may be translucent); the page falls back to white like a browser canvas. Returns `{ ratio, need, ok, fg, bg }`.
 */
export function judge(fg, layers, fontPx, weight) {
  let bg = { r: 255, g: 255, b: 255, a: 1 };
  for (let i = layers.length - 1; i >= 0; i--) if (layers[i] && layers[i].a > 0) bg = compose(layers[i], bg);
  const text = compose(fg, bg);
  const ratio = ratioOf(text, bg);
  const need = neededRatio(fontPx, weight);
  return { ratio: Math.round(ratio * 100) / 100, need, ok: ratio >= need, fg: text, bg };
}

/** `#rrggbb` of an opaque colour. */
export function hex(c) {
  return `#${[c.r, c.g, c.b].map((v) => Math.round(v).toString(16).padStart(2, '0')).join('')}`;
}

/** The pure functions the browser needs, in dependency order; the expression embeds their source. */
const SHARED = [parseColor, compose, luminance, ratioOf, neededRatio, judge, hex];

/**
 * A `page.eval` expression that resolves to `{ checked, failed, failures: [{ text, selector, fg, bg, ratio, need, px }] }`.
 * `root` scopes the walk (a CSS selector, default the whole body). `limit` caps the failures listed; `checked` and
 * `failed` still count every text.
 */
export function contrastExpression({ root = null, limit = 60 } = {}) {
  return `(() => {
    ${SHARED.map((f) => f.toString()).join('\n')}
    const scope = ${root ? `document.querySelector(${JSON.stringify(root)})` : 'document.body'};
    if (!scope) return { checked: 0, failures: [], missing: true };
    const selectorOf = (e) => {
      const bits = [];
      for (let n = e; n && n !== document.body && bits.length < 3; n = n.parentElement) {
        bits.unshift(n.id ? '#' + n.id : n.tagName.toLowerCase() + (typeof n.className === 'string' && n.className.trim() ? '.' + n.className.trim().split(/\\s+/)[0] : ''));
        if (n.id) break;
      }
      return bits.join(' > ');
    };
    const layersOf = (e) => {
      const out = [];
      for (let n = e; n; n = n.parentElement) {
        const c = parseColor(getComputedStyle(n).backgroundColor);
        if (c && c.a > 0) { out.push(c); if (c.a >= 0.999) break; }
      }
      return out;
    };
    const walker = document.createTreeWalker(scope, NodeFilter.SHOW_TEXT);
    const seen = new Set();
    const failures = [];
    let checked = 0;
    let failed = 0;
    while (walker.nextNode()) {
      const t = walker.currentNode;
      const e = t.parentElement;
      if (!e || seen.has(e) || !t.textContent.trim()) continue;
      if (['SCRIPT', 'STYLE', 'NOSCRIPT'].includes(e.tagName)) continue;
      if (!e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) continue;
      if (e.closest('[inert], [hidden], [aria-hidden="true"], :disabled, [aria-disabled="true"]')) continue;
      const r = e.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) continue;
      seen.add(e);
      const cs = getComputedStyle(e);
      const fg = parseColor(cs.color);
      if (!fg) continue;
      checked++;
      const j = judge(fg, layersOf(e), parseFloat(cs.fontSize), cs.fontWeight);
      if (j.ok) continue;
      failed++;
      if (failures.length < ${Number(limit) || 60}) {
        failures.push({ text: t.textContent.trim().replace(/\\s+/g, ' ').slice(0, 40), selector: selectorOf(e), fg: hex(j.fg), bg: hex(j.bg), ratio: j.ratio, need: j.need, px: parseFloat(cs.fontSize) });
      }
    }
    return { checked, failed, failures };
  })()`;
}

/** Run the walker in `page` (anything with `eval(expression)`). */
export function contrastAudit(page, options) {
  return page.eval(contrastExpression(options));
}
