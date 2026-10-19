// Session-detail actions, DOM side (slice 5): the transcript cursor, event and
// user-message motion, folds, visibility toggles, yank, and the rename, rating,
// model-switch, prompt-preview and continue dialogs. The rules live in
// detail-model.ts (pure, tested) and the registry entries in detail-spec.ts;
// commands.ts calls runDetail / detailSnapshot / noteAction from its single
// dispatch path. Daemon text is set with textContent only.
import { api, invoke } from '../api.js';
import { type Invocation, type SliceRuntime, copyText, dispatch } from '../commands.js';
import { renderEvents } from '../conversation.js';
import {
  type DiscoveredModels, type Focus, type PendingSwitch, type SwitchOptions, type SwitchState, type Visibility,
  MAX_TITLE_CHARS, SCROLL_LINES, buildSwitch, checkFollowUp, checkTitle, chosen, focusAfter, foldTargets,
  foldWanted, isVisible, keepCursor, parseRating, pendingLabel, previewMove, previewScroll, ratingFromKey,
  routeDetailKey, sessionRating, moveModel, overlayChord, stepEffort, stepEvent, stepRating, stepUserMessage, switchDecision,
  tupleLabel, visibleSeqs, yankTarget,
} from '../detail-model.js';
import { $, el, toast } from '../dom.js';
import { deliver } from '../operator-messages.js';
import { isTypingTarget } from '../keyengine.js';
import { sessionLabel } from '../model.js';
import { refreshSessions, renderSessionList, select, visibleRows } from '../sessions.js';
import { currentSessions, selected, state } from '../state.js';
import { type DetailActionId, type DetailSnapshot, SLICE } from './detail-spec.js';
import { focusOwnsKey } from '../modal-gate.js';

// ---------- state ----------

const d = {
  sessionId: null as string | null,
  /** Sequence of the selected transcript event. */
  cursor: null as number | null,
  focus: 'list' as Focus,
  hideSystem: false,
  hideThinking: false,
};
/** Queued model switches the daemon reported, by session. */
const pendings = new Map<string, PendingSwitch | null>();
/** Unsent continue-dialog text, by session. */
const drafts = new Map<string, string>();

/** A different session is open: drop the transcript cursor and hand the keys to the list. */
function sync(): void {
  if (d.sessionId !== state.selectedId) {
    d.sessionId = state.selectedId;
    d.cursor = null;
    d.focus = 'list';
  }
  d.cursor = keepCursor(state.events, d.cursor);
}

function vis(): Visibility {
  return { hideTools: state.hideTools, hideSystem: d.hideSystem, hideThinking: d.hideThinking };
}

export function detailSnapshot(): DetailSnapshot {
  sync();
  const v = vis();
  let userMessages = 0;
  let shown = 0;
  for (const e of state.events) {
    if (!isVisible(e, v)) continue;
    shown += 1;
    if (e.event_type === 'Message' && e.role === 'User') userMessages += 1;
  }
  return {
    events: state.events.length,
    shown,
    userMessages,
    cursor: d.cursor !== null,
    focus: d.focus,
    hideSystem: d.hideSystem,
    hideThinking: d.hideThinking,
  };
}

/** Where the keys are (gap sweep G1's pane focus, Ctrl-H/L). */
export function detailFocus(): Focus {
  sync();
  return d.focus;
}

/** Hand the keys to the transcript or back to the list (G1 Ctrl-H/L; G2 e, Backspace, Ctrl-w, Ctrl-Left / Right). */
export function setDetailFocus(f: Focus): void {
  sync();
  d.focus = f;
}

/** An action ran: the keys now belong to the list or to the transcript. */
export function noteAction(id: string): void {
  d.focus = focusAfter(id, d.focus);
}

