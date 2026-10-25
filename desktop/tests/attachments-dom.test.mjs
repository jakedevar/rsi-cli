// Attaching images (local #267): the Attach image button, dropping image files, removable thumbnails, and
// the paste path they share. The rules (attachments-model.ts) run directly; the DOM module (attachments.ts)
// runs against a minimal fake document, a scripted upload_paste_image and fake files (no browser needed).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

// ---------- the fake page ----------

function matchesSimple(n, sel) {
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)((?:\[[^\]]+\])*)$/i.exec(sel);
  if (!m) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  for (const c of m[2].split('.').filter(Boolean)) if (!n.className.split(/\s+/).includes(c)) return false;
  for (const a of m[3].match(/\[[^\]]+\]/g) ?? []) {
    const [, name] = /^\[([\w-]+)/.exec(a) ?? [];
    if (name === 'open' && !n.open) return false;
  }
  return true;
}
const matches = (n, selector) => selector.split(',').map((s) => s.trim()).some((sel) => matchesSimple(n, sel.split(/\s+/).at(-1)));

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.listeners = {}; this._text = ''; this.className = ''; this.attrs = {}; this.parent = null;
    this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this.value = ''; this.title = ''; this.open = false;
    this.files = []; this.accept = ''; this.clicked = 0; this.selectionStart = 0; this.selectionEnd = 0; this.focused = false;
    const self = this;
    this.classList = {
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
      contains(c) { return self.className.split(/\s+/).includes(c); },
    };
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...kids) { for (const k of kids) { k.parent?.removeChild?.(k); k.parent = this; this.children.push(k); } }
  removeChild(k) { this.children = this.children.filter((c) => c !== k); }
  remove() { this.parent?.removeChild(this); this.parent = null; }
  replaceChildren(...kids) { this.children = []; this.append(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matches(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  dispatchEvent(ev) { const e = Object.assign(ev, { target: this }); for (const f of this.listeners[ev.type] ?? []) f(e); return true; }
  click() { this.clicked += 1; for (const f of this.listeners.click ?? []) f({ target: this }); }
  closest(sel) { for (let n = this; n; n = n.parent) if (matches(n, sel)) return n; return null; }
  contains(n) { for (let x = n; x; x = x.parent) if (x === this) return true; return false; }
  focus() { doc.activeElement = this; this.focused = true; }
  setRangeText(text, start, end) {
    this.value = this.value.slice(0, start) + text + this.value.slice(end);
    this.selectionStart = this.selectionEnd = start + text.length;
  }
}

const ids = new Map();
const mk = (tag, id, parent) => { const e = new FakeEl(tag); e.id = id; ids.set(id, e); parent?.append(e); return e; };
const body = new FakeEl('body');
const doc = {
  body, activeElement: null, listeners: {},
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); },
  querySelector: (sel) => body.querySelector(sel),
  getElementById: (id) => ids.get(id) ?? null,
  createElement: (tag) => new FakeEl(tag),
};
const sessionView = mk('section', 'session-view', body);
sessionView.className = 'session-view';
const events = mk('div', 'events', sessionView);
const message = mk('textarea', 'message', sessionView);
const attachBtn = mk('button', 'attach-image', sessionView);
const tray = mk('ul', 'attach-tray', sessionView);
tray.hidden = true;
const fileInput = mk('input', 'attach-file', sessionView);
const toastEl = mk('div', 'toast', body);

const uploads = [];
let uploadFails = null;
let n = 0;
globalThis.window = { setTimeout: () => 1, clearTimeout() {} };
globalThis.document = doc;
globalThis.Element = FakeEl;
globalThis.Node = FakeEl;
globalThis.Event = class { constructor(type, init = {}) { this.type = type; Object.assign(this, init); } };
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};
globalThis.FileReader = class {
  readAsDataURL(blob) { queueMicrotask(() => { this.result = `data:${blob.type};base64,${Buffer.from(blob.name ?? 'x').toString('base64')}`; this.onload(); }); }
};
const revoked = [];
URL.createObjectURL = (f) => `blob:fake/${f.name}`;
URL.revokeObjectURL = (u) => revoked.push(u);
globalThis.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      assert.equal(cmd, 'upload_paste_image', 'every attach path uses the paste RPC');
      uploads.push(args.request);
      if (uploadFails) throw new Error(uploadFails);
      n += 1;
      return { path: `/home/u/.rsi/paste/rsid/img-${n}.${args.request.mime.split('/')[1]}` };
    },
  },
};
const toasts = [];

const { state } = await import('../dist/state.js');
const { initAttachments, attachFiles, insertImage } = await import('../dist/attachments.js');
const model = await import('../dist/attachments-model.js');
const dom = await import('../dist/dom.js');
dom.onToast((text, ok) => toasts.push({ text, ok }));
doc.querySelectorAll = () => [];

