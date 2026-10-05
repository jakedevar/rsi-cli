// DOM for operator attention: pending-question banner, pause/archive controls
// and the window-title count. All daemon text is rendered via textContent.
import { api, type Session } from './api.js';
import {
  type Answer, DECLINE_RESPONSE, type PauseLevel, attentionCount, formatAnswers, needsAttention,
  pauseLabel, pendingQuestions, questionKey,
} from './approvals-model.js';

export interface ApprovalDeps {
  toast: (msg: string, ok?: boolean) => void;
  /** Re-fetch the session list after a mutation. */
  refresh: () => void;
  selected: () => Session | undefined;
}

let deps: ApprovalDeps;
let bannerKey = '';
let pause: { id: string; status: string; level: PauseLevel } | undefined;
let lastCount = -1;

function $<T extends HTMLElement>(id: string): T {
  const e = document.getElementById(id);
  if (!e) throw new Error(`missing #${id}`);
  return e as T;
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

// ---------- banner ----------

async function submit(sessionId: string, text: string): Promise<void> {
  try {
    await api.answerQuestion(sessionId, text);
    deps.toast('Answer sent', true);
    bannerKey = '';
    deps.refresh();
  } catch (e) {
    deps.toast(`Answer failed: ${String(e)}`);
  }
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
    block.append(text);
    inputs.push({ checks, text });
    box.append(block);
  });
  if (questions.length === 0) {
    const text = el('textarea');
    text.rows = 2;
    text.placeholder = 'Your answer';
    inputs.push({ checks: [], text });
    box.append(text);
  }
  const actions = el('div', 'approval-actions');
  const decline = el('button', 'btn danger', 'Decline');
  decline.type = 'button';
  decline.addEventListener('click', () => void submit(s.id, DECLINE_RESPONSE));
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
  box.append(actions);
  return box;
}

/** Show the banner for the selected session; keeps input while the request is unchanged. */
export function renderApproval(s: Session | undefined): void {
  const host = $('approval-banner');
  if (!s || !needsAttention(s)) {
    host.hidden = true;
    if (bannerKey) host.replaceChildren();
    bannerKey = '';
    return;
  }
  const key = questionKey(s);
  host.hidden = false;
  if (key === bannerKey) return;
  bannerKey = key;
  host.replaceChildren(buildBanner(s));
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

/** Update pause/archive controls for the selected session. */
export function renderControls(s: Session | undefined): void {
  const pauseBtn = $<HTMLButtonElement>('hard-pause');
  const clearBtn = $<HTMLButtonElement>('clear-pause');
  const archiveBtn = $<HTMLButtonElement>('archive');
  const state = $('pause-state');
  if (!s) return;
  const live = s.status === 'Starting' || s.status === 'Running' || s.status === 'WaitingApproval';
  pauseBtn.disabled = !live;
  archiveBtn.disabled = s.status === 'Archived' || s.status === 'Deleted';
  if (!pause || pause.id !== s.id || pause.status !== s.status) {
    void refreshPause(s);
  }
  const level = pause && pause.id === s.id ? pause.level : 'none';
  state.textContent = pauseLabel(level);
  state.hidden = level === 'none';
  clearBtn.hidden = level === 'none';
}

async function hardPause(): Promise<void> {
  const s = deps.selected();
  if (!s) return;
  try {
    await api.pauseSession(s.id, 'hard');
    deps.toast('Hard pause requested', true);
    pause = undefined;
    deps.refresh();
  } catch (e) {
    deps.toast(`Pause failed: ${String(e)}`);
  }
}

async function clearPause(): Promise<void> {
  const s = deps.selected();
  if (!s) return;
  try {
    await api.setOperatorPause(s.id, 'none');
    deps.toast('Pause cleared', true);
    pause = undefined;
    renderControls(s);
  } catch (e) {
    deps.toast(`Clear pause failed: ${String(e)}`);
  }
}

function confirmDialog(message: string, confirmLabel: string): Promise<boolean> {
  return new Promise((resolve) => {
    const dlg = el('dialog', 'confirm');
    const form = el('form', 'launch-form');
    form.method = 'dialog';
    form.append(el('p', undefined, message));
    const actions = el('div', 'actions');
    const cancel = el('button', 'btn', 'Cancel');
    cancel.type = 'button';
    const ok = el('button', 'btn danger', confirmLabel);
    ok.type = 'submit';
    actions.append(cancel, ok);
    form.append(actions);
    dlg.append(form);
    document.body.append(dlg);
    let result = false;
    cancel.addEventListener('click', () => dlg.close());
    form.addEventListener('submit', () => { result = true; });
    dlg.addEventListener('close', () => { dlg.remove(); resolve(result); });
    dlg.showModal();
  });
}

async function archive(): Promise<void> {
  const s = deps.selected();
  if (!s) return;
  const name = s.title?.trim() || s.id.slice(0, 8);
  if (!(await confirmDialog(`Archive session "${name}"? It is hidden from the list; data is kept.`, 'Archive'))) return;
  try {
    await api.archiveSession(s.id);
    deps.toast('Session archived', true);
    deps.refresh();
  } catch (e) {
    deps.toast(`Archive failed: ${String(e)}`);
  }
}

// ---------- window title ----------

export function updateAttention(all: Session[]): void {
  const n = attentionCount(all);
  if (n === lastCount) return;
  lastCount = n;
  document.title = n > 0 ? `(${n}) RSI` : 'RSI';
  api.setAttentionCount(n).catch(() => { lastCount = -1; });
}

export function initApprovals(d: ApprovalDeps): void {
  deps = d;
  $('hard-pause').addEventListener('click', () => void hardPause());
  $('clear-pause').addEventListener('click', () => void clearPause());
  $('archive').addEventListener('click', () => void archive());
}