/** `gg` / `G` while the transcript has the keys: select the first / last shown event (jump_to_top / bottom). */
export function jumpTranscript(end: 'first' | 'last'): void {
  sync();
  const seqs = visibleSeqs(state.events, vis());
  const seq = end === 'first' ? seqs[0] : seqs[seqs.length - 1];
  if (seq === undefined) return;
  setCursor(seq);
  if (end === 'first') $('events').scrollTop = 0;
  else $('events').scrollTop = $('events').scrollHeight;
}

// ---------- transcript cursor ----------

const nodeFor = (seq: number): HTMLElement | null =>
  $('events').querySelector<HTMLElement>(`.ev[data-seq="${seq}"]`);

/** The event's node, re-rendering the pane when it sits above "Show earlier". */
function reveal(seq: number): HTMLElement | null {
  const node = nodeFor(seq);
  if (node) return node;
  const idx = state.events.findIndex((e) => e.sequence === seq);
  if (idx < 0) return null;
  const need = state.events.length - idx;
  if (need > state.renderLimit) {
    state.renderLimit = need;
    state.renderedCount = 0;
    renderEvents();
  }
  return nodeFor(seq);
}

function applyCursor(): void {
  sync();
  const pane = $('events');
  for (const n of pane.querySelectorAll('.ev-cursor')) n.classList.remove('ev-cursor');
  if (d.cursor !== null) nodeFor(d.cursor)?.classList.add('ev-cursor');
}

function setCursor(seq: number | null): void {
  d.cursor = seq;
  if (seq !== null) reveal(seq);
  applyCursor();
  if (seq !== null) nodeFor(seq)?.scrollIntoView({ block: 'nearest' });
}

function applyVisibility(): void {
  const pane = $('events');
  pane.classList.toggle('hide-system', d.hideSystem);
  pane.classList.toggle('hide-thinking', d.hideThinking);
}

/** After a visibility change the cursor must not sit on a hidden event. */
function dropHiddenCursor(): void {
  const e = state.events.find((x) => x.sequence === d.cursor);
  if (e && !isVisible(e, vis())) setCursor(null);
}

function detailsOf(seqs: readonly number[]): HTMLDetailsElement[] {
  return seqs.flatMap((s) => [...(nodeFor(s)?.querySelectorAll('details') ?? [])]);
}

function fold(op: 'open' | 'close' | 'toggle'): void {
  const targets = foldTargets(state.events, d.cursor);
  for (const s of targets) reveal(s);
  const folds = detailsOf(targets);
  if (folds.length === 0) { toast('Nothing to fold on this event'); return; }
  const want = foldWanted(op, folds.map((f) => f.open));
  for (const f of folds) f.open = want;
}

function foldAll(open: boolean): void {
  for (const f of $('events').querySelectorAll('details')) f.open = open;
}

function scrollPane(dir: 1 | -1): void {
  const pane = $('events');
  const lh = Number.parseFloat(getComputedStyle(pane).lineHeight) || 18;
  pane.scrollBy({ top: dir * SCROLL_LINES * lh });
  keepCursorVisible(pane);
}

/** keybindings.md: Up/Down scroll "selection stays visible": a selected event scrolled out moves to the nearest shown one. */
function keepCursorVisible(pane: HTMLElement): void {
  if (d.cursor === null) return;
  const node = nodeFor(d.cursor);
  const box = pane.getBoundingClientRect();
  const inView = (n: Element) => { const r = n.getBoundingClientRect(); return r.height > 0 && r.bottom > box.top && r.top < box.bottom; };
  if (!node || inView(node)) return;
  const shown = [...pane.querySelectorAll<HTMLElement>('.ev[data-seq]')].filter(inView);
  const above = node.getBoundingClientRect().bottom <= box.top;
  const pick = above ? shown[0] : shown[shown.length - 1];
  const seq = pick ? Number(pick.dataset.seq) : NaN;
  if (Number.isFinite(seq)) { d.cursor = seq; applyCursor(); }
}

// ---------- the action switch ----------

