// A fake document rich enough to run the palette and picker DOM modules without a browser (local #318): elements with
// attributes (set, get, remove), class names, dataset, children, a small selector matcher, key and click listeners and
// focus. Installing it sets globalThis.document / window, so import it before the module under test.
function matchesSimple(n, sel) {
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)((?:\[[^\]]+\])*)$/i.exec(sel);
  if (!m) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  for (const c of m[2].split('.').filter(Boolean)) if (!n.className.split(/\s+/).includes(c)) return false;
  for (const a of m[3].match(/\[[^\]]+\]/g) ?? []) {
    const [, name, val] = /^\[([\w-]+)(?:="([^"]*)")?\]$/.exec(a) ?? [];
    if (!(name in n.attrs) || (val !== undefined && n.attrs[name] !== val)) return false;
  }
  return true;
}
const matches = (n, selector) => selector.split(',').map((s) => s.trim()).some((sel) => matchesSimple(n, sel.split(/\s+/).at(-1)));

export class FakeEl {
  constructor(tag) {
    this.tag = tag; this.tagName = tag.toUpperCase(); this.children = []; this.listeners = {}; this._text = ''; this.className = '';
    this.attrs = {}; this.parent = null; this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false;
    this.value = ''; this.title = ''; this.open = false; this.tabIndex = 0; this.id = '';
    const self = this;
    this.classList = {
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
      add(c) { this.toggle(c, true); },
      contains(c) { return self.className.split(/\s+/).includes(c); },
    };
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  get isConnected() { return true; }
  append(...kids) {
    for (const k of kids) {
      if (typeof k === 'string') { this.append(Object.assign(new FakeEl('#text'), { _text: k })); continue; }
      k.parent = this; this.children.push(k);
    }
  }
  replaceChildren(...kids) { this._text = ''; this.children = []; this.append(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  removeAttribute(k) { delete this.attrs[k]; }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => n.tag !== '#text' && matches(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  fire(t, ev = {}) { const e = { target: this, preventDefault() { e.prevented = true; }, stopPropagation() {}, ...ev }; for (const f of this.listeners[t] ?? []) f(e); this[`on${t}`]?.(e); return e; }
  closest(sel) { for (let n = this; n; n = n.parent) if (n.tag !== '#text' && matches(n, sel)) return n; return null; }
  focus() { doc.activeElement = this; }
  select() {}
  scrollIntoView() {}
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this); this.parent = null; }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  showModal() { this.open = true; }
  close() { this.open = false; this.fire('close'); }
}

const body = new FakeEl('body');
export const doc = {
  documentElement: { dataset: {} }, title: '', activeElement: null, body, head: new FakeEl('head'),
  addEventListener() {},
  contains: () => true,
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => body.walk().find((n) => n.id === id) ?? null,
  createElement: (tag) => new FakeEl(tag),
  createDocumentFragment: () => new FakeEl('#frag'),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};
const storage = new Map();
globalThis.window = globalThis;
globalThis.document = doc;
globalThis.localStorage = { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)), removeItem: (k) => storage.delete(k) };
globalThis.addEventListener = () => {};
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};

/** A fake element appended to the body with an id (the page's static markup). */
export function mount(tag, id, parent = body) {
  const e = new FakeEl(tag);
  e.id = id;
  parent.append(e);
  return e;
}
export { body };
