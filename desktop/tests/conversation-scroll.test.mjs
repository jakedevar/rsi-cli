// Local #306: a long conversation keeps the reader's place. The fake pane lays its children out (fixed heights, a gap and a
// padding) so scrollTop, scrollHeight and each event's rectangle are real numbers: "Show earlier" keeps the event at the top
// of the viewport where it was, new events never move a reader who scrolled up, "Jump to latest (N new)" counts them, and a
// reader at the bottom follows the tail while the window trims from the top. The real-browser run of the same cases is the
// smoke step long-conversation-scroll.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const PANE_TOP = 100, VIEW = 600, EV_H = 40, MORE_H = 30, GAP = 8, PAD = 12;

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.parent = null; this.listeners = {}; this.attrs = {}; this.dataset = {};
    this.className = ''; this._text = ''; this.hidden = false; this.scrollTop = 0; this.focused = false;
    const self = this;
    this.classList = {
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
      add(c) { this.toggle(c, true); }, remove(c) { this.toggle(c, false); }, contains(c) { return self.className.split(/\s+/).includes(c); },
    };
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  set tabIndex(v) { this.attrs.tabindex = String(v); }
  get tabIndex() { return Number(this.attrs.tabindex ?? -1); }
  get clientHeight() { return this.isPane ? VIEW : 0; }
  get scrollHeight() { return this.isPane ? this.layout().total : 0; }
  attach(kids) { for (const k of kids) { k.parent?.detach(k); k.parent = this; } }
  detach(k) { this.children = this.children.filter((c) => c !== k); }
  append(...kids) { const ks = kids.map((k) => (typeof k === 'string' ? Object.assign(new FakeEl('#text'), { _text: k }) : k)); this.attach(ks); this.children.push(...ks); }
  prepend(...kids) { this.attach(kids); this.children.unshift(...kids); }
  replaceChildren(...kids) { this.children = []; this.attach(kids); this.children = kids; }
  before(...kids) { const p = this.parent; p.attach(kids); p.children.splice(p.children.indexOf(this), 0, ...kids); }
  remove() { this.parent?.detach(this); this.parent = null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  removeAttribute(k) { delete this.attrs[k]; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  fire(t) { for (const f of this.listeners[t] ?? []) f({ target: this }); }
  focus() { document.activeElement = this; this.focused = true; }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  matchesSel(sel) {
    const m = /^([a-z]*)((?:\.[\w-]+)*)(?:\[data-seq="(\d+)"\])?$/.exec(sel);
    if (!m) return false;
    if (m[1] && this.tag !== m[1]) return false;
    for (const c of m[2].split('.').filter(Boolean)) if (!this.classList.contains(c)) return false;
    return m[3] === undefined || this.dataset.seq === m[3];
  }
  querySelectorAll(sel) {
    const parts = sel.split(/\s+/);
    let found = this.walk().slice(1);
    for (let i = 0; i < parts.length; i++) found = (i === 0 ? found : found.flatMap((n) => n.walk().slice(1))).filter((n) => n.matchesSel(parts[i]));
    return found;
  }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  /** Vertical layout of the pane's children: a "Show earlier" button, then one fixed-height row per event. */
  layout() {
    let y = PAD; const tops = new Map();
    for (const c of this.children) {
      if (c.hidden) continue;
      const h = c.classList.contains('more') ? MORE_H : EV_H;
      tops.set(c, { y, h }); y += h + GAP;
    }
    return { tops, total: y - GAP + PAD };
  }
  getBoundingClientRect() {
    if (this.isPane) return { top: PANE_TOP, bottom: PANE_TOP + VIEW, height: VIEW };
    const box = this.parent.layout().tops.get(this);
    if (!box) return { top: 0, bottom: 0, height: 0 };
    const top = PANE_TOP + box.y - this.parent.scrollTop;
    return { top, bottom: top + box.h, height: box.h };
  }
}
// scrollTop is clamped like a browser's.
const pane = new FakeEl('div'); pane.isPane = true; pane.id = 'events';
let st = 0;
Object.defineProperty(pane, 'scrollTop', { get: () => st, set: (v) => { st = Math.max(0, Math.min(pane.scrollHeight - VIEW, v)); } });
const jump = new FakeEl('button'); jump.hidden = true;
const body = new FakeEl('body');
const byId = { events: pane, 'jump-latest': jump };
const document = {
  activeElement: null,
  getElementById: (id) => byId[id] ?? null,
  createElement: (tag) => new FakeEl(tag),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
  querySelector: () => null, querySelectorAll: () => [],
  addEventListener() {}, removeEventListener() {}, head: new FakeEl('head'), body, documentElement: new FakeEl('html'),
};
pane.focus = function () { document.activeElement = this; this.focused = true; };
globalThis.document = document;
globalThis.window = globalThis;
globalThis.addEventListener ??= () => {};
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };
globalThis.navigator ??= { platform: 'MacIntel', userAgent: 'Macintosh' };
globalThis.__TAURI__ = { core: { invoke: async () => ({}) } };
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (...a) => { const t = realSetTimeout(...a); t.unref?.(); return t; };

