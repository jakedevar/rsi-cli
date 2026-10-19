// Gap sweep G1 (hub #3000), DOM side: the vim editor in the message box and
// the overlay text fields (vim-field.ts), the input bar's keys (Submit on
// Enter, Ctrl-G, Ctrl-Y, Ctrl-Shift-G, Ctrl-Shift-A, slash suggestions, the
// correction preview, clipboard image paste), the TUI's global intercepts
// (jumplist, pane focus and zones, overlay geometry, sidebar width, the
// transcript column, session stepping, Ctrl-Alt-G anywhere), modal toggles,
// the daemon-config gate, recent files and the operator-message commands.
// Rules are input-model.ts / vim-editor.ts (pure, tested); registry entries
// actions/input-spec.ts. Daemon and file text is set with textContent only.
import { sendMessage } from '../actions.js';
import { invoke } from '../api.js';
import { type Invocation, type SliceRuntime, dispatch, snapshot } from '../commands.js';
import { $, el, toast } from '../dom.js';
import { openAbsoluteFile } from '../files.js';
import {
  type ComposerState, type Correction, type ConfigState, type Geometry, type GlobalAction, type SlashCommand, type SlashRoot, Jumplist,
  NO_GEOMETRY, SIDEBAR_DEFAULT, acceptSlash, columnStep, compileFailure, compileOutcome, configGate, filterSlash,
  dataUrlBase64, geometryStep, imageReference, moveSuggestion, paneStep, parseGeometry, previewKey, recentFiles,
  pasteImageMime, relativeTo, routeGlobal, sidebarStep, slashQuery, slashRoot, slashRootKey, submitKey, toggleSequence,
} from '../input-model.js';
import { isTypingTarget, keyToken } from '../keyengine.js';
import { isKeyHelpOpen, openKeyHelp } from '../keyhelp.js';
import { stepZone, zoneLabel } from '../nav-model.js';
import { runOperatorLine, initPendingCount } from '../operator-messages.js';
import { closePalette, isPaletteOpen } from '../palette.js';
import { renderSessionList, select, visibleRows } from '../sessions.js';
import { collapsedIds, setCollapsed } from '../sessionTree.js';
import { currentSessions, isReadOnly, onUiChange, selected, state } from '../state.js';
import { isContainer } from '../tree.js';
import { VimEditor, type VimMode } from '../vim-editor.js';
import { type VimField, enterInsert, isSingleLine, isVimField, modeOf, onVimMode, prepareForTyping, resetField, vimKey } from '../vim-field.js';
import { currentPlatform, isVimKeysOn } from '../vimkeys.js';
import { detailFocus, setDetailFocus } from './detail.js';
import { type InputActionId, type InputSnapshot, SLICE } from './input-spec.js';
import { focusOwnsKey } from '../modal-gate.js';

// ---------- local settings ----------

const SUBMIT_KEY = 'rsi.desktop.submitOnEnter';
const SIDEBAR_KEY = 'rsi.desktop.sidebarWidth';
const COLUMN_KEY = 'rsi.desktop.transcriptColumn';
const GEOMETRY_PREFIX = 'rsi.desktop.geometry.';

function store(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}
const getItem = (k: string): string | null => { try { return store()?.getItem(k) ?? null; } catch { return null; } };
const setItem = (k: string, v: string | null): void => {
  try { if (v === null) store()?.removeItem(k); else store()?.setItem(k, v); } catch { /* private mode: this run only */ }
};

let submitOnEnter = getItem(SUBMIT_KEY) !== 'off';

// ---------- the message box ----------

const composer = (): HTMLTextAreaElement => $<HTMLTextAreaElement>('message');

/** Where the keys are for the message box (input-model.ts ComposerState). */
function composerState(active: Element | null): ComposerState {
  if (active !== composer()) return 'none';
  if (!isVimKeysOn()) return 'standard';
  return modeOf(composer()) === 'insert' ? 'insert' : 'normal';
}

/** A field focused with a mode request (i/a/o/O) takes it on focusin. */
let entry: { el: VimField; how: 'i' | 'a' | 'A' | 'I' | 'o' | 'O' } | null = null;

/**
 * `i` / `a` / `o` / `O` (the core focus-composer action): from the list the
 * session under the cursor opens first; the box takes the key's cursor
 * place (insert at the cursor, append, open a line below / above).
 */
export function focusComposer(arg: string | undefined): void {
  if (!selected()) {
    const cur = state.cursorId;
    if (cur) select(cur);
  }
  const box = composer();
  if (box.disabled) return;
  const how = (['i', 'a', 'o', 'O'].includes(arg ?? '') ? arg : 'i') as 'i' | 'a' | 'o' | 'O';
  setDetailFocus('transcript');
  if (isVimKeysOn()) {
    entry = { el: box, how };
    if (document.activeElement === box) { enterInsert(box, how); entry = null; } else box.focus();
    return;
  }
  // Standard editing has no modes; the key still places the caret.
  const ed = new VimEditor(box.value, 'normal');
  ed.setText(box.value, box.selectionStart ?? box.value.length);
  ed.enterInsert(how);
  if (ed.text !== box.value) { box.value = ed.text; box.dispatchEvent(new Event('input', { bubbles: true })); }
  box.focus();
  box.setSelectionRange(ed.cursorOffset(), ed.cursorOffset());
}

function modeBadge(): HTMLElement {
  let b = document.getElementById('g1-mode');
  if (!b) {
    b = el('span', 'g1-mode');
    b.id = 'g1-mode';
    b.setAttribute('aria-live', 'polite');
    composer().before(b);
  }
  return b;
}

