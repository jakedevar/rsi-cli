// DOM for operator attention: pending-question banner, pause/archive controls
// and the window-title count. All daemon text is rendered via textContent.
import { api, type Session } from './api.js';
import {
  type Answer, ANSWER_LAND_MS, ARCHIVE_UNDO_MS, answerLanding, DECLINE_RESPONSE, type PauseLevel, SKIP_LABEL, SKIP_NOTE, SKIP_TITLE, SKIP_TOAST, attentionCount, formatAnswers, needsAttention,
  newlyNeedingYou, ARCHIVE_CLEARED_TOAST, archiveButton, archiveConfirmText, archiveControls, archivedToastText, archivePending, archiveWhenFinishedConfirmText, defersArchive, WILL_ARCHIVE_TOAST, pauseLabel, pendingQuestions, questionKey, restoredToastText, UNDO_LABEL, waitingAnnouncement,
} from './approvals-model.js';
import { type ConfirmKeyState, confirmKey } from './confirm-model.js';
import { NONE_WAITING, nextWaiting, nextWaitingButton, nextWaitingTitle } from './next-waiting-model.js';
// The shared el(): it gives buttons and summaries their tab stop (local #242).
import { type ToastAction, el } from './dom.js';
import { focusInside, focusIsLost, keepFocusIn, landFocus } from './focus-return.js';
import { sessionLabel } from './model.js';
import { runHardPause } from './session-head-model.js';
import { notifyAttention } from './notify.js';
import { NEEDS_RSID } from './onboarding-model.js';
import { isReadOnly, state as daemonState, uiChanged } from './state.js';

export interface ApprovalDeps {
  toast: (msg: string, ok?: boolean, action?: ToastAction) => void;
  /** Re-fetch the session list after a mutation. */
  refresh: () => void;
  selected: () => Session | undefined;
  /** Draw the session list and the open session's pane from the state now, without a read (an archive or a mark shows at once). */
  redraw?: () => void;
  /** The sessions that wait for the operator, in session-list order (listmodel.attentionQueue). */
  waiting?: () => Session[];
  /** Open a session in the pane, as a click on its row does. */
  open?: (id: string) => void;
}

let deps: ApprovalDeps;
let bannerKey = '';
let pause: { id: string; status: string; level: PauseLevel } | undefined;
let lastCount = -1;
/** Ids that needed the operator at the last refresh; null until the first one (local #245). */
let attentionIds: Set<string> | null = null;

function $<T extends HTMLElement>(id: string): T {
  const e = document.getElementById(id);
  if (!e) throw new Error(`missing #${id}`);
  return e as T;
}

// ---------- banner ----------

/** When an answer was just sent for the banner on screen; its Submit button may not hold focus (a pointer click in WebKit). */
let answeredAt: number | null = null;

async function submit(sessionId: string, text: string, done = 'Answer sent'): Promise<void> {
  try {
    await api.answerQuestion(sessionId, text);
    answeredAt = Date.now();
    offerNextWaiting(sessionId);
    deps.toast(done, true);
    bannerKey = '';
    deps.refresh();
  } catch (e) {
    deps.toast(`Answer failed: ${String(e)}`);
  }
}

// ---------- next waiting (local #312) ----------

/** The session whose answer was just sent: its pane offers the next waiting session until the operator moves on. */
let offerFor: string | null = null;
const wiredGo = new WeakSet<HTMLElement>();

/** An answer (or "Let the agent decide") went through for `sessionId`: its pane now offers what waits next. */
export function offerNextWaiting(sessionId: string): void {
  offerFor = sessionId;
}

/** Open the next waiting session with its question's first field focused. */
function takeNextWaiting(): void {
  if (offerFor === null || !deps?.waiting || !deps.open) return;
  const { next } = nextWaiting(deps.waiting(), offerFor);
  if (!next) return;
  offerFor = null;
  deps.open(next.id);
  landOnOpenedSession();
}

/**
 * The bar under the question: "Next waiting (N)" while other sessions wait, else "No more sessions waiting". It shows once the
 * answered session has no question left, and goes when the operator opens another session. Returns the button when it is shown.
 */
