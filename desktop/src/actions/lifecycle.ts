// Session-lifecycle actions, DOM side (slice 6): delete to trash, unarchive,
// downgrade a pause, the trash browser, the notification browser, recent
// completions and the question modal. The rules live in lifecycle-model.ts
// (pure, tested) and the registry entries in lifecycle-spec.ts; commands.ts
// calls runLifecycle / lifecycleSnapshot / openQuestionModal from its single
// dispatch path. Daemon text is set with textContent only.
import { type Session, api, invoke } from '../api.js';
import { DECLINE_RESPONSE, pendingQuestions } from '../approvals-model.js';
import { confirmDialog, forgetPause } from '../approvals.js';
import type { Invocation, SliceRuntime } from '../commands.js';
import { $, el, onToast, toast } from '../dom.js';
import {
  type NoticeStore, type QuestionState, type SeenMap, type TrashState,
  EMPTY_NOTICES, activeNotices, deleteBlock, dismissAll, dismissAt, lifecycleTarget, listKey, noticeRows, openQuestion,
  openTrash, previewQuery, pushNotice, questionHint, questionKey, questionResponse, questionTarget, recentCompletions,
  sessionNotices, setQuestionText, toastKind, trashMove, trashRemove, trashRows, trashSelected, purgeStep, pickOption,
} from '../lifecycle-model.js';
import { relativeTime, sessionLabel } from '../model.js';
import { refreshArchived, refreshSessions, renderHeader, renderSessionList, select, setView } from '../sessions.js';
import { currentSessions, onUiChange, selected, state } from '../state.js';
import { button, dialogEl, errorLine, focusDialog, overlayKey, showError } from './detail.js';
import { type LifecycleActionId, type LifecycleSnapshot, SLICE } from './lifecycle-spec.js';
import { focusOwnsKey } from '../modal-gate.js';

// ---------- state ----------

let notices: NoticeStore = EMPTY_NOTICES;
let seen: SeenMap = new Map();
let lastSessions: Session[] | undefined;

/** The session a lifecycle action acts on: the list cursor's, else the open one (the TUI acts on the selected row). */
function targetSession(): Session | undefined {
  const list = currentSessions();
  const cur = state.cursorId ? list.find((s) => s.id === state.cursorId) : undefined;
  return lifecycleTarget(cur, selected());
}

export function lifecycleSnapshot(): LifecycleSnapshot {
  const t = targetSession();
  return {
    target: t ? { status: t.status } : null,
    questions: state.sessions.filter((s) => pendingQuestions(s).length > 0).length,
    notices: activeNotices(notices, Date.now()),
    completions: recentCompletions(state.sessions).length,
  };
}

/** Feed the notification browser from the session list: finished and waiting sessions. */
function trackSessions(): void {
  if (state.sessions === lastSessions) return;
  lastSessions = state.sessions;
  const r = sessionNotices(seen, state.sessions);
  seen = r.seen;
  const now = Date.now();
  for (const n of r.notices) notices = pushNotice(notices, n, now);
}

// ---------- session helpers ----------

/** Close the open session when it left the list (deleted, unarchived). */
function closeIfOpen(id: string): void {
  if (state.cursorId === id) state.cursorId = null;
  if (state.selectedId !== id) return;
  state.selectedId = null;
  state.selectionEpoch += 1;
  state.events = [];
  $('events').replaceChildren();
}

async function reload(): Promise<void> {
  if (state.view === 'archived') await refreshArchived();
  await refreshSessions();
  renderSessionList();
  renderHeader();
}

/** Open a session by id, switching to the Archived view when it is archived (a notification can link either). */
async function openSession(id: string): Promise<void> {
  if (state.sessions.some((s) => s.id === id) && state.view === 'active') { select(id); return; }
  if (state.archived.some((s) => s.id === id) && state.view === 'archived') { select(id); return; }
  try {
    const s = await api.getSession(id);
    const want = s.status === 'Archived' ? 'archived' : 'active';
    if (state.view !== want) await setView(want);
    await refreshSessions();
    select(id);
  } catch (e) {
    toast(`Session unavailable: ${String(e)}`);
  }
}

// ---------- delete, unarchive, downgrade ----------