const MODE_TEXT: Readonly<Record<VimMode, string>> = { insert: 'INSERT', normal: 'NORMAL', visual: 'VISUAL', 'visual-line': 'V-LINE' };

/** The input bar's mode before the text (the TUI shows NORMAL / INSERT before `›`). */
function renderBadge(): void {
  const b = modeBadge();
  const box = composer();
  const corr = correction.inFlight ? 'CORRECTING…' : '';
  if (!isVimKeysOn()) { b.textContent = corr; b.hidden = corr === ''; return; }
  const focused = document.activeElement === box;
  const mode: VimMode = focused ? modeOf(box) : 'normal';
  b.textContent = corr || MODE_TEXT[mode];
  b.dataset.mode = corr ? 'busy' : mode;
  b.hidden = false;
}

/**
 * The TUI shows the editing mode (NORMAL / INSERT / VISUAL) for every field;
 * a field in a dialog gets the same tag after it while it has focus.
 */
function renderFieldTag(f: VimField, show: boolean): void {
  if (f === composer()) return;
  const next = f.nextElementSibling;
  let tag = next instanceof HTMLElement && next.classList.contains('vim-mode-tag') ? next : null;
  if (!show || !isVimKeysOn() || !f.closest('dialog, #settings-view')) { tag?.remove(); return; }
  if (!tag) {
    tag = el('span', 'vim-mode-tag');
    tag.setAttribute('aria-label', 'Editing mode');
    f.after(tag);
  }
  const mode = modeOf(f);
  tag.textContent = MODE_TEXT[mode];
  tag.dataset.mode = mode;
}

// ---------- slash suggestions ----------

const slashCache = new Map<string, SlashCommand[] | 'loading'>();
const sugg = { field: null as VimField | null, items: [] as SlashCommand[], at: 0, dismissed: '' };

function slashRootFor(field: VimField): SlashRoot | null {
  if (field.closest('#launch-dialog')) {
    const form = $<HTMLFormElement>('launch-form');
    const workingDir = (form.elements.namedItem('working_dir') as HTMLInputElement | null)?.value ?? '';
    const projectId = (form.elements.namedItem('project_id') as HTMLSelectElement | null)?.value ?? '';
    return slashRoot({ workingDir, projectId }, state.projects, null);
  }
  return slashRoot(null, state.projects, selected()?.id ?? null);
}

const suggestable = (f: Element | null): f is HTMLTextAreaElement =>
  f instanceof HTMLTextAreaElement && !!f.closest('#composer, #continue-dialog, #launch-dialog, #g1-modal-dialog');

function popup(): HTMLElement {
  let p = document.getElementById('g1-suggest');
  if (!p) {
    p = el('div', 'g1-suggest');
    p.id = 'g1-suggest';
    p.setAttribute('role', 'listbox');
    p.hidden = true;
    document.body.append(p);
  }
  return p;
}

function hideSuggestions(): void {
  sugg.field = null;
  sugg.items = [];
  popup().hidden = true;
}

function updateSuggestions(field: Element | null): void {
  if (!suggestable(field) || (isVimKeysOn() && modeOf(field) !== 'insert')) { hideSuggestions(); return; }
  const q = slashQuery(field.value, field.selectionStart ?? 0);
  const root = slashRootFor(field);
  const key = root ? slashRootKey(root) : null;
  if (q === null || !root || !key || `${key}\n${field.value}` === sugg.dismissed) { hideSuggestions(); return; }
  const have = slashCache.get(key);
  if (have === undefined) {
    slashCache.set(key, 'loading');
    // hub #3044: rsid lists the root's commands and skills; the desktop reads no file.
    invoke<{ commands?: SlashCommand[] }>('list_slash_commands', { request: root }).then(
      (listing) => {
        slashCache.set(key, Array.isArray(listing?.commands) ? listing.commands : []);
        updateSuggestions(document.activeElement);
      },
      () => slashCache.set(key, []),
    );
    return;
  }
  if (have === 'loading') return;
  const items = filterSlash(have, q).slice(0, 12);
  if (items.length === 0) { hideSuggestions(); return; }
  if (sugg.field !== field || sugg.items.length !== items.length) sugg.at = 0;
  sugg.field = field;
  sugg.items = items;
  drawSuggestions();
}

function drawSuggestions(): void {
  const p = popup();
  const f = sugg.field;
  if (!f) { p.hidden = true; return; }
  p.replaceChildren(...sugg.items.map((c, i) => {
    const row = el('div', `g1-suggest-row${i === sugg.at ? ' sel' : ''}`);
    row.setAttribute('role', 'option');
    row.append(el('span', 'k', `/${c.name}`), el('span', 't', c.description));
    row.addEventListener('mousedown', (ev) => { ev.preventDefault(); sugg.at = i; acceptSuggestion(); });
    return row;
  }));
  const r = f.getBoundingClientRect();
  p.hidden = false;
  const h = p.offsetHeight;
  p.style.left = `${Math.round(r.left)}px`;
  p.style.width = `${Math.round(Math.min(r.width, 520))}px`;
  p.style.top = `${Math.round(r.top - h - 4 >= 0 ? r.top - h - 4 : r.bottom + 4)}px`;
}

function acceptSuggestion(): void {
  const f = sugg.field;
  const c = sugg.items[sugg.at];
  if (!f || !c) return;
  const r = acceptSlash(f.value, f.selectionStart ?? 0, c.name);
  f.value = r.text;
  f.setSelectionRange(r.cursor, r.cursor);
  f.dispatchEvent(new Event('input', { bubbles: true }));
  hideSuggestions();
}