export function runDetail(id: DetailActionId, inv: Invocation): void {
  sync();
  const v = vis();
  switch (id) {
    case 'event-next':
    case 'event-prev':
      setCursor(stepEvent(visibleSeqs(state.events, v), d.cursor, id === 'event-next' ? 1 : -1));
      return;
    case 'user-next':
    case 'user-prev':
      setCursor(stepUserMessage(state.events, v, d.cursor, id === 'user-next' ? 1 : -1));
      return;
    case 'scroll-up': scrollPane(-1); return;
    case 'scroll-down': scrollPane(1); return;
    case 'fold-open': fold('open'); return;
    case 'fold-close': fold('close'); return;
    case 'fold-toggle': fold('toggle'); return;
    case 'fold-close-all': foldAll(false); return;
    case 'fold-open-all': foldAll(true); return;
    case 'toggle-system':
      d.hideSystem = !d.hideSystem;
      applyVisibility();
      dropHiddenCursor();
      toast(d.hideSystem ? 'System events hidden' : 'System events shown', true);
      return;
    case 'toggle-thinking':
      d.hideThinking = !d.hideThinking;
      applyVisibility();
      dropHiddenCursor();
      toast(d.hideThinking ? 'Thinking events hidden' : 'Thinking events shown', true);
      return;
    case 'yank': yank(); return;
    case 'rename-session': openRename(); return;
    case 'rate-session': rateFromArg(inv.arg); return;
    case 'switch-model': void openSwitch(); return;
    case 'prompt-preview': openPreview(); return;
  }
}

function yank(): void {
  if (yankTarget(d.focus, d.cursor) === 'event') {
    const e = state.events.find((x) => x.sequence === d.cursor);
    if (e) { void copyText(e.content, 'event text'); return; }
  }
  const s = selected();
  if (s) void copyText(s.id, 'session ID');
}

// ---------- dialogs ----------

export function dialogEl(id: string): HTMLDialogElement {
  let dlg = document.getElementById(id) as HTMLDialogElement | null;
  if (!dlg) {
    dlg = document.createElement('dialog');
    dlg.id = id;
    dlg.className = 'detail-dialog';
    dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg!.close(); });
    document.body.append(dlg);
  }
  dlg.onkeydown = null;
  dlg.onclose = null;
  chordArmed = false;
  return dlg;
}

let chordArmed = false;

/** `Space N` / `Space ;` inside an overlay: close it, then open the new-session dialog / palette. */
export function overlayKey(ev: KeyboardEvent, dlg: HTMLDialogElement): boolean {
  if (ev.ctrlKey || ev.metaKey || ev.altKey) { chordArmed = false; return false; }
  const r = overlayChord(chordArmed, ev.key);
  chordArmed = r.armed;
  if (r.run) {
    ev.preventDefault();
    dlg.close();
    dispatch(r.run, { source: 'key' });
    return true;
  }
  if (ev.key === ' ') { ev.preventDefault(); return true; }
  return false;
}

export function button(text: string, cls = 'btn', type: 'button' | 'submit' = 'button'): HTMLButtonElement {
  const b = el('button', cls, text);
  b.type = type;
  return b;
}

export function errorLine(): HTMLElement {
  const e = el('div', 'form-error');
  e.hidden = true;
  e.setAttribute('role', 'alert');
  return e;
}

export function showError(line: HTMLElement, msg: string): void {
  line.textContent = msg;
  line.hidden = false;
}

export function focusDialog(dlg: HTMLDialogElement): void {
  dlg.tabIndex = -1;
  dlg.focus();
}

// ----- rename -----