state.selectedId = 's1';
state.sessions = [{ id: 's1', status: 'Running' }];
initAttachments();

const file = (name, type, size = 1000) => ({ name, type, size });
const reset = () => {
  uploads.length = 0; toasts.length = 0; uploadFails = null; message.value = ''; message.disabled = false;
  message.dispatchEvent(new Event('input'));
  state.view = 'active'; state.sessions = [{ id: 's1', status: 'Running' }]; sessionView.hidden = false;
  fileInput.clicked = 0; revoked.length = 0;
};
const drag = (type, files, over = sessionView, extra = {}) => {
  const ev = { type, target: over, dataTransfer: { types: files ? ['Files'] : ['text/plain'], files: files ?? [], dropEffect: '' }, prevented: false, preventDefault() { this.prevented = true; }, ...extra };
  for (const f of doc.listeners[type] ?? []) f(ev);
  return ev;
};
const settle = () => new Promise((r) => setImmediate(r));
const chipsIn = () => tray.querySelectorAll('.attach-chip');

// ---------- the rules ----------

test('the picker lists only the types the paste path takes, and the limit is the paste limit', () => {
  assert.equal(model.IMAGE_ACCEPT, 'image/png,image/jpeg,image/gif,image/webp');
  assert.equal(model.MAX_IMAGE_BYTES, 32 * 1024 * 1024);
  const rust = readFileSync(here('../src-tauri/src/input.rs'), 'utf8');
  assert.match(rust, /MAX_IMAGE_BYTES: usize = 32 \* 1024 \* 1024/);
});

test('judgeImageFile accepts PNG, JPEG, GIF and WebP (by type, or by name when an OS drop has no type)', () => {
  for (const [name, type, mime] of [['a.png', 'image/png', 'image/png'], ['b.JPG', 'image/jpeg', 'image/jpeg'], ['c.gif', 'image/gif', 'image/gif'],
    ['d.webp', 'image/webp', 'image/webp'], ['e.jpeg', '', 'image/jpeg'], ['f.PNG', '', 'image/png']]) {
    assert.deepEqual(model.judgeImageFile(file(name, type)), { ok: true, mime }, name);
  }
});

test('a refused file gets a plain sentence: wrong type, empty, too large', () => {
  const heic = model.judgeImageFile(file('IMG_1.heic', 'image/heic'));
  assert.equal(heic.ok, false);
  assert.match(heic.message, /"IMG_1.heic" was not attached: only PNG, JPEG, GIF and WebP images can be attached\./);
  assert.equal(model.judgeImageFile(file('notes.pdf', 'application/pdf')).ok, false);
  assert.equal(model.judgeImageFile(file('trick.png', 'application/pdf')).ok, false, 'a known non-image type is refused whatever the name says');
  assert.equal(model.judgeImageFile(file('noext', '')).ok, false);
  assert.match(model.judgeImageFile(file('e.png', 'image/png', 0)).message, /the file is empty/);
  assert.match(model.judgeImageFile(file('big.png', 'image/png', 40 * 1024 * 1024)).message, /it is 40 MB and images can be up to 32 MB/);
  assert.equal(model.judgeImageFile(file('edge.png', 'image/png', model.MAX_IMAGE_BYTES)).ok, true);
});

test('@path references: found as whole words and removed with their trailing space', () => {
  assert.equal(model.hasRef('look @/a/b.png now', '/a/b.png'), true);
  assert.equal(model.hasRef('look @/a/b.png.bak now', '/a/b.png'), false);
  assert.equal(model.hasRef('x@/a/b.png', '/a/b.png'), false);
  assert.equal(model.removeRef('look @/a/b.png now', '/a/b.png'), 'look now');
  assert.equal(model.removeRef('@/a/b.png ', '/a/b.png'), '');
  assert.equal(model.removeRef('hi @/a/b.png', '/a/b.png'), 'hi ');
  assert.equal(model.removeRef('nothing', '/a/b.png'), 'nothing');
});

test('dropVerdict: files only, in an open session, outside a dialog, never read-only', () => {
  const ok = { files: true, overSession: true, modalOpen: false, readOnly: false };
  assert.equal(model.dropVerdict(ok).kind, 'attach');
  assert.equal(model.dropVerdict({ ...ok, files: false }).kind, 'ignore');
  for (const bad of [{ overSession: false }, { modalOpen: true }, { readOnly: true }]) {
    const v = model.dropVerdict({ ...ok, ...bad });
    assert.equal(v.kind, 'refuse');
    assert.ok(v.message.length > 20);
  }
});

// ---------- the button ----------