const { state } = await import('../dist/state.js');
const cv = await import('../dist/conversation.js');
const sm = await import('../dist/scroll-model.js');

const ev = (n, extra = {}) => ({ id: n, session_id: 's1', sequence: n, created_at: '2026-10-20T10:00:00.000000000Z', event_type: n % 2 ? 'ToolUse' : 'ToolResult', role: 'Assistant', tool_name: 'Bash', content: `event ${n}`, ...extra });
const range = (a, b) => Array.from({ length: b - a + 1 }, (_, i) => ev(a + i));
function open(n) {
  state.events = range(1, n);
  state.renderLimit = 400; state.renderedCount = 0; state.renderedFirstSeq = undefined; state.unseenEvents = 0; state.hideTools = false;
  st = 0; pane.replaceChildren(); cv.resetScroll(); cv.renderEvents();
}
const push = (n) => { const base = state.events.length; state.events = state.events.concat(range(base + 1, base + n)); cv.renderEvents(); };
const nodes = () => pane.children.filter((c) => c.classList.contains('ev'));
const more = () => pane.children.find((c) => c.classList.contains('more'));
const maxTop = () => pane.scrollHeight - VIEW;
/** Where the event sits below the pane's top edge. */
const yOf = (seq) => pane.querySelector(`.ev[data-seq="${seq}"]`).getBoundingClientRect().top - PANE_TOP;
/** The event the reader is looking at: the first whose bottom edge is below the top edge. */
const topEvent = () => { for (const n of nodes()) if (n.getBoundingClientRect().bottom > PANE_TOP) return Number(n.dataset.seq); };
const scrollTo = (v) => { pane.scrollTop = v; pane.fire('scroll'); };

test('opening a conversation over 400 events paints the last 400 at the bottom with a Show earlier button', () => {
  open(1000);
  assert.equal(nodes().length, 400);
  assert.equal(more().textContent, 'Show earlier (600 hidden)');
  assert.equal(pane.scrollTop, maxTop());
  assert.equal(jump.hidden, true);
});

test('Show earlier keeps the event at the top of the viewport in the same place', () => {
  open(1000);
  scrollTo(0);
  const seq = topEvent();
  const y = yOf(seq);
  more().fire('click');
  assert.equal(nodes().length, 800, 'one more page, no rebuild of the others');
  assert.equal(more().textContent, 'Show earlier (200 hidden)');
  assert.equal(yOf(seq), y, 'the event did not move');
  assert.ok(pane.scrollTop > 0 && pane.scrollTop < maxTop(), `scrollTop ${pane.scrollTop} is at that event, not the top or the bottom`);
  more().fire('click');
  assert.equal(nodes().length, 1000);
  assert.equal(more(), undefined, 'every event is loaded: the button is gone');
  assert.equal(yOf(seq), y);
});

