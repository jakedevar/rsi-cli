// The Issue inspector's body and handoffs as Markdown (local #293). issue-markdown.ts and markdown.ts run against a minimal
// fake document and a scripted bridge whose render_markdown stands in for the backend's sanitizing renderer (no browser).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.listeners = {}; this._text = ''; this.className = ''; this.attrs = {}; this.parent = null;
    this.dataset = {}; this.style = {}; this.tabIndex = 0; this.id = ''; this.htmlSets = [];
  }
  get classList() {
    const self = this;
    return {
      add: (c) => { if (!self.className.split(/\s+/).includes(c)) self.className = `${self.className} ${c}`.trim(); },
      remove: (c) => { self.className = self.className.split(/\s+/).filter((x) => x && x !== c).join(' '); },
      contains: (c) => self.className.split(/\s+/).includes(c),
      [Symbol.iterator]: () => self.className.split(/\s+/).filter(Boolean)[Symbol.iterator](),
    };
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  set innerHTML(v) { this.htmlSets.push(String(v)); this._html = String(v); this._text = ''; this.children = []; }
  get innerHTML() { return this._html ?? ''; }
  get isConnected() { return true; }
  append(...kids) { for (const k of kids) { k.parent = this; this.children.push(k); } }
  replaceChildren(...kids) { this.children = []; this.append(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  querySelectorAll() { return []; }
  querySelector() { return null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  dispatchEvent(ev) {
    const e = Object.assign(ev, { target: ev.target ?? this, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } });
    for (let n = this; n; n = n.parent) for (const f of n.listeners[ev.type] ?? []) f(e);
    return e;
  }
  has(cls) { return this.className.split(/\s+/).includes(cls); }
  closest(sel) {
    const hasMd = sel === '.md a';
    const last = hasMd ? 'a' : sel;
    for (let n = this; n; n = n.parent) {
      const ok = last.startsWith('.') ? n.has(last.slice(1)) : n.tag === last;
      if (ok && (!hasMd || this.ancestorHas('md'))) return n;
    }
    return null;
  }
  ancestorHas(cls) { for (let n = this; n; n = n.parent) if (n.has(cls)) return true; return false; }
}

const toastEl = new FakeEl('div');
const doc = {
  body: new FakeEl('body'), activeElement: null,
  createElement: (tag) => new FakeEl(tag),
  createTextNode: (t) => Object.assign(new FakeEl('#text'), { _text: t }),
  getElementById: (id) => (id === 'toast' ? toastEl : null),
};
const calls = [];
let renderFails = false;
globalThis.document = doc;
globalThis.window = { setTimeout: (f, ms) => setTimeout(f, ms), clearTimeout: (t) => clearTimeout(t) };
Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { clipboard: { writeText: async (t) => { calls.push({ cmd: 'clipboard', text: t }); } } } });
// The stand-in for render_markdown: headings, bullets, inline code and fences become tags, and any raw HTML is escaped, as the
// backend's sanitizer allowlist leaves no script or event handler behind.
const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
function render(src) {
  const out = [];
  let fence = null;
  for (const line of src.split('\n')) {
    if (line.startsWith('```')) {
      if (fence === null) { fence = []; } else { out.push(`<pre><code>${esc(fence.join('\n'))}\n</code></pre>`); fence = null; }
      continue;
    }
    if (fence) { fence.push(line); continue; }
    const h = /^(#{1,6}) (.*)$/.exec(line);
    if (h) { out.push(`<h${h[1].length}>${esc(h[2])}</h${h[1].length}>`); continue; }
    if (line.startsWith('- ')) { out.push(`<ul><li>${esc(line.slice(2)).replace(/`([^`]+)`/g, '<code>$1</code>')}</li></ul>`); continue; }
    if (line.trim()) out.push(`<p>${esc(line)}</p>`);
  }
  return out.join('');
}
globalThis.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === 'render_markdown') {
        if (renderFails) throw 'render_markdown failed';
        return args.texts.map(render);
      }
      if (cmd === 'open_url') {
        if (!/^https?:\/\//.test(args.url)) throw 'only web links';
        return { opened: args.url };
      }
      return null;
    },
  },
};

const { issueTextBlock, rawToggle, bodyKey, handoffKey, NO_DESCRIPTION } = await import('../dist/issue-markdown.js');
const { wireMarkdownEvents, openMarkdownLink } = await import('../dist/markdown.js');

const settle = () => new Promise((r) => setTimeout(r, 40));
const mdOf = (block) => block.children[0];
const BODY = '# Plan\n\n- fix `desktop/src/issues.ts`\n- add a test\n\n```rust\nlet very_long_line = 1; // aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n```\n';

test('a body with a heading, a list, inline code and a fenced block is rendered by the transcript renderer', async () => {
  const before = calls.filter((c) => c.cmd === 'render_markdown').length;
  const block = issueTextBlock(bodyKey('i1'), BODY, false);
  assert.ok(block.has('iv-body') && block.has('iv-md'));
  assert.equal(mdOf(block).textContent, BODY, 'plain wrapped text until the render lands');
  assert.ok(mdOf(block).has('md-pending'));
  await settle();
  const html = mdOf(block).innerHTML;
  assert.match(html, /<h1>Plan<\/h1>/);
  assert.match(html, /<li>fix <code>desktop\/src\/issues\.ts<\/code><\/li>/);
  assert.match(html, /<li>add a test<\/li>/);
  assert.match(html, /<pre><code>let very_long_line/);
  assert.ok(!mdOf(block).has('md-pending'));
  assert.equal(calls.filter((c) => c.cmd === 'render_markdown').length, before + 1, 'one batched render call');
});

test('a second draw of the same text is answered from the cache at once, with no render call', async () => {
  issueTextBlock(bodyKey('i2'), '- one\n- two', false);
  await settle();
  const n = calls.filter((c) => c.cmd === 'render_markdown').length;
  const again = issueTextBlock(bodyKey('i2'), '- one\n- two', false);
  assert.match(mdOf(again).innerHTML, /<li>one<\/li>/, 'rendered synchronously on the redraw');
  assert.equal(calls.filter((c) => c.cmd === 'render_markdown').length, n);
  const changed = issueTextBlock(bodyKey('i2'), '- one\n- three', false);
  assert.ok(mdOf(changed).has('md-pending'), 'a changed body does not reuse the old HTML');
  await settle();
});

test('each handoff has its own cache key', () => {
  assert.notEqual(handoffKey('i1', 1), handoffKey('i1', 2));
  assert.notEqual(handoffKey('i1', 1), handoffKey('i2', 1));
  assert.notEqual(handoffKey('i1', 1), bodyKey('i1'));
});

test('script and HTML in a body are never executed: the page shows them as text and sends them only to the sanitizing backend', async () => {
  const evil = 'hello <script>window.pwned = 1</script> <img src=x onerror="window.pwned = 2"> <b>bold?</b>';
  const block = issueTextBlock(bodyKey('evil'), evil, false);
  assert.equal(mdOf(block).htmlSets.length, 0, 'before the render the text is set with textContent, not innerHTML');
  assert.equal(mdOf(block).textContent, evil);
  await settle();
  const sets = mdOf(block).htmlSets;
  assert.equal(sets.length, 1);
  assert.ok(!/<script|<img|<b>|<[^>]*onerror/.test(sets[0]), `only sanitized markup was assigned: ${sets[0]}`);
  assert.match(sets[0], /&lt;script&gt;/);
  assert.equal(globalThis.pwned, undefined);
  assert.ok(calls.some((c) => c.cmd === 'render_markdown' && c.args.texts.includes(evil)));
});

test('when rendering is unavailable the body stays readable as plain text', async () => {
  renderFails = true;
  const text = '# Heading\n- item';
  const block = issueTextBlock(bodyKey('down'), text, false);
  await settle();
  renderFails = false;
  assert.equal(mdOf(block).textContent, text);
  assert.equal(mdOf(block).htmlSets.length, 0);
  assert.match(toastEl.textContent, /Markdown rendering unavailable/);
});

test('raw mode shows the text whole in a monospace pre, with no render call', () => {
  const n = calls.filter((c) => c.cmd === 'render_markdown').length;
  const block = issueTextBlock(bodyKey('raw'), BODY, true);
  assert.equal(block.tag, 'pre');
  assert.ok(block.has('iv-body'));
  assert.equal(block.textContent, BODY);
  assert.equal(calls.filter((c) => c.cmd === 'render_markdown').length, n);
});

test('an empty body still reads (no description); an empty handoff has its own words; plain text looks like plain text', async () => {
  const empty = issueTextBlock(bodyKey('e'), '', false);
  assert.equal(empty.textContent, '(no description)');
  assert.equal(NO_DESCRIPTION, '(no description)');
  assert.equal(issueTextBlock(handoffKey('e', 1), '', false, '(empty handoff)').textContent, '(empty handoff)');
  const plain = issueTextBlock(bodyKey('plain'), 'Just a sentence.', false);
  await settle();
  assert.equal(mdOf(plain).textContent, 'Just a sentence.');
});

test('the View raw toggle keeps one label (a redraw refocuses by label) and says its state with aria-pressed', () => {
  const off = rawToggle(false);
  const on = rawToggle(true);
  assert.equal(off.tag, 'button');
  assert.equal(off.textContent, 'View raw');
  assert.equal(on.textContent, 'View raw');
  assert.equal(off.className, on.className, 'the class is the same either way, so a redraw can refocus it');
  assert.equal(off.getAttribute('aria-pressed'), 'false');
  assert.equal(on.getAttribute('aria-pressed'), 'true');
  assert.equal(off.type ?? 'button', 'button');
});

// ---------- links ----------

function pane() {
  const root = new FakeEl('div');
  const md = new FakeEl('div'); md.className = 'md';
  root.append(md);
  const link = (href) => { const a = new FakeEl('a'); a.setAttribute('href', href); md.append(a); return a; };
  wireMarkdownEvents(root, { openLinks: true });
  return { root, link };
}
const click = (el) => el.dispatchEvent({ type: 'click' });

test('clicking a web link opens it in the system browser through open_url and never navigates the webview', async () => {
  const { link } = pane();
  const before = calls.length;
  const ev = click(link('https://example.com/a?b=1'));
  assert.equal(ev.defaultPrevented, true, 'the webview is not navigated');
  await settle();
  const made = calls.slice(before);
  assert.deepEqual(made.find((c) => c.cmd === 'open_url'), { cmd: 'open_url', args: { url: 'https://example.com/a?b=1' } });
});

test('a link that is not a web address (javascript:, file:, a relative path) is never opened: its address is copied instead', async () => {
  const { link } = pane();
  for (const href of ['javascript:alert(1)', 'file:///etc/passwd', '../secret.md', '#frag']) {
    const before = calls.length;
    const ev = click(link(href));
    assert.equal(ev.defaultPrevented, true, `${href} does not navigate`);
    await settle();
    const made = calls.slice(before);
    assert.ok(!made.some((c) => c.cmd === 'open_url'), `${href} never reaches the opener`);
    assert.ok(made.some((c) => c.cmd === 'clipboard' && c.text === href), `${href} is copied`);
  }
});

test('when the opener fails the address is copied, so the link is never lost', async () => {
  const before = calls.length;
  await openMarkdownLink('https://example.com/');
  assert.ok(calls.slice(before).some((c) => c.cmd === 'open_url'));
  const bad = calls.length;
  globalThis.__TAURI__.core.invoke = async (cmd, args) => { calls.push({ cmd, args }); if (cmd === 'open_url') throw 'no opener'; return null; };
  await openMarkdownLink('https://example.com/b');
  await settle();
  assert.ok(calls.slice(bad).some((c) => c.cmd === 'clipboard' && c.text === 'https://example.com/b'));
});

test('the transcript keeps its own behaviour: a link click copies the address and does not open it', async () => {
  const root = new FakeEl('div');
  const md = new FakeEl('div'); md.className = 'md';
  const a = new FakeEl('a'); a.setAttribute('href', 'https://example.com/t'); md.append(a); root.append(md);
  wireMarkdownEvents(root);
  const before = calls.length;
  const ev = click(a);
  await settle();
  assert.equal(ev.defaultPrevented, true);
  assert.ok(!calls.slice(before).some((c) => c.cmd === 'open_url'));
  assert.ok(calls.slice(before).some((c) => c.cmd === 'clipboard' && c.text === 'https://example.com/t'));
});

// ---------- source wiring ----------

test('the inspector draws the body and every handoff through issueTextBlock, shares one raw switch and wires the links', () => {
  const src = readFileSync(here('../src/issues.ts'), 'utf8');
  assert.ok(!/el\('pre', 'iv-body'/.test(src), 'no raw pre is built any more');
  assert.match(src, /issueTextBlock\(bodyKey\(issue\.id\), issue\.body, ws\.raw\)/);
  assert.match(src, /issueTextBlock\(handoffKey\(issue\.id, h\.sequence\), h\.body, ws\.raw/);
  assert.match(src, /wireMarkdownEvents\(\$\('issues-inspector'\), \{ openLinks: true \}\)/);
  assert.match(src, /\[data-md-raw\]/);
});

test('long code blocks scroll sideways inside the inspector', () => {
  const css = readFileSync(here('../static/styles.css'), 'utf8');
  assert.match(css, /\.md pre \{[^}]*overflow-x: auto[^}]*white-space: pre/);
  assert.match(css, /\.iv-md \.md pre \{[^}]*overflow-x: auto/);
  assert.match(css, /\.iv-md \{[^}]*min-width: 0/);
});
