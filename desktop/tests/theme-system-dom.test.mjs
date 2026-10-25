// "Match system" and the per-theme color-scheme (local #274): a fresh profile follows prefers-color-scheme live,
// an explicit theme ignores the system, the picker offers "Match system" first, and the page sets color-scheme
// (and the --on-* text colours) from the theme it paints. DOM module against a minimal fake document and a
// fake matchMedia (no browser needed).
import { test } from 'node:test';
import assert from 'node:assert/strict';

const props = new Map();
const root = {
  dataset: {},
  style: { setProperty: (k, v) => props.set(k, v), removeProperty: (k) => props.delete(k), getPropertyValue: (k) => props.get(k) ?? '' },
};
function matchesSimple(n, sel) {
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)((?:\[[^\]]+\])*)$/i.exec(sel);
  if (!m) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  for (const c of m[2].split('.').filter(Boolean)) if (!n.className.split(/\s+/).includes(c)) return false;
  for (const a of m[3].match(/\[[^\]]+\]/g) ?? []) {
    const [, name, val] = /^\[([\w-]+)(?:="([^"]*)")?\]$/.exec(a) ?? [];
    if (name === 'open') { if (!n.open) return false; continue; }
    if (name.startsWith('data-')) {
      const key = name.slice(5).replace(/-(\w)/g, (_, c) => c.toUpperCase());
      if (!(key in n.dataset) || (val !== undefined && n.dataset[key] !== val)) return false;
      continue;
    }
    if (!(name in n.attrs) || (val !== undefined && n.attrs[name] !== val)) return false;
  }
  return true;
}
const matches = (n, selector) => selector.split(',').map((s) => s.trim()).some((sel) => matchesSimple(n, sel.split(/\s+/).at(-1)));

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.listeners = {}; this._text = ''; this.className = ''; this.attrs = {}; this.parent = null;
    this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this.value = ''; this.title = ''; this.open = false;
    this.scrollTop = 0; this.offsetTop = 0; this.tabIndex = 0;
    const self = this;
    this.classList = {
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
    };
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...kids) {
    for (const k of kids) {
      if (typeof k === 'string') { this.append(Object.assign(new FakeEl('#text'), { _text: k })); continue; }
      if (k.tag === '#frag') { this.append(...k.children); continue; }
      k.parent = this; this.children.push(k);
    }
  }
  replaceChildren(...kids) { this._text = ''; this.children = []; this.append(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matches(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  fire(t, ev = {}) { const e = { target: this, preventDefault() { e.prevented = true; }, stopPropagation() {}, ...ev }; for (const f of this.listeners[t] ?? []) f(e); return e; }
  closest(sel) { for (let n = this; n; n = n.parent) if (n.tag !== '#text' && matches(n, sel)) return n; return null; }
  focus() { doc.activeElement = this; }
  select() {}
  scrollIntoView() {}
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  showModal() { this.open = true; }
  close() { this.open = false; this.onclose?.(); }
}

const ids = new Map();
const mk = (tag, id) => { const e = new FakeEl(tag); e.id = id; ids.set(id, e); return e; };
const body = new FakeEl('body');
const doc = {
  documentElement: root, title: '', activeElement: null, body, head: new FakeEl('head'),
  addEventListener() {},
  contains: () => true,
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => ids.get(id) ?? body.walk().find((n) => n.id === id) ?? null,
  createElement: (tag) => new FakeEl(tag),
  createDocumentFragment: () => new FakeEl('#frag'),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};

const toast = mk('div', 'toast');
body.append(toast);
const storage = new Map();
const KEY = 'rsi.desktop.theme.v1';
const mq = { matches: false, listeners: [], addEventListener(t, f) { this.listeners.push(f); } };
const setSystemLight = (light) => { mq.matches = light; for (const f of mq.listeners) f({ matches: light }); };
globalThis.window = globalThis;
globalThis.document = doc;
globalThis.localStorage = { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)), removeItem: (k) => storage.delete(k) };
globalThis.matchMedia = (q) => { assert.equal(q, '(prefers-color-scheme: light)'); return mq; };
globalThis.addEventListener = () => {};
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};

const themes = await import('../dist/themes.js');
const { contrastRatio } = await import('../dist/theme-model.js');