function openRename(): void {
  const s = selected();
  if (!s) return;
  const dlg = dialogEl('rename-dialog');
  const form = el('form', 'launch-form');
  const input = el('input');
  input.type = 'text';
  input.value = s.title ?? '';
  input.maxLength = MAX_TITLE_CHARS;
  input.setAttribute('aria-label', 'Session title');
  const label = el('label', undefined, 'Title');
  label.append(input);
  const err = errorLine();
  const cancel = button('Cancel');
  cancel.addEventListener('click', () => dlg.close());
  const actions = el('div', 'actions');
  actions.append(cancel, button('Rename', 'btn primary', 'submit'));
  form.append(el('h2', undefined, 'Rename session'), label, err, actions);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const r = checkTitle(input.value);
    if (!r.ok) { showError(err, r.why); return; }
    invoke('rename_session', { sessionId: s.id, title: r.value }).then(() => {
      dlg.close();
      toast('Title updated', true);
      void refreshSessions();
    }, (e: unknown) => showError(err, `Rename failed: ${String(e)}`));
  });
  dlg.replaceChildren(form);
  dlg.showModal();
  input.select();
}

// ----- rating -----

function rateFromArg(arg: string | undefined): void {
  const r = parseRating(arg);
  if (!r.ok) { toast(r.why); return; }
  const s = selected();
  if (!s) return;
  if (r.value === null) openRate(); else void saveRating(s.id, r.value);
}

async function saveRating(id: string, n: number): Promise<boolean> {
  try {
    await invoke('rate_session', { sessionId: id, rating: n });
    toast(`Rated ${n}/10`, true);
    void refreshSessions();
    return true;
  } catch (e) {
    toast(`Rating failed: ${String(e)}`);
    return false;
  }
}

function openRate(): void {
  const s = selected();
  if (!s) return;
  const dlg = dialogEl('rate-dialog');
  let pick = sessionRating(s.rating);
  const box = el('div', 'launch-form');
  const row = el('div', 'rate-row');
  row.setAttribute('role', 'radiogroup');
  row.setAttribute('aria-label', 'Rating');
  const save = button('Save', 'btn primary');
  const draw = (): void => {
    row.replaceChildren(...Array.from({ length: 10 }, (_, i) => {
      const n = i + 1;
      const b = button(String(n), `btn rate-btn${pick === n ? ' sel' : ''}`);
      b.setAttribute('role', 'radio');
      b.setAttribute('aria-checked', String(pick === n));
      b.addEventListener('click', () => { pick = n; draw(); });
      return b;
    }));
    save.disabled = pick === null;
  };
  const submit = (): void => {
    if (pick === null) return;
    void saveRating(s.id, pick).then((ok) => { if (ok) dlg.close(); });
  };
  const cancel = button('Cancel');
  cancel.addEventListener('click', () => dlg.close());
  save.addEventListener('click', submit);
  const actions = el('div', 'actions');
  actions.append(cancel, save);
  box.append(
    el('h2', undefined, 'Rate session'),
    el('div', 'dialog-note', `${sessionLabel(s)}: 1-9 pick that rating, 0 picks 10, h/l step it, Enter saves.`),
    row, actions,
  );
  draw();
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (overlayKey(ev, dlg) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const n = ratingFromKey(ev.key);
    if (n !== null) { ev.preventDefault(); pick = n; draw(); return; }
    if (ev.key === 'h' || ev.key === 'ArrowLeft') { ev.preventDefault(); pick = stepRating(pick, -1); draw(); }
    else if (ev.key === 'l' || ev.key === 'ArrowRight') { ev.preventDefault(); pick = stepRating(pick, 1); draw(); }
    else if (ev.key === 'Enter') { ev.preventDefault(); submit(); }
    else if (ev.key === 'q') { ev.preventDefault(); dlg.close(); }
  };
  dlg.replaceChildren(box);
  dlg.showModal();
  focusDialog(dlg);
}

// ----- model / effort switch -----

function normalizeDiscovered(raw: unknown): DiscoveredModels | null {
  if (Array.isArray(raw)) return { models: raw as Array<[string, string]> };
  if (raw && typeof raw === 'object' && Array.isArray((raw as DiscoveredModels).models)) return raw as DiscoveredModels;
  return null;
}