async function deleteSession(inv: Invocation): Promise<void> {
  const s = targetSession();
  if (!s) return;
  const block = deleteBlock(s.status);
  if (block) { toast(block); return; }
  // The TUI's confirmation is the double-tap DD (or typing the whole command).
  // Menus, the palette and buttons have no double-tap, so they ask.
  if (inv.source !== 'key') {
    const ok = await confirmDialog(`Move "${previewQuery(s)}" to the trash? Space g X opens the trash to restore it.`, 'Delete');
    if (!ok) return;
  }
  try {
    await invoke('delete_session', { sessionId: s.id });
    toast(`Deleted: ${previewQuery(s)}`, true);
    closeIfOpen(s.id);
    await reload();
  } catch (e) {
    toast(`Delete failed: ${String(e)}`);
  }
}

async function unarchiveSession(): Promise<void> {
  const s = targetSession();
  if (!s) return;
  try {
    await invoke('unarchive_session', { sessionId: s.id });
    toast(`Unarchived: ${previewQuery(s, 30)}`, true);
    closeIfOpen(s.id);
    await reload();
  } catch (e) {
    toast(`Unarchive failed: ${String(e)}`);
  }
}

async function downgradePause(): Promise<void> {
  const s = selected();
  if (!s) return;
  try {
    await api.setOperatorPause(s.id, 'soft');
    toast('Operator pause downgraded to SOFT', true);
    forgetPause();
    await refreshSessions();
    renderHeader();
  } catch (e) {
    toast(`Pause change failed: ${String(e)}`);
  }
}

// ---------- shared dialog bits ----------

function hint(text: string): HTMLElement {
  return el('div', 'dialog-note', text);
}

function row(cls: string, selectedRow: boolean): HTMLLIElement {
  const li = el('li', `pick-item ${cls}${selectedRow ? ' sel' : ''}`);
  li.setAttribute('role', 'option');
  li.setAttribute('aria-selected', String(selectedRow));
  return li;
}

function list(label: string): HTMLUListElement {
  const ul = el('ul', 'pick-list');
  ul.setAttribute('role', 'listbox');
  ul.setAttribute('aria-label', label);
  return ul;
}

// ---------- trash browser ----------