test('the Attach image button opens the file chooser, limited to image types', () => {
  reset();
  assert.equal(fileInput.accept, model.IMAGE_ACCEPT);
  attachBtn.click();
  assert.equal(fileInput.clicked, 1);
});

test('the button says why when there is no session to attach to', () => {
  reset();
  state.selectedId = null;
  attachBtn.click();
  assert.equal(fileInput.clicked, 0);
  assert.match(toasts.at(-1).text, /Open a session/);
  state.selectedId = 's1';
});

test('choosing a file uploads it through the paste RPC, adds @path and a thumbnail, and clears the chooser', async () => {
  reset();
  message.value = 'see ';
  fileInput.files = [{ ...file('shot.png', 'image/png'), size: 5 }];
  fileInput.value = 'C:\\fakepath\\shot.png';
  fileInput.dispatchEvent(new Event('change'));
  await settle();
  assert.equal(uploads.length, 1);
  assert.equal(uploads[0].mime, 'image/png');
  assert.ok(uploads[0].data_base64.length > 0);
  assert.equal(message.value, 'see @/home/u/.rsi/paste/rsid/img-1.png ', 'the same @path a paste inserts, after the text, with a trailing space');
  assert.equal(fileInput.value, '', 'the same file can be picked again');
  assert.equal(tray.hidden, false);
  const [chip] = chipsIn();
  assert.equal(chip.dataset.path, '/home/u/.rsi/paste/rsid/img-1.png');
  assert.equal(chip.querySelector('.attach-name').textContent, 'shot.png');
  assert.equal(chip.querySelector('img').src, 'blob:fake/shot.png');
  assert.deepEqual(toasts.at(-1), { text: 'Image attached', ok: true });
});

test('a refused file shows a plain message and nothing is uploaded; accepted files in the same batch still attach', async () => {
  reset();
  await attachFiles([file('notes.pdf', 'application/pdf'), file('ok.jpg', 'image/jpeg'), file('huge.png', 'image/png', 99 * 1024 * 1024)]);
  assert.equal(uploads.length, 1);
  assert.equal(chipsIn().length, 1);
  const t = toasts.at(-1);
  assert.equal(t.ok, false);
  assert.match(t.text, /"notes.pdf" was not attached: only PNG/);
  assert.match(t.text, /"huge.png" was not attached: it is 99 MB/);
});

test('a failed upload says so and adds no thumbnail', async () => {
  reset();
  uploadFails = 'image is not a PNG';
  await attachFiles([file('x.png', 'image/png')]);
  assert.equal(chipsIn().length, 0);
  assert.equal(message.value, '');
  assert.match(toasts.at(-1).text, /"x.png" was not attached: .*image is not a PNG/);
});

// ---------- drop ----------

test('dropping image files onto the conversation attaches them (two files, two thumbnails)', async () => {
  reset();
  const files = [file('one.png', 'image/png'), file('two.webp', 'image/webp')];
  const over = drag('dragover', files, events);
  assert.equal(over.prevented, true, 'the page keeps the drop (no navigation to the file)');
  assert.equal(over.dataTransfer.dropEffect, 'copy');
  assert.equal(sessionView.classList.contains('drop-target'), true);
  const dropped = drag('drop', files, events);
  assert.equal(dropped.prevented, true);
  assert.equal(sessionView.classList.contains('drop-target'), false);
  await settle();
  assert.equal(uploads.length, 2);
  assert.equal(chipsIn().length, 2);
  assert.match(message.value, /^@\/.*img-\d+\.png @\/.*img-\d+\.webp $/);
  assert.deepEqual(toasts.at(-1), { text: '2 images attached', ok: true });
});

test('dropping onto the message box attaches too', async () => {
  reset();
  drag('drop', [file('p.gif', 'image/gif')], message);
  await settle();
  assert.equal(chipsIn().length, 1);
});

test('a drop outside the open session, over a dialog, or onto an archived session explains why and attaches nothing', async () => {
  reset();
  const elsewhere = mk('div', 'sidebar', body);
  let ev = drag('drop', [file('p.png', 'image/png')], elsewhere);
  assert.equal(ev.prevented, true, 'still kept: the app must not navigate away');
  assert.match(toasts.at(-1).text, /open a session and drop it onto the conversation/);

  state.view = 'archived';
  ev = drag('dragover', [file('p.png', 'image/png')], events);
  assert.equal(ev.dataTransfer.dropEffect, 'none');
  drag('drop', [file('p.png', 'image/png')], events);
  assert.match(toasts.at(-1).text, /archived \(read-only\)/);
  state.view = 'active';

  const savedQuery = doc.querySelector;
  doc.querySelector = (sel) => (sel === 'dialog[open]' ? {} : savedQuery(sel));
  drag('drop', [file('p.png', 'image/png')], events);
  doc.querySelector = savedQuery;
  assert.match(toasts.at(-1).text, /Close the open window/);
  await settle();
  assert.equal(uploads.length, 0);
  assert.equal(chipsIn().length, 0);
});