async function openSwitch(): Promise<void> {
  const s = selected();
  if (!s) return;
  let options: SwitchOptions;
  try {
    options = await invoke<SwitchOptions>('get_session_model_switch_options', { sessionId: s.id });
  } catch (e) {
    toast(`Model switch unavailable: ${String(e)}`);
    return;
  }
  pendings.set(s.id, options.pending ?? null);
  decorateHeader();
  if (!options.switchable) {
    toast(options.unavailable_reason ?? 'Model switch unavailable for this session');
    return;
  }
  let discovered: DiscoveredModels | null = null;
  try {
    discovered = normalizeDiscovered(await invoke<unknown>('discover_models', { provider: options.provider }));
  } catch {
    discovered = null; // the current model is still offered
  }
  let st = buildSwitch(options, discovered);
  if (!st) { toast('No models are available to switch to for this session'); return; }
  const dlg = dialogEl('switch-dialog');
  const box = el('div', 'launch-form');
  const list = el('ul', 'pick-list');
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', 'Models');
  const effort = el('div', 'effort-row');
  const err = errorLine();
  const draw = (): void => {
    const cur = st!;
    list.replaceChildren(...cur.models.map((m, i) => {
      const li = el('li', `pick-item${i === cur.modelIndex ? ' sel' : ''}`);
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(i === cur.modelIndex));
      li.append(el('span', undefined, m.label), el('span', 'pick-id', m.id === m.label ? '' : m.id));
      li.addEventListener('click', () => { st = moveModel(cur, i); draw(); });
      return li;
    }));
    list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    const less = button('◀', 'btn');
    const more = button('▶', 'btn');
    less.setAttribute('aria-label', 'Lower effort');
    more.setAttribute('aria-label', 'Raise effort');
    less.addEventListener('click', () => { st = stepEffort(cur, -1); draw(); });
    more.addEventListener('click', () => { st = stepEffort(cur, 1); draw(); });
    effort.replaceChildren(el('span', undefined, 'Effort'), less, el('strong', undefined, cur.efforts[cur.effortIndex] ?? 'default'), more);
  };
  const submit = (): void => {
    const cur = st!;
    const decision = switchDecision(cur);
    const pick = chosen(cur);
    const label = tupleLabel(pick.model, pick.effort);
    if (decision === 'no-fence') { showError(err, 'Model switch unavailable: no model invocation to fence it'); return; }
    dlg.close();
    if (decision === 'unchanged') { toast(`Already running ${label}`, true); return; }
    invoke<{ state?: string }>('queue_session_model_update', {
      sessionId: s.id,
      expectedModelInvocationId: cur.options.model_invocation_id,
      newModel: pick.model,
      newEffort: pick.effort,
    }).then((receipt) => {
      if (receipt?.state === 'applied') {
        pendings.set(s.id, null);
        toast(`Already running ${label}`, true);
      } else {
        pendings.set(s.id, { model: pick.model, effort: pick.effort });
        toast(`Switching to ${label} from the next turn`, true);
      }
      decorateHeader();
    }, (e: unknown) => toast(`Model switch failed: ${String(e)}`));
  };
  const cancel = button('Cancel');
  cancel.addEventListener('click', () => dlg.close());
  const go = button('Queue switch', 'btn primary');
  go.addEventListener('click', submit);
  const actions = el('div', 'actions');
  actions.append(cancel, go);
  const now = tupleLabel(options.model ?? 'default model', options.effort ?? null);
  const queued = pendingLabel(options.pending);
  box.append(
    el('h2', undefined, 'Switch model / effort'),
    el('div', 'dialog-note', `Now ${now}${queued ? `, queued ${queued}` : ''}. ${options.context_note}`),
    list, effort, err,
    el('div', 'dialog-note', 'j/k pick a model, h/l the effort, Enter queues it for the next turn. A running turn is never interrupted.'),
    actions,
  );
  draw();
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (overlayKey(ev, dlg) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const cur = st!;
    const go1 = (to: number): void => { ev.preventDefault(); st = moveModel(cur, to); draw(); };
    if (ev.key === 'j' || ev.key === 'ArrowDown') go1(cur.modelIndex + 1);
    else if (ev.key === 'k' || ev.key === 'ArrowUp') go1(cur.modelIndex - 1);
    else if (ev.key === 'g') go1(0);
    else if (ev.key === 'G') go1(cur.models.length - 1);
    else if (ev.key === 'h' || ev.key === 'ArrowLeft') { ev.preventDefault(); st = stepEffort(cur, -1); draw(); }
    else if (ev.key === 'l' || ev.key === 'ArrowRight') { ev.preventDefault(); st = stepEffort(cur, 1); draw(); }
    else if (ev.key === 'Enter') { ev.preventDefault(); submit(); }
    else if (ev.key === 'q') { ev.preventDefault(); dlg.close(); }
  };
  dlg.replaceChildren(box);
  dlg.showModal();
  focusDialog(dlg);
}