const fresh = (stored, light) => {
  storage.clear();
  if (stored !== undefined) storage.set(KEY, stored);
  mq.matches = light;
  themes.initThemes();
};
const stored = () => JSON.parse(storage.get(KEY));

test('a fresh profile follows the system appearance, live, as it changes', () => {
  fresh(undefined, false);
  assert.equal(root.dataset.theme, 'desktop-default');
  assert.equal(props.get('color-scheme'), 'dark');
  assert.equal(props.has('--base'), false, 'the stylesheet itself paints the dark default');
  setSystemLight(true);
  assert.equal(root.dataset.theme, 'light');
  assert.equal(props.get('color-scheme'), 'light', 'native checkboxes and selects turn light with the page');
  assert.match(props.get('--base'), /^#[0-9a-f]{6}$/);
  setSystemLight(false);
  assert.equal(root.dataset.theme, 'desktop-default');
  assert.equal(props.get('color-scheme'), 'dark');
  assert.equal(props.has('--base'), false);
});

test('a fresh profile that starts in a light system is Light at once', () => {
  fresh(undefined, true);
  assert.equal(root.dataset.theme, 'light');
  assert.equal(props.get('color-scheme'), 'light');
  assert.match(themes.themeName(), /Match system .*now Light/);
  setSystemLight(false);
  assert.match(themes.themeName(), /Match system .*now Dark/);
});

test('an explicit theme keeps its look when the system changes, and a stored choice is explicit', () => {
  fresh(JSON.stringify({ theme: 'goth', overrides: {} }), false);
  assert.equal(root.dataset.theme, 'goth');
  setSystemLight(true);
  assert.equal(root.dataset.theme, 'goth');
  setSystemLight(false);
  // A user who saved the desktop default keeps it under a light system.
  fresh(JSON.stringify({ theme: null, overrides: {} }), true);
  assert.equal(root.dataset.theme, 'desktop-default');
  assert.equal(props.get('color-scheme'), 'dark');
  setSystemLight(false);
});

test('the Light theme sets color-scheme light and High Contrast keeps dark, whatever the system says', () => {
  fresh(JSON.stringify({ theme: 'light', overrides: {} }), false);
  assert.equal(props.get('color-scheme'), 'light');
  fresh(JSON.stringify({ theme: 'high-contrast', overrides: {} }), true);
  assert.equal(props.get('color-scheme'), 'dark');
  setSystemLight(false);
});

test('badge and button text colours are set per theme and reach 4.5:1', () => {
  for (const theme of ['light', 'high-contrast']) {
    fresh(JSON.stringify({ theme, overrides: {} }), false);
    for (const accent of ['blue', 'green', 'yellow', 'red', 'mauve', 'peach', 'teal']) {
      const ratio = contrastRatio(props.get(`--on-${accent}`), props.get(`--${accent}`));
      assert.ok(ratio >= 4.5, `${theme} --on-${accent}: ${ratio.toFixed(2)}:1`);
    }
  }
  // Leaving a theme clears them again (the stylesheet's fallback paints).
  fresh(JSON.stringify({ theme: null, overrides: {} }), false);
  assert.equal(props.has('--on-red'), false);
});

test('picking Match system, a theme and back is stored as such and applied at once', () => {
  fresh(JSON.stringify({ theme: 'ruby', overrides: {} }), true);
  themes.applyThemeByName('system');
  assert.equal(stored().system, true);
  assert.equal(root.dataset.theme, 'light');
  setSystemLight(false);
  assert.equal(root.dataset.theme, 'desktop-default');
  themes.applyThemeByName('goth');
  assert.equal('system' in stored(), false, 'an explicit pick drops system');
  assert.equal(stored().theme, 'goth');
  setSystemLight(true);
  assert.equal(root.dataset.theme, 'goth');
  themes.applyThemeByName('Match system');
  assert.equal(root.dataset.theme, 'light');
  setSystemLight(false);
});

test('the theme picker lists Match system first, marks it, and choosing a row applies it', () => {
  fresh(undefined, false);
  themes.openThemePicker();
  const dlg = body.walk().find((n) => n.tag === 'dialog' && n.id === 'theme-picker');
  const rows = () => dlg.querySelectorAll('.pick-item');
  const label = (r) => r.children[1].textContent;
  assert.equal(label(rows()[0]), 'Match system (light/dark)');
  assert.equal(label(rows()[1]), 'Desktop default (Catppuccin Mocha)');
  assert.equal(label(rows()[2]), '1  Goth');
  assert.equal(rows()[0].children[0].textContent, '●', 'a fresh profile sits on Match system');
  assert.equal(rows()[1].children[0].textContent, '');
  assert.equal(rows()[0].getAttribute('aria-selected'), 'true');
  // Click Desktop default: explicit, so the system no longer matters.
  rows()[1].fire('click');
  assert.equal('system' in stored(), false);
  assert.equal(stored().theme, null);
  setSystemLight(true);
  assert.equal(root.dataset.theme, 'desktop-default');
  setSystemLight(false);
  // Open again on an explicit choice and click Match system.
  themes.openThemePicker();
  const dlg2 = body.walk().find((n) => n.tag === 'dialog' && n.id === 'theme-picker');
  assert.equal(dlg2.querySelectorAll('.pick-item')[1].children[0].textContent, '●');
  dlg2.querySelectorAll('.pick-item')[0].fire('click');
  assert.equal(stored().system, true);
});

test('a picker preview of Match system paints the system theme and Esc rolls it back', () => {
  fresh(JSON.stringify({ theme: 'goth', overrides: {} }), true);
  themes.openThemePicker();
  const dlg = body.walk().find((n) => n.tag === 'dialog' && n.id === 'theme-picker');
  dlg.onkeydown({ key: 'k', target: dlg, preventDefault() {}, stopPropagation() {} });
  dlg.onkeydown({ key: 'k', target: dlg, preventDefault() {}, stopPropagation() {} });
  assert.equal(root.dataset.theme, 'light', 'previewed Match system under a light system');
  dlg.onkeydown({ key: 'Escape', target: dlg, preventDefault() {}, stopPropagation() {} });
  assert.equal(root.dataset.theme, 'goth');
  assert.equal(stored().theme, 'goth');
  setSystemLight(false);
});

test('the Settings help names the choice and the stylesheet follows the theme for native controls', async () => {
  const { readFileSync } = await import('node:fs');
  const { fileURLToPath } = await import('node:url');
  const css = readFileSync(fileURLToPath(new URL('../static/styles.css', import.meta.url)), 'utf8');
  assert.match(css, /:root \{[^}]*color-scheme: dark; accent-color: var\(--blue\)/);
});

// The native window (title bar, frame) follows the app's effective theme (local #283): "Match system" hands the window
// back to the OS, an explicit theme pins it to the light or dark side it paints, and only a change is sent.
test('the native window theme follows the app theme: Match system -> system, an explicit theme -> its own light or dark', async () => {
  const sent = [];
  globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => { sent.push([cmd, args?.theme]); } } };
  try {
    fresh(undefined, false);
    assert.deepEqual(sent, [['set_window_theme', 'system']], 'a new install matches the system: the window stays with the OS');
    setSystemLight(true);
    assert.equal(sent.length, 1, 'the OS flipping changes the page, not the window request');
    themes.applyThemeByName('goth');
    assert.deepEqual(sent.at(-1), ['set_window_theme', 'dark']);
    themes.applyThemeByName('light');
    assert.deepEqual(sent.at(-1), ['set_window_theme', 'light'], 'Light on a dark OS gets a light title bar');
    const n = sent.length;
    themes.applyThemeByName('light');
    assert.equal(sent.length, n, 'unchanged: nothing sent');
    themes.applyThemeByName('high-contrast');
    assert.deepEqual(sent.at(-1), ['set_window_theme', 'dark']);
    themes.applyThemeByName('system');
    assert.deepEqual(sent.at(-1), ['set_window_theme', 'system']);
    setSystemLight(false);
  } finally {
    delete globalThis.__TAURI__;
  }
});

test('a shell without the command (or a failing one) never breaks theming and is retried on the next change', async () => {
  const sent = [];
  globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => { sent.push(args?.theme); throw 'command set_window_theme not found'; } } };
  try {
    fresh(JSON.stringify({ theme: 'goth', overrides: {} }), false);
    await new Promise((r) => setTimeout(r, 5));
    assert.equal(root.dataset.theme, 'goth');
    themes.applyThemeByName('goth');
    assert.ok(sent.length >= 2, `retried after the failure: ${sent}`);
  } finally {
    delete globalThis.__TAURI__;
  }
});