test('Show earlier mid-way through the page keeps the event the reader was on', () => {
  open(1000);
  scrollTo(1500);
  const seq = topEvent(); const y = yOf(seq);
  more().fire('click');
  assert.equal(yOf(seq), y);
  assert.equal(topEvent(), seq);
});

test('new events never move a reader who scrolled up; the button counts them', () => {
  open(1000);
  scrollTo(Math.round(maxTop() / 2));
  const top = pane.scrollTop; const seq = topEvent(); const y = yOf(seq);
  push(3);
  assert.equal(pane.scrollTop, top, 'scrollTop unchanged');
  assert.equal(yOf(seq), y);
  assert.equal(jump.hidden, false);
  assert.equal(jump.textContent, 'Jump to latest (3 new)');
  push(2);
  assert.equal(jump.textContent, 'Jump to latest (5 new)');
  assert.equal(state.unseenEvents, 5);
  assert.equal(pane.scrollTop, top);
});

test('the same holds in a conversation whose window is full, where every append used to shift the first event', () => {
  open(1000);
  const firstBefore = nodes()[0];
  scrollTo(Math.round(maxTop() / 2));
  const seq = topEvent(); const y = yOf(seq);
  push(1);
  assert.equal(nodes()[0], firstBefore, 'the first rendered node is the same element: nothing was rebuilt');
  assert.equal(yOf(seq), y);
  assert.equal(nodes().length, 401, 'the window grows by one instead of rebuilding');
});

test('a reader at the bottom follows the tail and sees no button', () => {
  open(1000);
  push(3);
  assert.equal(pane.scrollTop, maxTop());
  assert.equal(jump.hidden, true);
  // Within 80 px of the end still counts as the bottom.
  scrollTo(maxTop() - 40);
  push(2);
  assert.equal(pane.scrollTop, maxTop());
  assert.equal(jump.hidden, true);
  // Further than that does not.
  scrollTo(maxTop() - 200);
  const top = pane.scrollTop;
  push(1);
  assert.equal(pane.scrollTop, top);
  assert.equal(jump.hidden, false);
});

test('reaching the bottom by scrolling clears the button; clicking it jumps there, clears it and keeps the keys on the pane', () => {
  open(1000);
  scrollTo(100);
  push(4);
  assert.equal(jump.textContent, 'Jump to latest (4 new)');
  scrollTo(maxTop());
  assert.equal(jump.hidden, true, 'scrolled to the bottom by hand');
  assert.equal(state.unseenEvents, 0);
  scrollTo(100);
  push(2);
  assert.equal(jump.hidden, false);
  jump.fire('click');
  assert.equal(pane.scrollTop, maxTop());
  assert.equal(jump.hidden, true);
  assert.equal(state.unseenEvents, 0);
  assert.equal(document.activeElement, pane, 'the hidden button must not leave focus on the page');
  assert.equal(pane.tabIndex, -1);
});

test('scrollToLatest (G and End) jumps and clears the count', () => {
  open(1000);
  scrollTo(100);
  push(2);
  cv.scrollToLatest();
  assert.equal(pane.scrollTop, maxTop());
  assert.equal(state.unseenEvents, 0);
  assert.equal(jump.hidden, true);
});

test('the window trims from the top at 400 + 400 events without moving the viewport', () => {
  open(1000);
  // At the bottom: 601..1000 plus 450 new = 850 rendered > 800: trimmed to the last 400, still at the bottom.
  push(450);
  assert.equal(nodes().length, 400);
  assert.equal(Number(nodes()[0].dataset.seq), 1051);
  assert.equal(more().textContent, 'Show earlier (1050 hidden)');
  assert.equal(pane.scrollTop, maxTop());
  // Scrolled up near the tail: 405 more events push the window past the limit; the trim stops above the reader's event.
  scrollTo(maxTop() - 600);
  const seq = topEvent(); const y = yOf(seq);
  const firstNode = nodes()[0];
  push(405);
  assert.ok(Number(nodes()[0].dataset.seq) > 1051, 'the trim dropped the oldest events');
  assert.ok(!nodes().includes(firstNode));
  assert.ok(nodes().some((n) => Number(n.dataset.seq) === seq - 50), 'with a little context above the reader');
  assert.equal(yOf(seq), y, 'the reader keeps their event through the trim');
  assert.equal(topEvent(), seq);
  assert.match(more().textContent, /^Show earlier \(\d+ hidden\)$/);
  assert.equal(jump.textContent, 'Jump to latest (405 new)');
});