/** Tab accepts, Down / Ctrl-N and Up / Ctrl-P move, Esc dismisses. */
function suggestionKey(ev: KeyboardEvent): boolean {
  if (!sugg.field || document.activeElement !== sugg.field || popup().hidden) return false;
  const plain = !ev.altKey && !ev.metaKey && !ev.shiftKey;
  let handled = true;
  if (ev.key === 'Tab' && plain && !ev.ctrlKey) acceptSuggestion();
  else if ((ev.key === 'ArrowDown' && plain && !ev.ctrlKey) || (ev.ctrlKey && plain && ev.key.toLowerCase() === 'n')) {
    sugg.at = moveSuggestion(sugg.items.length, sugg.at, 1); drawSuggestions();
  } else if ((ev.key === 'ArrowUp' && plain && !ev.ctrlKey) || (ev.ctrlKey && plain && ev.key.toLowerCase() === 'p')) {
    sugg.at = moveSuggestion(sugg.items.length, sugg.at, -1); drawSuggestions();
  } else if (ev.key === 'Escape' && plain && !ev.ctrlKey) {
    const root = slashRootFor(sugg.field);
    sugg.dismissed = `${root ? slashRootKey(root) : ''}\n${sugg.field.value}`;
    hideSuggestions();
  } else handled = false;
  if (handled) { ev.preventDefault(); ev.stopPropagation(); }
  return handled;
}

// ---------- correction preview ----------

const correction = { inFlight: false, preview: null as Correction | null, epoch: 0 };

function previewPanel(): HTMLElement {
  let p = document.getElementById('g1-correction');
  if (!p) {
    p = el('div', 'g1-correction');
    p.id = 'g1-correction';
    p.hidden = true;
    $('composer').before(p);
  }
  return p;
}

function renderPreview(): void {
  const p = previewPanel();
  const c = correction.preview;
  renderBadge();
  if (!c) { p.hidden = true; p.replaceChildren(); return; }
  const title = c.clarify ? 'The compiler needs clarification' : c.kind === 'compile' ? 'Compiled prompt' : 'Grammar fix';
  const btn = (text: string, cls: string, fn: () => void): HTMLButtonElement => {
    const b = el('button', cls, text);
    b.type = 'button';
    b.addEventListener('click', fn);
    return b;
  };
  const acts = el('div', 'actions');
  if (!c.clarify) acts.append(btn('Accept (a)', 'btn primary', () => previewAction('accept')));
  acts.append(
    btn('Discard (d / Esc)', 'btn', () => previewAction('discard')),
    btn('Send original (Ctrl+Enter)', 'btn', () => previewAction('send-original')),
  );
  p.replaceChildren(el('div', 'g1-correction-title', title), el('pre', 'g1-correction-text', c.text), acts);
  p.hidden = false;
}

function previewAction(a: ReturnType<typeof previewKey>): void {
  const c = correction.preview;
  switch (a) {
    case 'accept': {
      if (!c || c.clarify) return;
      const box = composer();
      box.value = c.text;
      box.dispatchEvent(new Event('input', { bubbles: true }));
      correction.preview = null;
      break;
    }
    case 'discard': correction.preview = null; break;
    case 'send-original': correction.preview = null; void sendMessage(); break;
    case 'compile': case 'grammar': void runCorrection(a); return;
    default: return;
  }
  renderPreview();
}

/** Ctrl-Y (compile) / Ctrl-Shift-G (grammar): the draft goes to the prompt processor; a preview comes back. */
async function runCorrection(kind: 'compile' | 'grammar'): Promise<void> {
  const draft = composer().value;
  if (!draft.trim()) { toast('The message box is empty'); return; }
  const mine = ++correction.epoch;
  correction.inFlight = true;
  correction.preview = null;
  renderPreview();
  try {
    if (kind === 'compile') {
      const out = compileOutcome(await invoke<unknown>('compile_prompt', { input: draft }));
      if (mine !== correction.epoch) return;
      correction.preview = out.preview;
      if (out.notice) toast(out.notice);
    } else {
      const r = await invoke<{ text: string }>('generate_text', { draft, question: null });
      if (mine !== correction.epoch) return;
      correction.preview = { text: r.text, clarify: false, kind: 'grammar' };
    }
  } catch (e) {
    if (mine !== correction.epoch) return;
    if (kind === 'compile') {
      const out = compileFailure(String(e));
      correction.preview = out.preview;
      if (out.notice) toast(out.notice);
    } else {
      toast(`Grammar fix failed: ${String(e)}`);
    }
  } finally {
    if (mine === correction.epoch) correction.inFlight = false;
    renderPreview();
  }
}

// ---------- dialogs: input modal, recent files ----------

function dialog(id: string, cls = 'detail-dialog'): HTMLDialogElement {
  let dlg = document.getElementById(id) as HTMLDialogElement | null;
  if (!dlg) {
    dlg = document.createElement('dialog');
    dlg.id = id;
    dlg.className = cls;
    dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg!.close(); });
    document.body.append(dlg);
  }
  dlg.onclose = null;
  return dlg;
}