function renderNextWaiting(s: Session | undefined): HTMLElement | null {
  const bar = document.getElementById('next-waiting');
  const text = document.getElementById('next-waiting-text');
  const go = document.getElementById('next-waiting-go');
  if (offerFor !== null && s?.id !== offerFor) offerFor = null;
  if (!bar || !text || !go) return null;
  const show = offerFor !== null && !!s && !needsAttention(s) && !!deps?.waiting;
  bar.hidden = !show;
  if (!show) { go.hidden = true; return null; }
  const { next, left } = nextWaiting(deps.waiting!(), offerFor!);
  const want = next ? `${sessionLabel(next)} is waiting for you` : NONE_WAITING;
  if (text.textContent !== want) text.textContent = want;
  go.hidden = !next;
  if (!next) return null;
  const label = nextWaitingButton(left);
  if (go.textContent !== label) go.textContent = label;
  go.title = nextWaitingTitle(next);
  if (!wiredGo.has(go)) { wiredGo.add(go); go.addEventListener('click', takeNextWaiting); }
  return go;
}

/** The first thing to type into in a banner: an option or the answer box. */
function firstField(host: HTMLElement): HTMLElement | null {
  return host.querySelector<HTMLElement>('input, textarea');
}

/**
 * The banner the keyboard user was in has closed or been rebuilt (the answer went through, or the next question arrived):
 * focus goes to the next question, else the message box, else the conversation, never the page (local #297).
 */
function landAfterBanner(s: Session | undefined, host: HTMLElement, nextGo: HTMLElement | null = null): void {
  const to = answerLanding({ nextQuestion: !!s && !host.hidden && needsAttention(s), sessionOpen: !!s, canMessage: !!s && !isReadOnly(s) });
  const message = document.getElementById('message');
  // Another session waits: Enter on "Next waiting" takes it; Tab or a click elsewhere ignores the offer (local #312).
  const targets = to === 'next-question' ? [firstField(host), message] : to === 'composer' ? [nextGo, message] : to === 'conversation' ? [nextGo] : [];
  landFocus([...targets, 'events', 'session-list']);
}

/**
 * A click on an OS notification opened a session (local #294, #297): the window just came forward, so focus goes to where
 * the notification wanted the operator: the question that is waiting, else the message box, else the conversation.
 */
export function landOnOpenedSession(): void {
  const host = document.getElementById('approval-banner');
  const banner = host && !host.hidden ? firstField(host) : null;
  landFocus([banner, 'message', 'events', 'session-list']);
}

function buildBanner(s: Session): HTMLElement {
  const box = el('div', 'approval');
  box.append(el('div', 'approval-title', 'Waiting for you'));
  const questions = pendingQuestions(s);
  const inputs: Array<{ checks: HTMLInputElement[]; text: HTMLTextAreaElement }> = [];
  if (questions.length === 0) {
    box.append(el('div', 'approval-q', 'This session is waiting for input. Reply below, or use the composer.'));
  }
  questions.forEach((q, qi) => {
    const block = el('div', 'approval-block');
    if (q.header) block.append(el('div', 'approval-header', q.header));
    block.append(el('div', 'approval-q', q.question));
    const checks: HTMLInputElement[] = [];
    (q.options ?? []).forEach((opt) => {
      const label = el('label', 'approval-opt');
      const input = el('input');
      input.type = q.multiSelect ? 'checkbox' : 'radio';
      input.name = `q${qi}`;
      checks.push(input);
      const body = el('span');
      body.append(el('strong', undefined, opt.label));
      if (opt.description) body.append(el('span', 'approval-desc', ` — ${opt.description}`));
      label.append(input, body);
      block.append(label);
    });
    const text = el('textarea');
    text.rows = 2;
    text.placeholder = (q.options?.length ?? 0) > 0 ? 'Or type your own answer' : 'Your answer';
    // A name that stays once text is typed (the placeholder goes): the question it answers (local #326).
    text.setAttribute('aria-label', `Your own answer to: ${q.question}`);
    block.append(text);
    inputs.push({ checks, text });
    box.append(block);
  });
  if (questions.length === 0) {
    const text = el('textarea');
    text.rows = 2;
    text.placeholder = 'Your answer';
    text.setAttribute('aria-label', 'Your reply to the waiting session');
    inputs.push({ checks: [], text });
    box.append(text);
  }
  const actions = el('div', 'approval-actions');
  // Neutral, not a red "deny": it sends "no preference; use your best judgment" (local #270).
  const decline = el('button', 'btn', SKIP_LABEL);
  decline.type = 'button';
  decline.title = SKIP_TITLE;
  decline.addEventListener('click', () => void submit(s.id, DECLINE_RESPONSE, SKIP_TOAST));
  const send = el('button', 'btn primary', 'Submit answer');
  send.type = 'button';
  send.addEventListener('click', () => {
    const qs = questions.length > 0 ? questions : [{ question: '', header: '', options: [] }];
    const answers: Answer[] = inputs.map((inp) => ({
      selected: inp.checks.flatMap((c, i) => (c.checked ? [i] : [])),
      text: inp.text.value,
    }));
    const res = formatAnswers(qs, answers);
    if (!res.ok) { deps.toast(res.error); return; }
    void submit(s.id, res.text);
  });
  actions.append(decline, send);
  box.append(actions, el('div', 'approval-note', SKIP_NOTE));
  return box;
}