test('a reader far above the trim point is never trimmed away: the window grows instead', () => {
  open(1000);
  scrollTo(0);
  const seq = topEvent(); const y = yOf(seq);
  push(500);
  assert.equal(nodes().length, 900, 'kept: the reader is reading the oldest loaded events');
  assert.equal(yOf(seq), y);
  assert.equal(jump.textContent, 'Jump to latest (500 new)');
});

test('a rewrite of an earlier event rebuilds the pane and still keeps a scrolled-up reader in place', () => {
  open(1000);
  scrollTo(Math.round(maxTop() / 2));
  const seq = topEvent(); const y = yOf(seq);
  state.renderedCount = 0; // what refreshEvents does when an already-seen sequence comes back changed
  cv.renderEvents();
  assert.equal(yOf(seq), y);
  assert.equal(jump.hidden, true, 'a rewrite is not a new event');
});

test('opening another session starts at the bottom with no button', () => {
  open(1000);
  scrollTo(100);
  push(2);
  assert.equal(jump.hidden, false);
  open(30);
  assert.equal(jump.hidden, true);
  assert.equal(pane.scrollTop, maxTop());
});

test('scroll-model: near-bottom threshold and window planning', () => {
  assert.equal(sm.atBottom({ scrollTop: 920, scrollHeight: 1600, clientHeight: 600 }), false, '80 px short is not at the bottom, 79 is');
  assert.equal(sm.atBottom({ scrollTop: 921, scrollHeight: 1600, clientHeight: 600 }), true);
  assert.equal(sm.atBottom({ scrollTop: 900, scrollHeight: 1600, clientHeight: 600 }), false);
  assert.equal(sm.restoredScrollTop(1000, 340, 100, 40), 1200);
  assert.equal(sm.jumpLabel(7), 'Jump to latest (7 new)');
  const base = { earlier: false, anchorIdx: null, atBottom: true };
  assert.deepEqual(sm.planWindow(1000, 600, 400, base), { start: 600, shift: 0 });
  assert.deepEqual(sm.planWindow(1400, 600, 400, base), { start: 600, shift: 0 }, 'exactly limit + slack: not yet');
  assert.deepEqual(sm.planWindow(1401, 600, 400, base), { start: 1001, shift: 401 });
  assert.deepEqual(sm.planWindow(1401, 600, 400, { ...base, atBottom: false, anchorIdx: 620 }), { start: 600, shift: 0 }, 'the reader is at the top of the window: nothing above them to drop');
  assert.deepEqual(sm.planWindow(1401, 600, 400, { ...base, atBottom: false, anchorIdx: 800 }), { start: 750, shift: 150 }, 'stops 50 events above the reader');
  assert.deepEqual(sm.planWindow(1401, 600, 400, { ...base, atBottom: false, anchorIdx: 1300 }), { start: 1001, shift: 401 }, 'never past the usual limit');
  assert.deepEqual(sm.planWindow(1000, 600, 400, { ...base, earlier: true }), { start: 200, shift: -400 });
  assert.deepEqual(sm.planWindow(1000, 100, 400, { ...base, earlier: true }), { start: 0, shift: -100 });
});

