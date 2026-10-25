// Operator messages and restart (keybindings.md Notes, gap sweep G1), DOM
// side. Every follow-up the desktop sends goes through deliver(): a typed
// `/pending …` or `/drain …` runs that command (as the TUI's
// continue_session does), a busy session gets its message queued for the next
// safe turn boundary (QueueOperatorMessage), anything else continues the
// session. The open session's pending count shows in its header. Rules live
// in input-model.ts; daemon text is set with textContent only.
import { type Session, api, invoke } from './api.js';
import { $, el, toast } from './dom.js';
import {
  type OperatorMessage, type PendingCommand, messagePreview, operatorSlash, parseOperatorCommand, pendingMessages,
  queuesFollowUp,
} from './input-model.js';
import { refreshSessions } from './sessions.js';
import { onUiChange, selected, state } from './state.js';
import { loadView } from './view-load.js';

/** Pending messages of each session, as last listed. */
const pending = new Map<string, OperatorMessage[]>();

/** How a follow-up went: continued the session, queued for it, or ran a command. */
export type Delivery = 'sent' | 'queued' | 'command';

export async function deliver(s: Session, text: string): Promise<Delivery> {
  const cmd = operatorSlash(text);
  if (cmd) {
    await runOperatorCommand(cmd);
    return 'command';
  }
  if (queuesFollowUp(s.status)) {
    await invoke('queue_operator_message', { sessionId: s.id, content: text });
    toast('Message queued; it reaches the agent at its next tool boundary (or next turn)', true);
    void refreshPending(s.id);
    return 'queued';
  }
  await api.sendMessage(s.id, text);
  void refreshSessions();
  return 'sent';
}

/** Reload one session's pending messages and redraw the header count. */
export async function refreshPending(sessionId: string): Promise<OperatorMessage[]> {
  try {
    const list = pendingMessages(await invoke<unknown>('list_operator_messages', { sessionId }));
    pending.set(sessionId, list);
    decorate();
    return list;
  } catch (e) {
    pending.delete(sessionId);
    decorate();
    throw e;
  }
}

/** `pending: N` in the open session's meta line. */
export function decorate(): void {
  const meta = document.getElementById('s-meta');
  if (!meta) return;
  const n = state.selectedId ? pending.get(state.selectedId)?.length ?? 0 : 0;
  const have = meta.querySelector<HTMLElement>('.meta-pending');
  if (n === 0) { have?.remove(); return; }
  const text = `${n}`;
  if (have?.lastElementChild?.textContent === text) return;
  have?.remove();
  const span = el('span', 'meta-pending');
  span.title = ':pending lists them';
  span.append('pending: ', el('span', undefined, text));
  meta.append(span);
}

export function pendingCount(sessionId: string): number {
  return pending.get(sessionId)?.length ?? 0;
}

/** `:pending …` / `:drain …` with their argument (the registry actions). */
export function runOperatorLine(name: 'pending' | 'drain', arg: string | undefined): void {
  void runOperatorCommand(parseOperatorCommand(name, arg ?? '')).catch((e: unknown) => toast(String(e)));
}

async function runOperatorCommand(cmd: PendingCommand): Promise<void> {
  const s = selected();
  switch (cmd.kind) {
    case 'error': toast(cmd.message); return;
    case 'drain': {
      const status = await invoke<{ active_sessions?: unknown[] }>('restart_daemon_drain').catch((e: unknown) => {
        throw new Error(`DRAIN restart failed: ${String(e)}`);
      });
      const n = Array.isArray(status?.active_sessions) ? status.active_sessions.length : 0;
      toast(`DRAIN restart requested; ${n} active session(s) finishing current turns`, true);
      return;
    }
    case 'drain-status':
      await loadView({
        key: 'drain-status', title: 'Drain status', failed: 'DRAIN status unavailable',
        read: () => invoke<unknown>('get_drain_restart_status'),
        open: (status) => showText('Drain status', JSON.stringify(status, null, 2)),
      });
      return;
    case 'list':
      if (!s) { toast('Open a session first: click one in the list (j/k, then Enter)'); return; }
      await loadView({
        key: 'pending-messages', title: 'Pending messages', failed: 'Pending messages unavailable',
        read: () => refreshPending(s.id),
        open: (list) => openPendingDialog(s, list),
      });
      return;
    case 'edit':
      if (!s) { toast('Open a session first: click one in the list (j/k, then Enter)'); return; }
      await editMessage(s.id, cmd.id, cmd.text);
      return;
    case 'withdraw':
      if (!s) { toast('Open a session first: click one in the list (j/k, then Enter)'); return; }
      await withdrawMessage(s.id, cmd.id);
      return;
  }
}