/** Ctrl-G: the draft in a large editor; closing keeps the text, Ctrl+Enter sends it. */
function openInputModal(): void {
  const box = composer();
  const dlg = dialog('g1-modal-dialog', 'detail-dialog g1-modal');
  const area = el('textarea');
  area.value = box.value;
  area.setAttribute('aria-label', 'Message');
  let send = false;
  area.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (submitKey(ev, false)) { ev.preventDefault(); send = true; dlg.close(); }
  });
  dlg.onclose = () => {
    if (box.value !== area.value) { box.value = area.value; box.dispatchEvent(new Event('input', { bubbles: true })); }
    if (send) void sendMessage(); else box.focus();
  };
  dlg.replaceChildren(
    el('h2', undefined, 'Message'),
    area,
    el('div', 'dialog-note', 'Ctrl+Enter sends. Esc (in Vim keys: Esc then Esc or q) closes and keeps the text in the message box. Ctrl+Arrow moves this window, Ctrl+Shift+Arrow resizes it, Ctrl+0 resets it.'),
  );
  dlg.showModal();
  area.focus();
}

function openRecent(arg: string | undefined): void {
  const files = recentFiles(state.events);
  const open = (path: string): void => {
    const why = openAbsoluteFile(path, relativeTo);
    if (why) toast(why);
  };
  if (arg) {
    const n = Number(arg);
    const path = files[n - 1];
    if (!path) { toast(`No recent file ${n}: this session has ${files.length}`); return; }
    open(path);
    return;
  }
  const dlg = dialog('g1-recent-dialog');
  const list = el('ul', 'pick-list');
  list.setAttribute('role', 'listbox');
  files.forEach((f, i) => {
    const li = el('li', 'pick-item');
    li.append(el('span', 'k', `gf${i + 1}`), el('span', 'mono', f));
    li.addEventListener('click', () => { dlg.close(); open(f); });
    list.append(li);
  });
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const n = Number(ev.key);
    if (Number.isInteger(n) && n >= 1 && files[n - 1]) { ev.preventDefault(); dlg.close(); open(files[n - 1]!); }
    else if (ev.key === 'q') { ev.preventDefault(); dlg.close(); }
  };
  dlg.replaceChildren(el('h2', undefined, 'Recent files'), el('div', 'dialog-note', '1-9 open that file in the viewer; q or Esc closes.'), list);
  dlg.showModal();
  dlg.tabIndex = -1;
  dlg.focus();
}

// ---------- global intercepts ----------

const jumplist = new Jumplist();
let jumping = false;
let lastSelected: string | null = null;
const exists = (id: string): boolean => currentSessions().some((s) => s.id === id);

function jump(dir: -1 | 1): void {
  const id = jumplist.jump(dir, exists);
  if (!id) { toast(dir < 0 ? 'No older session in the jumplist' : 'No newer session in the jumplist'); return; }
  jumping = true;
  try { select(id); } finally { jumping = false; }
}

/** Something other than the session list and detail covers the detail pane (issues, settings, files, terminal). */
function detailCovered(): boolean {
  const main = document.querySelector<HTMLElement>('main.detail');
  if (!main) return false;
  const r = main.getBoundingClientRect();
  const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
  return !!hit && !hit.closest('#session-view, #empty');
}

function focusPane(to: 'list' | 'detail'): void {
  setDetailFocus(to === 'list' ? 'list' : 'transcript');
  if (to === 'list' && document.activeElement === composer()) composer().blur();
  markPane();
}

function markPane(): void {
  const f = detailFocus();
  document.querySelector('.sidebar')?.classList.toggle('g1-pane-focus', f === 'list');
  document.querySelector('main.detail')?.classList.toggle('g1-pane-focus', f === 'transcript');
}

function applySidebar(width: number | null): void {
  const bar = document.querySelector<HTMLElement>('.sidebar');
  if (!bar) return;
  bar.style.width = width === null ? '' : `${width}px`;
  bar.style.minWidth = width === null ? '' : `${Math.min(width, 260)}px`;
}

function stepSidebar(dir: -1 | 1): void {
  const bar = document.querySelector<HTMLElement>('.sidebar');
  const cur = bar?.getBoundingClientRect().width || SIDEBAR_DEFAULT;
  const w = sidebarStep(Math.round(cur), dir, window.innerWidth);
  applySidebar(w);
  setItem(SIDEBAR_KEY, String(w));
}

let column = Number(getItem(COLUMN_KEY)) || 0;

function applyColumn(): void {
  $('events').style.setProperty('--g1-col', `${column}px`);
  $('events').classList.toggle('g1-column', column > 0);
}

function stepColumn(dir: -1 | 1): void {
  const was = column;
  column = columnStep(column, dir, $('events').getBoundingClientRect().width);
  setItem(COLUMN_KEY, String(column));
  applyColumn();
  if (column === 0 && was !== 0) toast('Transcript column: Left Aligned', true);
}

/** Left / Right step the list cursor; with Shift they open it (a Group or Epic open in detail unfolds instead). */
function stepSession(dir: -1 | 1, open: boolean): void {
  const s = selected();
  if (open && s && isContainer(s) && detailFocus() === 'transcript' && collapsedIds().has(s.id)) {
    const next = new Set(collapsedIds());
    next.delete(s.id);
    setCollapsed(next);
    renderSessionList();
    return;
  }
  const ids = visibleRows().map((r) => r.session.id);
  const at = state.cursorId ?? state.selectedId;
  const i = at === null ? -1 : ids.indexOf(at);
  const to = ids[i < 0 ? (dir > 0 ? 0 : ids.length - 1) : Math.max(0, Math.min(ids.length - 1, i + dir))] ?? null;
  if (!to) return;
  state.cursorId = to;
  renderSessionList();
  document.querySelector<HTMLElement>(`#session-list .session-item[data-id="${CSS.escape(to)}"]`)?.scrollIntoView({ block: 'nearest' });
  if (open) select(to);
}