// ---- local #307 x #306: the find bar's jump and the reader's place ----
// find-run.ts's showCurrent grows the window (limitToReveal), re-renders, scrolls the match to the middle (scrollToMatch) and
// settles the pane (settleScroll). The fake pane has no scrollIntoView, so findJump does the same steps by hand.
const fm = await import('../dist/find-model.js');
const { readFileSync } = await import('node:fs');
function findJump(index) {
  const limit = fm.limitToReveal(state.events.length, index, state.renderLimit);
  if (limit !== state.renderLimit) { state.renderLimit = limit; state.renderedCount = 0; cv.renderEvents(); }
  const seq = state.events[index].sequence;
  const node = pane.querySelector(`.ev[data-seq="${seq}"]`);
  const r = node.getBoundingClientRect();
  pane.scrollTop += (r.top - PANE_TOP) - (VIEW - r.height) / 2;
  cv.settleScroll();
  return seq;
}

test('find: a jump to a match behind Show earlier lands on it, is not a new event, and a rebuild does not pull it back', () => {
  open(1000);
  assert.equal(nodes().length, 400);
  const seq = findJump(100);
  assert.equal(seq, 101);
  assert.ok(nodes().some((n) => Number(n.dataset.seq) === seq), 'the match was rendered');
  const y = yOf(seq);
  assert.ok(y > 0 && y < VIEW, `the match is in view (${y})`);
  assert.equal(state.unseenEvents, 0, 'revealing earlier events is not an arrival');
  assert.equal(jump.hidden, true);
  // A poll brings 3 real events while the reader is on the match: they are counted, the match stays where it was.
  push(3);
  assert.equal(yOf(seq), y, 'the anchor restore keeps the find jump');
  assert.equal(state.unseenEvents, 3);
  assert.equal(jump.textContent, 'Jump to latest (3 new)');
  // A rewrite of an earlier event rebuilds the pane: still on the match, and not counted.
  state.renderedCount = 0;
  cv.renderEvents();
  assert.equal(yOf(seq), y);
  assert.equal(state.unseenEvents, 3);
});

test('find: Jump to latest still works after a find jump and clears the count', () => {
  open(1000);
  const seq = findJump(150);
  push(2);
  assert.equal(jump.hidden, false);
  jump.fire('click');
  assert.equal(pane.scrollTop, maxTop());
  assert.equal(jump.hidden, true);
  assert.equal(state.unseenEvents, 0);
  push(1);
  assert.equal(pane.scrollTop, maxTop(), 'the reader is on the tail again and follows it');
  assert.ok(!(yOf(seq) > 0 && yOf(seq) < VIEW), 'the match is no longer on screen');
});

test('find: a jump keeps the count of events that arrived before it, and landing on the tail clears it', () => {
  open(1000);
  scrollTo(100);
  push(4);
  assert.equal(state.unseenEvents, 4);
  findJump(700); // already rendered (601..1004): only a scroll, no rebuild
  assert.equal(state.unseenEvents, 4, 'a jump away is not a new event and does not forget the old ones');
  assert.equal(jump.textContent, 'Jump to latest (4 new)');
  findJump(state.events.length - 1); // the newest event: the jump lands on the tail
  assert.equal(pane.scrollTop, maxTop());
  assert.equal(state.unseenEvents, 0);
  assert.equal(jump.hidden, true);
});

test('find: anything that jumps to the latest event tells the find bar to let go of its pin', () => {
  let n = 0;
  cv.onJumpLatest(() => { n += 1; });
  open(1000);
  scrollTo(100); push(1);
  jump.fire('click');
  cv.scrollToLatest();
  assert.equal(n, 2);
  const src = readFileSync(new URL('../src/actions/find-run.ts', import.meta.url), 'utf8');
  assert.match(src, /function scrollToMatch[^\n]*\n[^\n]*scrollIntoView[^\n]*\n\s*settleScroll\(\)/, 'every find scroll goes through the conversation\'s settle step');
  assert.match(src, /onJumpLatest\(\(\) => \{ f\.pinUntil = 0; \}\)/, 'Jump to latest ends the pin');
});