async function openTrashBrowser(): Promise<void> {
  let sessions: Session[];
  try {
    sessions = await invoke<Session[]>('list_deleted_sessions', {
      projectId: $<HTMLSelectElement>('project-filter').value || null,
    });
  } catch (e) {
    toast(`Failed to load trash: ${String(e)}`);
    return;
  }
  if (sessions.length === 0) { toast('Trash is empty'); return; }
  let st: TrashState = openTrash(sessions);
  const dlg = dialogEl('trash-dialog');
  const box = el('div', 'launch-form');
  const ul = list('Deleted sessions');
  const note = hint('');
  const err = errorLine();
  const restoreBtn = button('Restore (U)');
  const purgeBtn = button('Purge (D)', 'btn danger');
  const closeBtn = button('Close');
  const actions = el('div', 'actions');
  actions.append(closeBtn, restoreBtn, purgeBtn);
  box.append(el('h2', undefined, 'Trash'), ul, note, err, actions,
    hint('j/k move, G last, U restores, D then D again purges permanently, Esc closes.'));

  const draw = (): void => {
    err.hidden = true;
    const now = Date.now();
    const sel = trashSelected(st);
    ul.replaceChildren(...trashRows(st.sessions, now).map((r) => {
      if (r.kind === 'header') {
        const h = el('li', 'pick-header', `── ${r.title} ──`);
        h.setAttribute('role', 'presentation');
        return h;
      }
      const li = row('trash-row', r.session.id === sel?.id);
      li.append(el('span', undefined, previewQuery(r.session, 70)), el('span', 'pick-id', relativeTime(r.session.updated_at, now)));
      li.addEventListener('click', () => {
        const at = st.sessions.findIndex((s) => s.id === r.session.id);
        if (at >= 0) { st = { ...st, selected: at, purge: null }; draw(); }
      });
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    const armed = st.purge && st.purge.id === sel?.id;
    note.textContent = armed ? `Press D again within 5 s to purge "${previewQuery(sel!, 40)}" permanently.` : '';
    purgeBtn.textContent = armed ? 'Purge permanently?' : 'Purge (D)';
  };

  const restore = (): void => {
    const sel = trashSelected(st);
    if (!sel) return;
    invoke('undelete_session', { sessionId: sel.id }).then(() => {
      st = trashRemove(st, sel.id);
      void refreshSessions();
      if (st.sessions.length === 0) { dlg.close(); toast('Restored (trash is now empty)', true); return; }
      toast(`Restored: ${previewQuery(sel, 30)}`, true);
      draw();
    }, (e: unknown) => showError(err, `Restore failed: ${String(e)}`));
  };
  const purge = (): void => {
    const step = purgeStep(st, Date.now());
    st = step.st;
    if (!step.run) { draw(); return; }
    const sel = trashSelected(st);
    const id = step.run;
    invoke('purge_session', { sessionId: id }).then(() => {
      st = trashRemove(st, id);
      if (st.sessions.length === 0) { dlg.close(); toast('Purged (trash is now empty)', true); return; }
      toast(`Purged: ${sel ? previewQuery(sel, 30) : id}`, true);
      draw();
    }, (e: unknown) => showError(err, `Purge failed: ${String(e)}`));
  };

  closeBtn.addEventListener('click', () => dlg.close());
  restoreBtn.addEventListener('click', restore);
  purgeBtn.addEventListener('click', purge);
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (overlayKey(ev, dlg) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const stop = (): void => ev.preventDefault();
    if (ev.key === 'j' || ev.key === 'ArrowDown') { stop(); st = trashMove(st, 'next'); draw(); }
    else if (ev.key === 'k' || ev.key === 'ArrowUp') { stop(); st = trashMove(st, 'prev'); draw(); }
    else if (ev.key === 'G') { stop(); st = trashMove(st, 'last'); draw(); }
    else if (ev.key === 'U') { stop(); restore(); }
    else if (ev.key === 'D') { stop(); purge(); }
    else if (ev.key === 'Escape' || ev.key === 'q') { stop(); dlg.close(); }
  };
  dlg.replaceChildren(box);
  draw();
  dlg.showModal();
  focusDialog(dlg);
}

// ---------- notification browser ----------

/** A logged toast, kept raw; the key-hint pass (data-key-tip) shows it as the Vim keys switch says. */
function noticeMsg(message: string): HTMLElement {
  const span = el('span', 'notice-msg', message);
  span.dataset.keyTip = '';
  return span;
}

function openNotifications(): void {
  let at = 0;
  const dlg = dialogEl('notice-dialog');
  const box = el('div', 'launch-form');
  const ul = list('Notifications');
  const empty = hint('No notifications yet.');
  const closeBtn = button('Close');
  const dismissAllBtn = button('Dismiss all (N)');
  const actions = el('div', 'actions');
  actions.append(closeBtn, dismissAllBtn);
  box.append(el('h2', undefined, 'Notifications'), ul, empty, actions,
    hint('j/k move, g/G first or last, x dismisses, N dismisses all, Enter opens the linked session, Esc closes.'));

  const draw = (): void => {
    const now = Date.now();
    const rows = noticeRows(notices, now);
    at = Math.max(0, Math.min(at, rows.length - 1));
    empty.hidden = rows.length > 0;
    ul.hidden = rows.length === 0;
    dismissAllBtn.disabled = activeNotices(notices, now) === 0;
    ul.replaceChildren(...rows.map((n, i) => {
      const active = !n.dismissed && now - n.at < n.ttl;
      const li = row(`notice notice-${n.kind}${active ? ' notice-active' : ''}`, i === at);
      li.append(noticeMsg(n.message), el('span', 'pick-id', `${active ? 'new · ' : ''}${relativeTime(new Date(n.at).toISOString(), now)}`));
      li.addEventListener('click', () => { at = i; draw(); });
      li.addEventListener('dblclick', () => openLinked());
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  };
  const openLinked = (): void => {
    const id = noticeRows(notices, Date.now())[at]?.sessionId;
    if (!id) return;
    dlg.close();
    void openSession(id);
  };
  closeBtn.addEventListener('click', () => dlg.close());
  dismissAllBtn.addEventListener('click', () => { notices = dismissAll(notices, Date.now()); at = 0; draw(); });
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (overlayKey(ev, dlg) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const total = noticeRows(notices, Date.now()).length;
    const moved = listKey(at, total, ev.key);
    if (moved !== null) { ev.preventDefault(); at = moved; draw(); return; }
    if (ev.key === 'x') { ev.preventDefault(); notices = dismissAt(notices, at, Date.now()); draw(); }
    else if (ev.key === 'N') { ev.preventDefault(); notices = dismissAll(notices, Date.now()); at = 0; draw(); }
    else if (ev.key === 'Enter') { ev.preventDefault(); if (total === 0) dlg.close(); else openLinked(); }
    else if (ev.key === 'Escape' || ev.key === 'q') { ev.preventDefault(); dlg.close(); }
  };
  dlg.replaceChildren(box);
  draw();
  dlg.showModal();
  focusDialog(dlg);
}

// ---------- recent completions ----------

function openRecent(): void {
  let at = 0;
  const dlg = dialogEl('recent-dialog');
  const box = el('div', 'launch-form');
  const ul = list('Recent completions');
  const closeBtn = button('Close');
  const actions = el('div', 'actions');
  actions.append(closeBtn);
  box.append(el('h2', undefined, 'Recent completions'), ul, actions,
    hint('j/k move, g/G first or last, Enter opens the session, i goes to the message box, Esc closes.'));
  const draw = (): void => {
    const now = Date.now();
    const rows = recentCompletions(state.sessions);
    at = Math.max(0, Math.min(at, rows.length - 1));
    ul.replaceChildren(...rows.map((s, i) => {
      const li = row(`recent recent-${s.status.toLowerCase()}`, i === at);
      li.append(el('span', undefined, `${s.status}: ${sessionLabel(s)}`), el('span', 'pick-id', relativeTime(s.updated_at, now)));
      li.addEventListener('click', () => { at = i; draw(); });
      li.addEventListener('dblclick', () => openAt());
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  };
  const openAt = (): void => {
    const s = recentCompletions(state.sessions)[at];
    if (!s) return;
    dlg.close();
    void openSession(s.id);
  };
  closeBtn.addEventListener('click', () => dlg.close());
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (overlayKey(ev, dlg) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const moved = listKey(at, recentCompletions(state.sessions).length, ev.key);
    if (moved !== null) { ev.preventDefault(); at = moved; draw(); return; }
    if (ev.key === 'Enter') { ev.preventDefault(); openAt(); }
    else if (ev.key === 'i') { ev.preventDefault(); dlg.close(); if (selected()) $('message').focus(); }
    else if (ev.key === 'Escape' || ev.key === 'q') { ev.preventDefault(); dlg.close(); }
  };
  dlg.replaceChildren(box);
  draw();
  dlg.showModal();
  focusDialog(dlg);
}

// ---------- question modal ----------

export function openQuestionModal(): void {
  const target = questionTarget(state.sessions, state.selectedId);
  if (!target) { toast('No sessions waiting for input'); return; }
  const questions = pendingQuestions(target);
  if (questions.length === 0) { toast('No pending question for this session'); return; }
  let st: QuestionState = openQuestion(questions);
  const dlg = dialogEl('question-dialog');
  const box = el('div', 'launch-form');
  const title = el('h2');
  const head = el('div', 'approval-header');
  const text = el('div', 'approval-q');
  const ul = list('Options');
  const area = el('textarea');
  area.rows = 3;
  area.setAttribute('aria-label', 'Your own answer');
  const areaLabel = el('label', undefined, 'Or write your own answer');
  areaLabel.append(area);
  const err = errorLine();
  const note = hint('');
  const declineBtn = button('Decline (d)', 'btn danger');
  const hideBtn = button('Hide (Esc)');
  const prevBtn = button('◀ Previous');
  const nextBtn = button('Next ▶');
  const submitBtn = button('Submit (Ctrl+Enter)', 'btn primary');
  const actions = el('div', 'actions');
  actions.append(declineBtn, hideBtn, prevBtn, nextBtn, submitBtn);
  box.append(title, head, text, ul, areaLabel, err, note, actions);

  const draw = (): void => {
    err.hidden = true;
    const q = st.questions[st.current]!;
    title.textContent = `${sessionLabel(target)} is waiting for you`;
    head.textContent = `${q.header ? `${q.header} · ` : ''}Question ${st.current + 1} of ${st.questions.length}${q.multiSelect ? ' · choose any' : ''}`;
    text.textContent = q.question;
    const chosen = st.chosen[st.current] ?? [];
    const options = q.options ?? [];
    ul.hidden = options.length === 0;
    ul.replaceChildren(...options.map((o, i) => {
      const li = row('question-opt', i === st.cursor[st.current] && st.mode === 'normal');
      const mark = q.multiSelect ? (chosen.includes(i) ? '☑' : '☐') : (chosen.includes(i) ? '●' : '○');
      li.append(el('span', 'opt-mark', mark), el('span', undefined, `${i + 1}. ${o.label}`));
      if (o.description) li.append(el('span', 'pick-id', o.description));
      li.addEventListener('click', () => { st = pickOption(st, i); draw(); });
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    const want = st.text[st.current] ?? '';
    if (area.value !== want) area.value = want;
    prevBtn.disabled = st.current === 0;
    nextBtn.disabled = st.current + 1 >= st.questions.length;
    note.textContent = questionHint(st);
    if (st.mode === 'insert') { if (document.activeElement !== area) area.focus(); }
    else if (document.activeElement === area) { area.blur(); focusDialog(dlg); }
  };

  const finish = async (response: string, done: string): Promise<void> => {
    try {
      await api.answerQuestion(target.id, response);
      dlg.close();
      toast(done, true);
      void refreshSessions();
    } catch (e) {
      showError(err, `Failed to answer: ${String(e)}`);
    }
  };
  const submit = (): void => {
    const r = questionResponse(st);
    if (!r.ok) { showError(err, r.error); return; }
    void finish(r.text, 'Answer sent');
  };
  const decline = (): void => void finish(DECLINE_RESPONSE, 'Declined');
  const step = (delta: number): void => {
    st = { ...st, current: st.current + delta, mode: (st.questions[st.current + delta]?.options?.length ?? 0) > 0 ? 'normal' : 'insert' };
    draw();
  };

  declineBtn.addEventListener('click', decline);
  hideBtn.addEventListener('click', () => dlg.close());
  prevBtn.addEventListener('click', () => step(-1));
  nextBtn.addEventListener('click', () => step(1));
  submitBtn.addEventListener('click', submit);
  area.addEventListener('input', () => { st = setQuestionText(st, area.value); draw(); });
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    const r = questionKey(st, { key: ev.key, ctrl: ev.ctrlKey, meta: ev.metaKey, shift: ev.shiftKey, alt: ev.altKey });
    if (!r.consumed) return;
    ev.preventDefault();
    st = r.st;
    if (r.effect === 'submit') submit();
    else if (r.effect === 'decline') decline();
    else if (r.effect === 'hide') dlg.close();
    else draw();
  };
  dlg.replaceChildren(box);
  draw();
  dlg.showModal();
  if (st.mode === 'insert') area.focus(); else focusDialog(dlg);
}

// ---------- the action switch ----------

export function runLifecycle(id: LifecycleActionId, inv: Invocation): void {
  switch (id) {
    case 'delete-session': void deleteSession(inv); return;
    case 'unarchive-session': void unarchiveSession(); return;
    case 'downgrade-pause': void downgradePause(); return;
    case 'open-trash': void openTrashBrowser(); return;
    case 'open-notifications': openNotifications(); return;
    case 'open-recent': openRecent(); return;
  }
}

export function initLifecycle(): void {
  const css = document.createElement('link');
  css.rel = 'stylesheet';
  css.href = 'lifecycle.css';
  document.head.append(css);
  // The notification browser keeps what the desktop said and what the sessions did.
  onToast((msg, ok) => { notices = pushNotice(notices, { kind: toastKind(msg, ok), message: msg }, Date.now()); });
  onUiChange(trackSessions);
}

/** The lifecycle slice's DOM side, collected by actions/runtime.generated.ts. */
export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runLifecycle(id as LifecycleActionId, inv),
  snapshot: () => ({ lifecycle: lifecycleSnapshot() }),
  init: initLifecycle,
  initOrder: 40,
};