function topDialog(): HTMLDialogElement | null {
  const open = [...document.querySelectorAll<HTMLDialogElement>('dialog[open]')];
  return open[open.length - 1] ?? null;
}

const geometryKey = (d: HTMLDialogElement): string => `${GEOMETRY_PREFIX}${d.id || d.className || 'dialog'}`;

function applyGeometry(d: HTMLDialogElement, g: Geometry): void {
  d.style.translate = g.dx || g.dy ? `${g.dx}px ${g.dy}px` : '';
  d.style.width = g.width === null ? '' : `${g.width}px`;
  d.style.height = g.height === null ? '' : `${g.height}px`;
  d.style.maxWidth = g.width === null ? '' : 'none';
  d.style.maxHeight = g.height === null ? '' : 'none';
}

function stepGeometry(op: Parameters<typeof geometryStep>[1]): void {
  const d = topDialog();
  if (!d) return;
  const g = op === 'reset' ? NO_GEOMETRY : parseGeometry(getItem(geometryKey(d)));
  const r = d.getBoundingClientRect();
  const next = geometryStep(g, op, { width: r.width, height: r.height }, { width: window.innerWidth, height: window.innerHeight });
  applyGeometry(d, next);
  setItem(geometryKey(d), op === 'reset' ? null : JSON.stringify(next));
}

function runGlobal(a: GlobalAction): void {
  switch (a.kind) {
    case 'geometry': stepGeometry(a.op); return;
    case 'jump': jump(a.dir); return;
    case 'zone': {
      // One zone cycle for both sweeps: Main → Jobs → Archived (G2's Jobs zone, nav-model.ts stepZone).
      const to = stepZone(state.view === 'archived' ? 'archived' : state.zone, a.dir);
      dispatch(to === 'main' ? 'view-active' : to === 'jobs' ? 'view-jobs' : 'view-archived', { source: 'key' });
      if (to !== 'jobs') toast(zoneLabel(to), true);
      return;
    }
    case 'pane': {
      const to = paneStep(detailFocus() === 'list' && document.activeElement !== composer() ? 'list' : 'detail', a.dir, !!selected());
      if (to) focusPane(to);
      return;
    }
    case 'sidebar': stepSidebar(a.dir); return;
    case 'column': stepColumn(a.dir); return;
    case 'step': stepSession(a.dir, a.open); return;
  }
}

// ---------- modal toggles ----------

const recent: Array<{ t: string; at: number }> = [];
const toggles = new WeakMap<HTMLDialogElement, readonly string[]>();
const inDialog = new WeakMap<HTMLDialogElement, string[]>();

function noteKey(token: string): void {
  recent.push({ t: token, at: Date.now() });
  if (recent.length > 4) recent.shift();
}

function dialogOpened(d: HTMLDialogElement): void {
  const last = recent[recent.length - 1];
  if (last && Date.now() - last.at < 600) {
    const seq = toggleSequence(recent.map((r) => r.t));
    if (seq) toggles.set(d, seq); else toggles.delete(d);
  } else toggles.delete(d);
  inDialog.set(d, []);
  const g = parseGeometry(getItem(geometryKey(d)));
  if (g !== NO_GEOMETRY) applyGeometry(d, g);
}

/** The key that opened a dialog pressed again inside it closes it (keybindings.md "Modal Toggle Behavior"). */
function toggleKey(d: HTMLDialogElement, token: string, typing: boolean): boolean {
  const seq = toggles.get(d);
  if (!seq) return false;
  if (typing && !token.startsWith('C-')) return false;
  const keys = inDialog.get(d) ?? [];
  keys.push(token);
  if (keys.length > 4) keys.shift();
  inDialog.set(d, keys);
  if (toggleSequence(keys) !== seq) return false;
  d.close();
  return true;
}

// ---------- the key listener ----------

const isHelpChord = (ev: KeyboardEvent): boolean =>
  ev.ctrlKey && ev.altKey && !ev.metaKey && (ev.code === 'KeyG' || ev.key.toLowerCase() === 'g');

const isPasteChord = (ev: KeyboardEvent): boolean =>
  !ev.altKey && !ev.shiftKey && (ev.key.toLowerCase() === 'v' || ev.code === 'KeyV')
  && (currentPlatform() === 'mac' ? ev.metaKey !== ev.ctrlKey : ev.ctrlKey && !ev.metaKey);

/** Composer-only chords: Submit on Enter, Ctrl-G, Ctrl-Y, Ctrl-Shift-G, insert-mode (Ctrl-Shift-A is the assist slice's AI chat) Up/Down. */
function composerChord(ev: KeyboardEvent, cs: ComposerState): boolean {
  const ctrl = ev.ctrlKey || (currentPlatform() === 'mac' && ev.metaKey);
  const k = ev.key.toLowerCase();
  let run: InputActionId | null = null;
  if (ctrl && !ev.altKey && !ev.shiftKey && k === 'g') run = 'input-modal';
  else if (ctrl && !ev.altKey && !ev.shiftKey && k === 'y') run = 'compile-prompt';
  else if (ctrl && !ev.altKey && ev.shiftKey && k === 'g') run = 'fix-grammar';
  if (run) {
    ev.preventDefault();
    ev.stopPropagation();
    dispatch(run, { source: 'key' });
    return true;
  }
  if ((cs === 'insert' || cs === 'standard') && ev.key === 'Enter' && !ev.ctrlKey && !ev.metaKey && submitKey(ev, submitOnEnter)) {
    ev.preventDefault();
    ev.stopPropagation();
    if (correction.preview) { correction.preview = null; renderPreview(); }
    void sendMessage();
    return true;
  }
  if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey) && correction.preview) {
    correction.preview = null; // main.ts sends the original
    renderPreview();
    return false;
  }
  // keybindings.md Insert Mode: Up / Down scroll the detail (bypassing the textarea).
  if (cs === 'insert' && (ev.key === 'ArrowUp' || ev.key === 'ArrowDown') && !ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey) {
    ev.preventDefault();
    ev.stopPropagation();
    dispatch(ev.key === 'ArrowUp' ? 'scroll-up' : 'scroll-down', { source: 'key' });
    return true;
  }
  return false;
}