// ----- prompt preview -----

function openPreview(): void {
  let cur = state.cursorId ?? state.selectedId;
  if (!cur) { toast('No session to preview (j/k to pick one)'); return; }
  const dlg = dialogEl('preview-dialog');
  const box = el('div', 'launch-form');
  const title = el('h2');
  const pos = el('div', 'dialog-note');
  const body = el('pre', 'preview-body');
  box.append(title, pos, body, el('div', 'dialog-note', 'j/k next or previous session, G last, Ctrl-D/U scroll, Enter opens it, p or Esc closes.'));
  const ids = (): string[] => visibleRows().map((r) => r.session.id);
  const draw = (): void => {
    const s = currentSessions().find((x) => x.id === cur);
    title.textContent = s ? sessionLabel(s) : 'Session not found';
    const all = ids();
    const at = cur ? all.indexOf(cur) : -1;
    pos.textContent = at >= 0 ? `${at + 1} of ${all.length}` : '';
    body.textContent = s?.query ?? '';
    body.scrollTop = 0;
  };
  const move = (key: 'next' | 'prev' | 'last'): void => {
    const to = previewMove(ids(), cur, key);
    if (!to) return;
    cur = to;
    state.cursorId = to;
    renderSessionList();
    draw();
  };
  const scroll = (dir: 1 | -1): void => {
    const lh = Number.parseFloat(getComputedStyle(body).lineHeight) || 18;
    body.scrollTop = previewScroll(body.scrollTop / lh, dir) * lh;
  };
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (overlayKey(ev, dlg) || ev.altKey || ev.metaKey) return;
    if (ev.ctrlKey) {
      if (ev.key === 'd') { ev.preventDefault(); scroll(1); }
      else if (ev.key === 'u') { ev.preventDefault(); scroll(-1); }
      return;
    }
    if (ev.key === 'j' || ev.key === 'ArrowDown') { ev.preventDefault(); move('next'); }
    else if (ev.key === 'k' || ev.key === 'ArrowUp') { ev.preventDefault(); move('prev'); }
    else if (ev.key === 'G') { ev.preventDefault(); move('last'); }
    else if (ev.key === 'Enter') { ev.preventDefault(); dlg.close(); if (cur) select(cur); }
    else if (ev.key === 'p' || ev.key === 'q') { ev.preventDefault(); dlg.close(); }
  };
  dlg.replaceChildren(box);
  draw();
  dlg.showModal();
  focusDialog(dlg);
}

// ----- continue prompt -----