/** Show the banner for the selected session; keeps input while the request is unchanged. */
export function renderApproval(s: Session | undefined): void {
  const host = $('approval-banner');
  // Whether the focused control is about to go with the banner: it holds focus, or an answer was just sent and focus is lost.
  const answered = answeredAt !== null && Date.now() - answeredAt < ANSWER_LAND_MS;
  const nextGo = renderNextWaiting(s);
  const inBanner = focusInside(host) || (answered && focusIsLost());
  if (!s || !needsAttention(s)) {
    host.hidden = true;
    if (bannerKey) host.replaceChildren();
    const gone = bannerKey !== '' || answered;
    bannerKey = '';
    if (gone) answeredAt = null;
    if (inBanner && gone) landAfterBanner(s, host, nextGo);
    return;
  }
  const key = questionKey(s);
  host.hidden = false;
  if (key === bannerKey) return;
  bannerKey = key;
  answeredAt = null;
  host.replaceChildren(buildBanner(s));
  if (inBanner) landAfterBanner(s, host);
}

// ---------- controls ----------

async function refreshPause(s: Session): Promise<void> {
  try {
    const r = await api.getOperatorPause(s.id);
    pause = { id: s.id, status: s.status, level: r.pause_level };
  } catch {
    pause = undefined;
  }
  if (deps.selected()?.id === s.id) renderControls(s);
}

/** While rsid is down a control that needs it says so in its tooltip; the usual tooltip comes back with the link (local #266). */
export function explainNeedsRsid(btn: HTMLElement, down: boolean): void {
  if (down) {
    if (btn.dataset.upTitle === undefined) btn.dataset.upTitle = btn.title;
    btn.title = NEEDS_RSID;
  } else if (btn.dataset.upTitle !== undefined) {
    btn.title = btn.dataset.upTitle;
    delete btn.dataset.upTitle;
  }
}

/** Update pause/archive controls for the selected session. */
export function renderControls(s: Session | undefined): void {
  // A button that hides or disables itself (Resume, Restore, Stop) must not take keyboard focus with it.
  keepFocusIn($('hard-pause').parentElement ?? $('hard-pause'), () => drawControls(s), ['message', 'events', 'session-list']);
}

function drawControls(s: Session | undefined): void {
  const pauseBtn = $<HTMLButtonElement>('hard-pause');
  const clearBtn = $<HTMLButtonElement>('clear-pause');
  const archiveBtn = $<HTMLButtonElement>('archive');
  const restoreBtn = $<HTMLButtonElement>('restore');
  const state = $('pause-state');
  if (!s) return;
  const live = s.status === 'Starting' || s.status === 'Running' || s.status === 'WaitingApproval';
  const down = daemonState.daemonDown;
  pauseBtn.disabled = !live || down;
  // An archived session swaps Archive for a plainly labelled Restore (local #268).
  const ac = archiveControls(s.status, down);
  archiveBtn.hidden = ac.archiveHidden;
  archiveBtn.disabled = ac.archiveDisabled;
  restoreBtn.hidden = ac.restoreHidden;
  restoreBtn.disabled = ac.restoreDisabled;
  // A running session cannot be archived yet: the button marks it to archive when it finishes, and cancels the mark (local #308).
  const ab = archiveButton(s);
  archiveBtn.textContent = ab.label;
  // While rsid is down the tooltip says so (explainNeedsRsid keeps the usual one aside); keep that aside current.
  if (archiveBtn.dataset.upTitle !== undefined) archiveBtn.dataset.upTitle = ab.title; else archiveBtn.title = ab.title;
  archiveBtn.classList.toggle('pending', archivePending(s));
  explainNeedsRsid(pauseBtn, down);
  explainNeedsRsid(archiveBtn, down);
  explainNeedsRsid(restoreBtn, down);
  if (!pause || pause.id !== s.id || pause.status !== s.status) {
    void refreshPause(s);
  }
  const level = pause && pause.id === s.id ? pause.level : 'none';
  state.textContent = pauseLabel(level);
  state.hidden = level === 'none';
  clearBtn.hidden = level === 'none';
  uiChanged();
}