async function editMessage(sessionId: string, id: string, text: string): Promise<void> {
  const m = await invoke<OperatorMessage>('edit_operator_message', { messageId: id, content: text }).catch((e: unknown) => {
    throw new Error(`Edit failed: ${String(e)}`);
  });
  if (m?.session_id && m.session_id !== sessionId) { toast('Message belongs to another session'); return; }
  toast('Pending message edited', true);
  void refreshPending(sessionId).catch(() => undefined);
}

async function withdrawMessage(sessionId: string, id: string): Promise<void> {
  const m = await invoke<OperatorMessage>('withdraw_operator_message', { messageId: id }).catch((e: unknown) => {
    throw new Error(`Withdraw failed: ${String(e)}`);
  });
  if (m?.session_id && m.session_id !== sessionId) { toast('Message belongs to another session'); return; }
  toast('Pending message withdrawn', true);
  void refreshPending(sessionId).catch(() => undefined);
}

function dialog(id: string): HTMLDialogElement {
  let dlg = document.getElementById(id) as HTMLDialogElement | null;
  if (!dlg) {
    dlg = document.createElement('dialog');
    dlg.id = id;
    dlg.className = 'detail-dialog';
    dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg!.close(); });
    document.body.append(dlg);
  }
  return dlg;
}

function showText(title: string, text: string): void {
  const dlg = dialog('g1-text-dialog');
  const box = el('div', 'launch-form');
  const close = el('button', 'btn', 'Close');
  close.type = 'button';
  close.addEventListener('click', () => dlg.close());
  const actions = el('div', 'actions');
  actions.append(close);
  box.append(el('h2', undefined, title), el('pre', 'preview-body', text), actions);
  dlg.replaceChildren(box);
  dlg.showModal();
  close.focus();
}

/** The `:pending` list: id, state and preview, with edit and withdraw. */
function openPendingDialog(s: Session, list: readonly OperatorMessage[]): void {
  const dlg = dialog('g1-pending-dialog');
  const box = el('div', 'launch-form');
  const rows = el('ul', 'pick-list g1-pending');
  if (list.length === 0) rows.append(el('li', 'dialog-note', 'No pending messages for this session'));
  for (const m of list) {
    const li = el('li', 'pick-item');
    const head = el('div', 'g1-pending-head');
    head.append(el('span', 'mono', m.id), el('span', `g1-state g1-state-${m.state}`, m.state));
    const area = el('textarea');
    area.value = m.content;
    area.rows = 3;
    area.setAttribute('aria-label', `Message ${m.id}`);
    const save = el('button', 'btn', 'Save edit');
    save.type = 'button';
    save.addEventListener('click', () => void editMessage(s.id, m.id, area.value).then(() => dlg.close(), (e: unknown) => toast(String(e))));
    const withdraw = el('button', 'btn danger', 'Withdraw');
    withdraw.type = 'button';
    withdraw.addEventListener('click', () => void withdrawMessage(s.id, m.id).then(() => dlg.close(), (e: unknown) => toast(String(e))));
    const acts = el('div', 'actions');
    acts.append(save, withdraw);
    li.append(head, el('div', 'dialog-note', messagePreview(m.content)), area, acts);
    rows.append(li);
  }
  const close = el('button', 'btn', 'Close');
  close.type = 'button';
  close.addEventListener('click', () => dlg.close());
  const actions = el('div', 'actions');
  actions.append(close);
  box.append(
    el('h2', undefined, 'Pending messages'),
    el('div', 'dialog-note', 'Queued for the next safe turn boundary. An uncertain message may already have reached the provider, so it is never replayed.'),
    rows, actions,
  );
  dlg.replaceChildren(box);
  dlg.showModal();
  close.focus();
}

/** Keep the header count current: on selection and every few seconds while the session is busy. */
export function initPendingCount(): void {
  new MutationObserver(decorate).observe($('s-meta'), { childList: true });
  let last: string | null = null;
  const tick = (poll: boolean): void => {
    const s = selected();
    if (!s) { last = null; return; }
    if (s.id !== last || (poll && (queuesFollowUp(s.status) || pendingCount(s.id) > 0))) {
      last = s.id;
      void refreshPending(s.id).catch(() => undefined);
    }
  };
  window.setInterval(() => tick(true), 5000);
  onUiChange(() => tick(false));
}