function copyYank(text: string): void {
  void navigator.clipboard?.writeText(text).catch(() => undefined);
}

/** A vim field's key; true when it was the editor's. */
function fieldKey(ev: KeyboardEvent, field: VimField): boolean {
  if (['Shift', 'Control', 'Alt', 'Meta', 'CapsLock', 'AltGraph'].includes(ev.key)) return false;
  const isComposer = field === composer();
  const plain = !ev.ctrlKey && !ev.altKey && !ev.metaKey;
  if (isComposer && modeOf(field) !== 'insert' && plain) {
    // Normal mode with an empty draft: every key belongs to the session detail.
    if (field.value === '' && ev.key !== 'Escape') { field.blur(); return false; }
    // A one-line draft: Up / Down still scroll the transcript.
    if ((ev.key === 'ArrowUp' || ev.key === 'ArrowDown') && !field.value.includes('\n')) return false;
  }
  const r = vimKey(field, ev, currentPlatform());
  if (r.yank) copyYank(r.yank);
  if (r.outcome === 'pass') return false;
  if (r.outcome !== 'unhandled') {
    ev.preventDefault();
    ev.stopPropagation();
    if (isComposer) renderBadge();
    return true;
  }
  const t = r.token ?? '';
  const chord = /^[CA]-/.test(t);
  if (isComposer) {
    // An unknown key passes through to the session detail (keybindings.md Input Bar).
    if (!chord) field.blur();
    return false;
  }
  if (chord || t === 'Tab' || t === 'S-Tab' || t === 'Esc') return false;
  // A one-line field's Enter still submits its form from normal mode.
  if (t === 'Enter' && isSingleLine(field)) return false;
  if (t === 'q') {
    field.closest('dialog')?.close();
    ev.preventDefault();
    ev.stopPropagation();
    return true;
  }
  // Normal mode never types into the field.
  ev.preventDefault();
  return t !== 'Space';
}

function onKeyDown(ev: KeyboardEvent): void {
  if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
  if (ev.defaultPrevented || ev.isComposing) return;
  const active = document.activeElement as HTMLElement | null;
  if (active?.closest('.xterm, .terminal-overlay')) return;
  // Ctrl-Alt-G: help, from anywhere and without typing into the field.
  if (isHelpChord(ev)) {
    ev.preventDefault();
    ev.stopPropagation();
    const help = document.getElementById('keys-help') as HTMLDialogElement | null;
    if (isKeyHelpOpen()) help?.close();
    else if (isPaletteOpen()) closePalette();
    else if (topDialog()) openKeyHelp(snapshot());
    else dispatch('help', { source: 'key' });
    return;
  }
  const dlg = topDialog();
  const typing = isTypingTarget(active);
  // Ctrl-V with no text field: the paste goes to the input bar (or the overlay's field).
  if (isPasteChord(ev)) {
    routePaste(active, dlg, typing);
    return;
  }
  if (suggestionKey(ev)) return;
  const cs = composerState(active);
  if (correction.preview && (cs !== 'none' || !typing)) {
    const token = keyToken(ev, currentPlatform());
    const plainKey = token !== null && !/^[CAS]-/.test(token) && token !== 'Esc';
    const k = token && (cs !== 'insert' || !plainKey) ? previewKey(token, correction.preview) : null;
    if (k && k !== 'send-original') {
      ev.preventDefault();
      ev.stopPropagation();
      previewAction(k);
      return;
    }
  }
  if (cs !== 'none' && composerChord(ev, cs)) return;
  if (isVimKeysOn() && isVimField(active) && fieldKey(ev, active)) return;
  const token = keyToken(ev, currentPlatform());
  if (token === null) return;
  if (dlg && toggleKey(dlg, token, typing)) { ev.preventDefault(); ev.stopPropagation(); return; }
  const now = document.activeElement as HTMLElement | null;
  const g = routeGlobal(token, {
    overlay: !!dlg,
    typing: isTypingTarget(now) && now !== composer(),
    composer: composerState(now),
    focus: detailFocus() === 'list' && now !== composer() ? 'list' : 'detail',
    sessionOpen: !!selected(),
    physicalCtrl: ev.ctrlKey && !ev.metaKey,
  });
  if (g && (g.kind === 'geometry' || !detailCovered())) {
    ev.preventDefault();
    ev.stopPropagation();
    runGlobal(g);
    return;
  }
  if (!dlg && !typing) noteKey(token);
}

// ---------- paste ----------