/** Forget the cached pause so the next render re-reads it from the daemon. */
export function forgetPause(): void {
  pause = undefined;
}

/** The open session's operator pause, as last fetched ('none' until known). */
export function pauseLevel(sessionId: string): PauseLevel {
  return pause && pause.id === sessionId ? pause.level : 'none';
}

/**
 * Pause until I resume (the TUI's hard pause). A key press (X, already pressed
 * twice) sends at once; a button, menu or palette run asks first.
 */
export async function hardPause(confirmFirst = true): Promise<void> {
  const s = deps.selected();
  if (!s) return;
  const name = sessionLabel(s);
  try {
    const r = await runHardPause({
      name, confirmFirst, confirm: (m) => confirmDialog(m, 'Pause'),
      send: async () => { await api.pauseSession(s.id, 'hard'); },
    });
    if (r === 'cancelled') return;
    deps.toast('Paused until you resume', true);
    pause = undefined;
    deps.refresh();
  } catch (e) {
    deps.toast(`Pause failed: ${String(e)}`);
  }
}

export async function clearPause(): Promise<void> {
  const s = deps.selected();
  if (!s) return;
  try {
    await api.setOperatorPause(s.id, 'none');
    deps.toast('Resumed', true);
    pause = undefined;
    renderControls(s);
  } catch (e) {
    deps.toast(`Resume failed: ${String(e)}`);
  }
}

export interface ConfirmOptions {
  /** A heading above the message. */
  title?: string;
  /** Secondary lines under the message (the session and its branch). */
  detail?: readonly string[];
  cancelLabel?: string;
  /** The confirm button is the primary one, not a danger one. */
  primary?: boolean;
  /** Open with the dialog itself focused, so Enter and the chord confirm and Space does not press a button. */
  chord?: readonly string[];
  /** Whether the Vim keys are on; the chord confirms only then. */
  vimKeysOn?: () => boolean;
}

let confirmSeq = 0;

export function confirmDialog(message: string, confirmLabel: string, opts: ConfirmOptions = {}): Promise<boolean> {
  return new Promise((resolve) => {
    const dlg = el('dialog', 'confirm');
    const form = el('form', 'launch-form');
    form.method = 'dialog';
    // A confirm interrupts the operator, so it is an alertdialog named by its title, or by its message when it has none (local #245).
    dlg.setAttribute('role', 'alertdialog');
    const head = opts.title ? el('h3', undefined, opts.title) : null;
    const body = el('p', undefined, message);
    const stem = `confirm-${++confirmSeq}`;
    const nameFrom = head ?? body;
    nameFrom.id = `${stem}-name`;
    dlg.setAttribute('aria-labelledby', nameFrom.id);
    if (head) { body.id = `${stem}-desc`; dlg.setAttribute('aria-describedby', body.id); }
    if (head) form.append(head);
    form.append(body);
    for (const line of opts.detail ?? []) form.append(el('p', 'confirm-detail', line));
    const actions = el('div', 'actions');
    const cancel = el('button', 'btn', opts.cancelLabel ?? 'Cancel');
    cancel.type = 'button';
    const ok = el('button', opts.primary ? 'btn primary' : 'btn danger', confirmLabel);
    ok.type = 'submit';
    actions.append(cancel, ok);
    form.append(actions);
    dlg.append(form);
    document.body.append(dlg);
    let result = false;
    cancel.addEventListener('click', () => dlg.close());
    form.addEventListener('submit', () => { result = true; });
    dlg.addEventListener('close', () => { dlg.remove(); resolve(result); });
    if (opts.chord) {
      // Enter and the chord confirm from the dialog itself; a focused button keeps its own press.
      dlg.tabIndex = -1;
      let keys: ConfirmKeyState = { progress: 0 };
      dlg.addEventListener('keydown', (ev) => {
        const r = confirmKey(keys, { key: ev.key, ctrlKey: ev.ctrlKey, metaKey: ev.metaKey, altKey: ev.altKey, targetIsButton: (ev.target as HTMLElement | null)?.closest('button') != null }, { vimKeysOn: (opts.vimKeysOn ?? (() => true))(), chord: opts.chord });
        keys = r.st;
        if (!r.confirm) return;
        ev.preventDefault();
        form.requestSubmit(ok);
      });
    }
    dlg.showModal();
    if (opts.chord) dlg.focus();
  });
}