test('a drag of text or a link is left alone', () => {
  reset();
  const ev = drag('dragover', null, events);
  assert.equal(ev.prevented, false);
  assert.equal(toasts.length, 0);
});

// ---------- thumbnails ----------

test('the remove button takes the thumbnail and its @path out of the message, keeping the rest', async () => {
  reset();
  message.value = 'before ';
  await attachFiles([file('a.png', 'image/png'), file('b.png', 'image/png')]);
  message.value += 'after';
  assert.equal(chipsIn().length, 2);
  const [first] = chipsIn();
  const btn = first.querySelector('button');
  assert.equal(btn.attrs['aria-label'], 'Remove image a.png');
  btn.click();
  assert.equal(chipsIn().length, 1);
  assert.equal(chipsIn()[0].querySelector('.attach-name').textContent, 'b.png');
  assert.equal(message.value, 'before @/home/u/.rsi/paste/rsid/img-' + n + '.png after');
  assert.deepEqual(revoked, ['blob:fake/a.png'], 'the preview is released');
  assert.equal(message.focused, true, 'focus returns to the message box');
  chipsIn()[0].querySelector('button').click();
  assert.equal(tray.hidden, true);
  assert.equal(message.value, 'before after');
});

test('thumbnails follow the message: sending (clearing) it or deleting the @path drops them', async () => {
  reset();
  await attachFiles([file('a.png', 'image/png')]);
  assert.equal(chipsIn().length, 1);
  message.value = ''; // what sendMessage does, then it dispatches input
  message.dispatchEvent(new Event('input'));
  assert.equal(chipsIn().length, 0);
  assert.equal(tray.hidden, true);
});

test('a thumbnail that cannot load is hidden, the name and remove button stay', async () => {
  reset();
  await attachFiles([file('a.png', 'image/png')]);
  const img = chipsIn()[0].querySelector('img');
  img.dispatchEvent(new Event('error'));
  assert.equal(img.hidden, true);
  assert.ok(chipsIn()[0].querySelector('button'));
});

// ---------- the paste path ----------

test('a paste into the message box uploads the same way, replaces the selection and shows a thumbnail', async () => {
  reset();
  message.value = 'abcdef';
  message.selectionStart = 2; message.selectionEnd = 4;
  const path = await insertImage(message, { name: 'image.png', type: 'image/png' }, 'image/png', 'image.png', 2);
  assert.equal(message.value, `ab@${path} ef`);
  assert.equal(uploads[0].mime, 'image/png');
  assert.equal(chipsIn().length, 1);
});

test('a paste into another field (a dialog) inserts the reference and adds no thumbnail', async () => {
  reset();
  const other = new FakeEl('input');
  await insertImage(other, { name: 'image.png', type: 'image/png' }, 'image/png', 'image.png', 0);
  assert.match(other.value, /^@\/home\/u\/.*\.png $/);
  assert.equal(chipsIn().length, 0);
});

// ---------- the page ----------

const html = readFileSync(here('../static/index.html'), 'utf8');
const src = (p) => readFileSync(here(`../src/${p}`), 'utf8');

test('the page has the button next to Send, a file input limited to images, and the thumbnail tray', () => {
  assert.match(html, /<div id="composer-actions"[^>]*>\s*<button id="attach-image"[^>]*type="button"[^>]*>Attach image<\/button>\s*<button class="btn primary" type="submit">Send<\/button>/);
  assert.match(html, /<input id="attach-file" type="file" multiple hidden>/);
  assert.match(html, /<ul id="attach-tray"[^>]*aria-label="Attached images"[^>]*hidden><\/ul>/);
});

test('Attach image is a real button (a Tab stop with Vim keys off, #243) and the readonly toggle covers it', () => {
  // dom.ts el() and the boot stamp (tab-stops.ts) give every <button> tabindex="0"; the stamp matches this one.
  assert.match(src('tab-stops.ts'), /TAB_STOP_SELECTOR = 'button,/);
  assert.match(src('sessions.ts'), /\$\('composer-actions'\)\.querySelectorAll\('button'\)\) b\.disabled = ro/);
  assert.match(html, /id="attach-image"[^>]*type="button"/);
});

test('an OS file drop reaches the page: the window does not take drops itself, and thumbnails are allowed by the CSP', () => {
  const conf = JSON.parse(readFileSync(here('../src-tauri/tauri.conf.json'), 'utf8'));
  assert.equal(conf.app.windows[0].dragDropEnabled, false);
  assert.match(conf.app.security.csp, /img-src 'self' blob:;/);
});