export function openContinue(): void {
  const s = selected();
  if (!s) return;
  const dlg = dialogEl('continue-dialog');
  const form = el('form', 'launch-form');
  const area = el('textarea');
  area.rows = 8;
  area.value = drafts.get(s.id) ?? '';
  area.setAttribute('aria-label', 'Follow-up message');
  const label = el('label', undefined, 'Follow-up');
  label.append(area);
  const err = errorLine();
  let sent = false;
  const cancel = button('Close');
  cancel.addEventListener('click', () => dlg.close());
  const actions = el('div', 'actions');
  actions.append(cancel, button('Send', 'btn primary', 'submit'));
  form.append(
    el('h2', undefined, `Continue: ${sessionLabel(s)}`),
    label,
    el('div', 'dialog-note', 'Ctrl+Enter (Cmd+Enter on macOS) sends. Esc closes without sending; the draft is kept.'),
    err, actions,
  );
  const send = (): void => {
    const r = checkFollowUp(area.value);
    if (!r.ok) { showError(err, r.why); return; }
    deliver(s, r.value).then(() => {
      sent = true;
      drafts.delete(s.id);
      dlg.close();
      toast('Sent', true);
      void refreshSessions();
    }, (e: unknown) => showError(err, `Send failed: ${String(e)}`));
  };
  form.addEventListener('submit', (ev) => { ev.preventDefault(); send(); });
  area.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey) && !ev.isComposing) { ev.preventDefault(); send(); }
  });
  dlg.onclose = () => { if (!sent) drafts.set(s.id, area.value); };
  dlg.replaceChildren(form);
  dlg.showModal();
  area.focus();
}

// ---------- header and wiring ----------

/** Rating and a queued model switch, appended to the open session's meta line. */
function decorateHeader(): void {
  const meta = document.getElementById('s-meta');
  if (!meta) return;
  const s = selected();
  const parts: Array<[string, string]> = [];
  if (s) {
    const r = sessionRating(s.rating);
    if (r !== null) parts.push(['rating', `${r}/10`]);
    const p = pendings.get(s.id);
    const label = p && s.model !== p.model ? pendingLabel(p) : null;
    if (label) parts.push(['queued', label]);
  }
  const sig = JSON.stringify(parts);
  const have = meta.querySelector<HTMLElement>('.meta-detail');
  if ((have?.dataset.sig ?? '[]') === sig) return;
  have?.remove();
  if (parts.length === 0) return;
  const span = el('span', 'meta-detail');
  span.dataset.sig = sig;
  parts.forEach(([k, v], i) => {
    if (i > 0) span.append(' ');
    span.append(`${k}: `, el('span', undefined, v));
  });
  meta.append(span);
}

export function initDetail(): void {
  const css = document.createElement('link');
  css.rel = 'stylesheet';
  css.href = 'detail.css';
  document.head.append(css);

  new MutationObserver(applyCursor).observe($('events'), { childList: true });
  new MutationObserver(decorateHeader).observe($('s-meta'), { childList: true });
  applyVisibility();

  // The keys go to the transcript when an event is clicked, back to the list when a row is.
  $('events').addEventListener('click', (ev) => {
    const box = (ev.target as HTMLElement).closest<HTMLElement>('.ev');
    const seq = box ? Number(box.dataset.seq) : NaN;
    if (!Number.isFinite(seq)) return;
    d.focus = 'transcript';
    setCursor(seq);
  });
  $('session-list').addEventListener('click', () => { d.focus = 'list'; });

  document.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    const target = document.activeElement as HTMLElement | null;
    const composer = $<HTMLTextAreaElement>('message');
    const route = routeDetailKey(ev, {
      composerFocused: target === composer,
      draft: composer.value,
      modalOpen: document.querySelector('dialog[open]') !== null,
      typingElsewhere: target !== composer && isTypingTarget(target),
      sessionOpen: !!selected(),
    });
    if (route.kind === 'pass') return;
    ev.preventDefault();
    if (route.kind === 'blur-composer') { composer.blur(); return; }
    dispatch(route.action, { source: 'key' });
  });
}

/** The detail slice's DOM side, collected by actions/runtime.generated.ts. */
export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runDetail(id as DetailActionId, inv),
  snapshot: () => ({ detail: detailSnapshot() }),
  afterAction: noteAction,
  init: initDetail,
  initOrder: 30,
};