export async function archive(): Promise<void> {
  const s = deps.selected();
  if (!s) return;
  const name = s.title?.trim() || s.id.slice(0, 8);
  // rsid refuses to archive a running session: the TUI marks it to archive itself when it finishes, and so do we.
  if (defersArchive(s.status)) { await toggleArchiveWhenFinished(s, name); return; }
  if (!(await confirmDialog(archiveConfirmText(name), 'Archive'))) return;
  try {
    await api.archiveSession(s.id);
    // Archive is about to leave the header (Restore takes its place), so note now whether focus was on it or lost.
    const hadFocus = focusIsOnArchiveOrLost();
    // The pane shows the session as archived at once, before the list is read again (local #309).
    if (deps.selected()?.id === s.id) {
      daemonState.departed = { ...s, status: 'Archived', pending_archive: false };
      daemonState.cursorId = daemonState.cursorId === s.id ? null : daemonState.cursorId;
    }
    // Undo restores this session by id, whatever is open by then.
    deps.toast(archivedToastText(name), true, { label: UNDO_LABEL, ms: ARCHIVE_UNDO_MS, run: () => void undoArchive(s, name) });
    deps.redraw?.();
    deps.refresh();
    if (hadFocus) focusUndo();
  } catch (e) {
    deps.toast(`Archive failed: ${String(e)}`);
  }
}

/**
 * Archive on a Starting, Running or WaitingApproval session (the TUI's <Space>a): mark it to archive itself when it finishes, or clear
 * the mark. Setting it asks first; clearing is one press. The mark shows in the header and on the row.
 */
async function toggleArchiveWhenFinished(s: Session, name: string): Promise<void> {
  const desired = !archivePending(s);
  if (desired && !(await confirmDialog(archiveWhenFinishedConfirmText(name), 'Archive when finished', { primary: true }))) return;
  try {
    await api.markPendingArchive(s.id, desired);
    // Show it now; the next poll brings rsid's own copy.
    const mark = (x: Session): Session => (x.id === s.id ? { ...x, pending_archive: desired } : x);
    daemonState.sessions = daemonState.sessions.map(mark);
    deps.toast(desired ? WILL_ARCHIVE_TOAST : ARCHIVE_CLEARED_TOAST, true);
    deps.redraw?.();
    deps.refresh();
  } catch (e) {
    deps.toast(`Mark pending archive failed: ${String(e)}`);
  }
}

/**
 * Keyboard users: the toast sits at the end of the page, a long Tab away. When the confirmation closed and left
 * focus on nothing (or on the Archive button that is about to go), put it on Undo.
 */
function focusIsOnArchiveOrLost(): boolean {
  const at = document.activeElement;
  return !at || at === document.body || at.id === 'archive';
}

function focusUndo(): void {
  document.querySelector<HTMLButtonElement>('#toast .toast-action')?.focus();
}

/** The Undo button on the Archive toast: the same unarchive command the Restore button and the U key run. */
export async function undoArchive(was: Session, name: string): Promise<void> {
  try {
    await api.unarchiveSession(was.id);
    // Back in the list as it was before, until the next poll says more; the pane stops showing it as archived.
    if (daemonState.departed?.id === was.id) daemonState.departed = null;
    if (!daemonState.sessions.some((x) => x.id === was.id)) daemonState.sessions = [...daemonState.sessions, was];
    deps.toast(restoredToastText(name), true);
    deps.redraw?.();
    deps.refresh();
  } catch (e) {
    deps.toast(`Undo failed: ${String(e)}`);
  }
}

// ---------- window title, Dock badge and OS notifications ----------

export function updateAttention(all: Session[]): void {
  // A session that newly needs you is spoken through the toast's live region, which stays above
  // open dialogs; the row text and the Waiting button change silently (local #245).
  const { now, fresh } = newlyNeedingYou(attentionIds, all);
  attentionIds = now;
  if (fresh.length > 0) deps.toast(waitingAnnouncement(fresh.map(sessionLabel)), true);
  // Outside the window: an OS notification when the person is elsewhere (local #294).
  notifyAttention(all, deps?.selected()?.id);
  const n = attentionCount(all);
  if (n === lastCount) return;
  lastCount = n;
  document.title = n > 0 ? `(${n}) RSI` : 'RSI';
  api.setAttentionCount(n).catch(() => { lastCount = -1; });
}

/** The pause and archive buttons run through the action registry (commands.ts). */
export function initApprovals(d: ApprovalDeps): void {
  deps = d;
}