/** Ctrl-V outside a text field: focus the input bar (or the overlay's first field) so the paste lands there. */
function routePaste(active: HTMLElement | null, dlg: HTMLDialogElement | null, typing: boolean): void {
  if (typing) {
    if (isVimField(active)) prepareForTyping(active);
    return;
  }
  if (document.getElementById('cmdline')?.hidden === false) return; // the `:` line takes the paste
  const target = dlg
    ? dlg.querySelector<HTMLTextAreaElement | HTMLInputElement>('textarea:not([disabled]), input[type=text]:not([disabled])')
    : selected() && !isReadOnly(selected()) && !composer().disabled ? composer() : null;
  if (!target) return;
  target.focus();
  if (isVimField(target)) { entry = null; enterInsert(target, 'A'); }
}

/** Read a clipboard file as base64 (a `data:` URL, its payload kept). */
function fileBase64(file: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error('cannot read the clipboard image'));
    reader.onload = () => {
      const data = typeof reader.result === 'string' ? dataUrlBase64(reader.result) : null;
      if (data) resolve(data); else reject(new Error('the clipboard image is empty'));
    };
    reader.readAsDataURL(file);
  });
}

/**
 * An image on the clipboard goes to rsid (`UploadPasteImage`, hub #3044),
 * which stores it under ~/.rsi/paste/rsid/ on the daemon host; the returned
 * path is inserted as `@path`.
 */
function onPaste(ev: ClipboardEvent): void {
  const field = document.activeElement;
  if (!(field instanceof HTMLTextAreaElement || field instanceof HTMLInputElement) || !isTypingTarget(field)) return;
  const items = [...(ev.clipboardData?.items ?? [])];
  const img = items.find((i) => i.kind === 'file' && pasteImageMime(i.type) !== null);
  if (!img) { if (isVimField(field)) prepareForTyping(field); return; }
  const mime = pasteImageMime(img.type)!;
  const file = img.getAsFile();
  if (!file) return;
  ev.preventDefault();
  ev.stopPropagation();
  if (isVimField(field)) prepareForTyping(field);
  void fileBase64(file).then(
    (data) => invoke<{ path: string }>('upload_paste_image', { request: { mime, data_base64: data } }),
  ).then((r) => {
    const ref = imageReference(r.path);
    const at = field.selectionStart ?? field.value.length;
    field.setRangeText(ref, at, field.selectionEnd ?? at, 'end');
    field.dispatchEvent(new Event('input', { bubbles: true }));
    toast(`Image saved to ${r.path}`, true);
  }, (e: unknown) => toast(`Image paste failed: ${String(e)}`));
}

// ---------- daemon config gate ----------

let config: ConfigState = { kind: 'loading' };

function pollConfig(): void {
  invoke<unknown>('get_daemon_config').then(
    () => { config = { kind: 'ready' }; window.setTimeout(pollConfig, 60_000); },
    (e: unknown) => { config = { kind: 'error', error: String(e) }; window.setTimeout(pollConfig, 3_000); },
  );
}

/** Launch and create submissions wait for an authoritative daemon config; the draft stays. */
function onSubmit(ev: SubmitEvent): void {
  const form = ev.target as HTMLFormElement | null;
  if (!form || !(form.id === 'launch-form' || form.dataset.configGate !== undefined)) return;
  const why = configGate(config);
  if (!why) return;
  ev.preventDefault();
  ev.stopImmediatePropagation();
  toast(why);
  if (config.kind === 'error') pollConfig();
}

// ---------- registry ----------

export function inputSnapshot(): InputSnapshot {
  return {
    recentFiles: selected() ? recentFiles(state.events).length : 0,
    draft: composer().value.trim() !== '',
    submitOnEnter,
    back: jumplist.canJump(-1, exists),
    forward: jumplist.canJump(1, exists),
  };
}

export function runInput(id: InputActionId, inv: Invocation): void {
  switch (id) {
    case 'open-recent-file': openRecent(inv.arg); return;
    case 'jump-back': jump(-1); return;
    case 'jump-forward': jump(1); return;
    case 'pane-left': focusPane('list'); return;
    case 'pane-right': if (selected()) focusPane('detail'); return;
    case 'sidebar-wider': stepSidebar(1); return;
    case 'sidebar-narrower': stepSidebar(-1); return;
    case 'column-left': stepColumn(-1); return;
    case 'column-right': stepColumn(1); return;
    case 'pending-messages': runOperatorLine('pending', inv.arg); return;
    case 'drain-restart': runOperatorLine('drain', inv.arg); return;
    case 'toggle-submit-on-enter':
      submitOnEnter = !submitOnEnter;
      setItem(SUBMIT_KEY, submitOnEnter ? 'on' : 'off');
      composer().placeholder = composerHint();
      toast(submitOnEnter ? 'Enter sends; Shift+Enter adds a line' : 'Ctrl+Enter sends; Enter adds a line', true);
      return;
    case 'input-modal': openInputModal(); return;
    case 'compile-prompt': void runCorrection('compile'); return;
    case 'fix-grammar': void runCorrection('grammar'); return;
  }
}

const composerHint = (): string => (submitOnEnter
  ? 'Message this session (Enter sends, Shift+Enter adds a line)'
  : 'Message this session (Ctrl+Enter to send)');

const INPUT_CSS = `
.g1-mode { font: 11px var(--mono, monospace); color: var(--subtext); padding: 0 6px; align-self: center; }
.g1-mode[data-mode=normal] { color: var(--peach, #fab387); }
.g1-mode[data-mode=visual], .g1-mode[data-mode=visual-line] { color: var(--mauve, #cba6f7); }
.g1-mode[data-mode=busy] { color: var(--yellow, #f9e2af); }
.vim-mode-tag { font: 10px var(--mono, monospace); color: var(--subtext); margin-left: 6px; }
.vim-mode-tag[data-mode=normal] { color: var(--peach, #fab387); }
.vim-mode-tag[data-mode=visual], .vim-mode-tag[data-mode=visual-line] { color: var(--mauve, #cba6f7); }
textarea[data-vim-mode=normal], textarea[data-vim-mode=visual], textarea[data-vim-mode=visual-line],
input[data-vim-mode=normal] { caret-color: transparent; outline: 1px solid var(--peach, #fab387); }
.g1-suggest { position: fixed; z-index: 1000; background: var(--mantle); border: 1px solid var(--surface1);
  border-radius: 6px; max-height: 260px; overflow-y: auto; font-size: 12px; box-shadow: 0 4px 16px rgba(0,0,0,.35); }
.g1-suggest-row { display: grid; grid-template-columns: max-content 1fr; gap: 10px; padding: 3px 8px; cursor: pointer; }
.g1-suggest-row.sel { background: var(--surface1); }
.g1-suggest-row .k { font-family: var(--mono, monospace); color: var(--blue); }
.g1-suggest-row .t { color: var(--subtext); }
.g1-correction { margin: 0 12px; padding: 8px 10px; border: 1px solid var(--surface1); border-radius: 8px; background: var(--mantle); }
.g1-correction-title { font-size: 12px; color: var(--subtext); margin-bottom: 4px; }
.g1-correction-text { white-space: pre-wrap; max-height: 30vh; overflow-y: auto; margin: 0 0 6px; }
.meta-pending { color: var(--peach, #fab387); }
.g1-pending .g1-pending-head { display: flex; gap: 10px; align-items: baseline; font-size: 12px; }
.g1-pending textarea { width: 100%; }
.g1-state-uncertain { color: var(--red); }
dialog.g1-modal { width: min(50vw, 900px); }
dialog.g1-modal textarea { width: 100%; min-height: 40vh; resize: vertical; }
.events.g1-column > .ev { margin-left: var(--g1-col, 0px); max-width: calc(100% - var(--g1-col, 0px)); }
.sidebar.g1-pane-focus { box-shadow: inset 0 2px 0 var(--blue); }
main.detail.g1-pane-focus .session-head { box-shadow: inset 0 2px 0 var(--blue); }
`;

export function initInput(): void {
  const style = document.createElement('style');
  style.id = 'g1-input-css';
  style.textContent = INPUT_CSS;
  document.head.append(style);

  const saved = Number(getItem(SIDEBAR_KEY));
  if (saved > 0) applySidebar(Math.max(SIDEBAR_DEFAULT / 2, saved));
  applyColumn();
  composer().placeholder = composerHint();
  renderBadge();

  document.addEventListener('keydown', onKeyDown, true);
  document.addEventListener('paste', onPaste, true);
  document.addEventListener('submit', onSubmit, true);
  // The launch prompt: Ctrl+Enter submits (through the config gate).
  $('launch-form').addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey) && !ev.isComposing) {
      ev.preventDefault();
      $<HTMLFormElement>('launch-form').requestSubmit();
    }
  });

  // A vim field takes focus in insert mode (or the mode i/a/o/O asked for); leaving drops a half-typed command.
  document.addEventListener('focusin', (ev) => {
    const t = ev.target as Element | null;
    if (isVimKeysOn() && isVimField(t)) {
      const how = entry && entry.el === t ? entry.how : 'i';
      entry = null;
      enterInsert(t, how);
    }
    renderBadge();
    if (isVimField(t)) renderFieldTag(t, true);
    updateSuggestions(t);
  });
  document.addEventListener('focusout', (ev) => {
    const t = ev.target as Element | null;
    if (isVimField(t)) { resetField(t); renderFieldTag(t, false); }
    window.setTimeout(() => { renderBadge(); if (document.activeElement !== sugg.field) hideSuggestions(); }, 0);
  });
  document.addEventListener('input', (ev) => updateSuggestions(ev.target as Element | null));
  document.addEventListener('keyup', (ev) => {
    if (ev.key === 'ArrowLeft' || ev.key === 'ArrowRight' || ev.key === 'Home' || ev.key === 'End') updateSuggestions(ev.target as Element | null);
  });
  onVimMode((f) => { if (f === composer()) renderBadge(); else if (document.activeElement === f) renderFieldTag(f, true); if (f === sugg.field || suggestable(f)) updateSuggestions(f); });

  // Restore a dialog's saved geometry and note the key that opened it.
  new MutationObserver((records) => {
    for (const r of records) {
      const d = r.target;
      if (d instanceof HTMLDialogElement && d.open && r.oldValue === null) dialogOpened(d);
    }
  }).observe(document.body, { subtree: true, attributes: true, attributeFilter: ['open'], attributeOldValue: true });

  // The jumplist records every session the operator opens.
  onUiChange(() => {
    if (state.selectedId && state.selectedId !== lastSelected && !jumping) jumplist.visit(state.selectedId);
    if (state.selectedId !== lastSelected) { correction.preview = null; renderPreview(); }
    lastSelected = state.selectedId;
    markPane();
  });

  initPendingCount();
  pollConfig();
}

/** The input slice's DOM side, collected by actions/runtime.generated.ts. */
export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runInput(id as InputActionId, inv),
  snapshot: () => ({ input: inputSnapshot() }),
  init: initInput,
  initOrder: 200,
};
