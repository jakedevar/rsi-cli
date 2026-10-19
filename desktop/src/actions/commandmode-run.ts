// The DOM side of the Command Mode rows (actions/commandmode.ts): each command
// forwards one rsid method through a src-tauri/src/commandmode.rs command and
// reports the TUI's toast, or runs the desktop substitute of a window or tab
// command. `:manual` opens the generated manual in an in-app reader. Daemon
// text is set with textContent only; the manual's HTML comes from the Rust
// sanitizer that renders transcripts.
import { api, invoke } from '../api.js';
import { type Invocation, type SliceRuntime, dispatch, snapshot } from '../commands.js';
import {
  ABANDON_NOTICE, type Armed, COLD_MOVE_CONFIRM, RESEAT_NOTICE, WINDOW_SUBSTITUTES, abandonMessage, blankLaunch,
  coldHasProblems, coldLastRunLine, coldStartedMessage, coldStatusLine, contextMessage, contextText, parseAbandonTarget,
  parseColdArchive, parseReseatArgs, reseatMessage, restoreMessage, secondPress,
} from '../commandmode-model.js';
import { $, el, toast } from '../dom.js';
import { parseLaunchDefault } from '../nav-model.js';
import { unavailable } from '../registry.js';
import { refreshSessions, renderSessionList } from '../sessions.js';
import { currentSessions, selected, state } from '../state.js';
import { injectCss } from '../sweep-css.js';
import { type CommandModeActionId, type CommandModeSnapshot, SLICE } from './commandmode.js';
import { detailSnapshot } from './detail.js';
import { focusOwnsKey } from '../modal-gate.js';

type Json = Record<string, unknown>;

const errText = (e: unknown): string => (e instanceof Error ? e.message : String(e));
const mint = (): string => crypto.randomUUID();

// ---------- targets ----------

function target(): CommandModeSnapshot['target'] {
  const s = selected() ?? currentSessions().find((x) => x.id === state.cursorId);
  return s ? { id: s.id, project_id: (s as { project_id?: string | null }).project_id ?? null, status: String(s.status) } : null;
}

export function commandModeSnapshot(): CommandModeSnapshot {
  return { target: target() };
}

// ---------- confirm gate (run the same command again within 10 s) ----------

let armed: Armed | null = null;

function confirmed(key: string, notice: string): boolean {
  const r = secondPress(armed, key, Date.now());
  armed = r.next;
  if (!r.confirmed) toast(notice);
  return r.confirmed;
}

// ---------- session commands ----------

async function abandonRotation(inv: Invocation): Promise<void> {
  const t = target();
  if (!t) return;
  const parsed = parseAbandonTarget(inv.arg);
  if ('error' in parsed) { toast(parsed.error); return; }
  if (!confirmed(`abandon:${t.id}:${inv.arg?.trim() ?? ''}`, ABANDON_NOTICE)) return;
  try {
    const r = await invoke<Json>('abandon_rotation', { sessionId: t.id, provider: parsed.provider, model: parsed.model, idempotencyKey: mint() });
    toast(abandonMessage(r), true);
    void refreshSessions();
  } catch (e) {
    toast(`Abandon failed: ${errText(e)}`);
  }
}

async function reseatManager(inv: Invocation): Promise<void> {
  const t = target();
  if (!t) return;
  const parsed = parseReseatArgs(inv.arg);
  if ('error' in parsed) { toast(parsed.error); return; }
  if (!t.project_id) { toast('The selected session belongs to no project'); return; }
  if (!confirmed(`reseat:${t.id}:${inv.arg?.trim() ?? ''}`, RESEAT_NOTICE)) return;
  try {
    const r = await invoke<Json>('reseat_manager', { projectId: t.project_id, provider: parsed.provider, model: parsed.model, reason: parsed.reason });
    toast(reseatMessage(r), true);
    void refreshSessions();
  } catch (e) {
    toast(`Reseat failed: ${errText(e)}`);
  }
}

async function coldRestore(): Promise<void> {
  const t = target();
  if (!t) return;
  try {
    toast(restoreMessage(await invoke<Json>('cold_restore', { sessionId: t.id })), true);
  } catch (e) {
    toast(`Restore from cold storage failed: ${errText(e)}`);
  }
}

function confirmMove(): Promise<boolean> {
  return new Promise((resolve) => {
    const d = el('dialog', 'pick');
    const form = el('form');
    form.method = 'dialog';
    form.append(el('h2', undefined, 'Move verified sessions to cold storage?'), el('p', 'pick-hint', COLD_MOVE_CONFIRM));
    const actions = el('div', 'pick-actions');
    const cancel = el('button', 'btn', 'Cancel');
    cancel.type = 'button';
    const ok = el('button', 'btn primary', 'Move');
    ok.type = 'submit';
    actions.append(cancel, ok);
    form.append(actions);
    d.append(form);
    document.body.append(d);
    const done = (v: boolean) => { d.close(); d.remove(); resolve(v); };
    cancel.addEventListener('click', () => done(false));
    form.addEventListener('submit', (ev) => { ev.preventDefault(); done(true); });
    d.addEventListener('cancel', (ev) => { ev.preventDefault(); done(false); });
    d.showModal();
    cancel.focus();
  });
}

async function coldArchive(inv: Invocation): Promise<void> {
  const cmd = parseColdArchive(inv.arg);
  if ('error' in cmd) { toast(cmd.error); return; }
  try {
    if (cmd.kind === 'run') {
      if (cmd.mode === 'move' && !(await confirmMove())) return;
      toast(coldStartedMessage(await invoke<Json>('cold_archive_run', { mode: cmd.mode })), true);
    }
    const status = await invoke<Json>('cold_archive_status');
    const line = `Cold storage: ${coldStatusLine(status)} — last pass: ${coldLastRunLine(status)}`;
    if (cmd.kind === 'status') toast(line, !coldHasProblems(status));
  } catch (e) {
    toast(cmd.kind === 'run' ? `Cold-storage ${cmd.mode} pass: ${errText(e)}` : `Cold storage: the status read failed: ${errText(e)}`);
  }
}

async function setContext(inv: Invocation): Promise<void> {
  const t = target();
  if (!t) return;
  const text = contextText(inv.arg);
  try {
    await invoke<unknown>('update_active_task', { sessionId: t.id, activeTask: text });
    const row = currentSessions().find((s) => s.id === t.id) as (Json & { id: string }) | undefined;
    if (row) row.active_task = text;
    toast(contextMessage(text), true);
  } catch (e) {
    toast(`Failed to update active task: ${errText(e)}`);
  }
}

// ---------- :blank <text> ----------

async function launchBlank(text: string): Promise<void> {
  let defaults = null;
  try { defaults = parseLaunchDefault(globalThis.localStorage?.getItem('rsi.desktop.launchDefault')); } catch { defaults = null; }
  const pid = $<HTMLSelectElement>('project-filter').value;
  const project = state.projects.find((p) => p.id === pid) ?? null;
  try {
    await api.launch(blankLaunch(text, defaults, project) as unknown as Parameters<typeof api.launch>[0]);
    toast('Session launched', true);
    window.setTimeout(() => void refreshSessions(), 500);
  } catch (e) {
    toast(`Launch failed: ${errText(e)}`);
  }
}

// ---------- window and tab substitutes ----------

function moveKeys(): void {
  if (detailSnapshot().focus === 'transcript') { dispatch('ascend-or-back', { source: 'key' }); return; }
  if (state.selectedId || state.cursorId) { dispatch('session-normal', { source: 'key' }); return; }
  toast(WINDOW_SUBSTITUTES.split);
}

function closeOverlay(): void {
  if (unavailable('close-pane', snapshot())) { toast(WINDOW_SUBSTITUTES.only); return; }
  dispatch('close-pane', { source: 'key' });
}

function allProjects(): void {
  const sel = $<HTMLSelectElement>('project-filter');
  sel.value = '';
  renderSessionList();
  toast('All projects', true);
}

// ---------- :manual ----------

const CSS = `
dialog.cm-manual { width: min(980px, 96vw); height: min(88vh, 1100px); padding: 0; display: flex; flex-direction: column; }
dialog.cm-manual:not([open]) { display: none; }
.cm-manual-head { display: flex; gap: 8px; align-items: center; padding: 8px 12px; border-bottom: 1px solid var(--surface1); background: var(--mantle); }
.cm-manual-head h2 { margin: 0; font-size: 15px; color: var(--mauve); flex: 0 0 auto; }
.cm-manual-head select { flex: 1 1 auto; min-width: 0; }
.cm-manual-body { overflow: auto; padding: 8px 18px 24px; flex: 1; font-size: 13px; line-height: 1.5; }
.cm-manual-body table { border-collapse: collapse; margin: 8px 0; }
.cm-manual-body th, .cm-manual-body td { border: 1px solid var(--surface1); padding: 2px 6px; vertical-align: top; }
.cm-manual-body code { font-family: var(--mono); }
@media print { body > *:not(dialog.cm-manual) { display: none !important; } dialog.cm-manual { position: static; width: auto; height: auto; } .cm-manual-head { display: none; } .cm-manual-body { overflow: visible; } }
`;

let sections: Array<{ title: string; html: string }> | null = null;

async function openManual(inv: Invocation): Promise<void> {
  const mode = (inv.arg ?? '').trim();
  if (mode && mode !== 'pager' && mode !== 'pdf') { toast('Use :manual [pager|pdf]'); return; }
  try {
    sections ??= await invoke<Array<{ title: string; html: string }>>('manual_sections');
  } catch (e) {
    toast(`Manual: ${errText(e)}`);
    return;
  }
  injectCss('cm-manual-css', CSS);
  document.querySelector('dialog.cm-manual')?.remove();
  const d = el('dialog', 'cm-manual');
  d.setAttribute('aria-label', 'rsi manual');
  const head = el('div', 'cm-manual-head');
  const pick = el('select');
  pick.setAttribute('aria-label', 'Manual section');
  for (const [i, s] of sections.entries()) { const o = el('option', undefined, s.title); o.value = String(i); pick.append(o); }
  const print = el('button', 'btn', 'Print / PDF…');
  print.type = 'button';
  const close = el('button', 'btn', 'Close');
  close.type = 'button';
  head.append(el('h2', undefined, 'rsi manual'), pick, print, close);
  const body = el('div', 'cm-manual-body');
  body.tabIndex = 0;
  // Sanitized by src-tauri/src/markdown.rs (the transcript's ammonia allowlist).
  body.innerHTML = sections.map((s, i) => `<section data-section="${i}">${s.html}</section>`).join('');
  d.append(head, body);
  document.body.append(d);
  pick.addEventListener('change', () => body.querySelector(`[data-section="${pick.value}"]`)?.scrollIntoView({ block: 'start' }));
  print.addEventListener('click', () => window.print());
  close.addEventListener('click', () => { d.close(); d.remove(); });
  d.addEventListener('close', () => d.remove());
  d.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.target instanceof HTMLSelectElement) return;
    const step = ev.key === 'j' || ev.key === 'ArrowDown' ? 40 : ev.key === 'k' || ev.key === 'ArrowUp' ? -40 : 0;
    if (step) { body.scrollBy(0, step); ev.preventDefault(); }
    else if (ev.key === 'q') { d.close(); ev.preventDefault(); }
    else if (ev.key === 'g') { body.scrollTop = 0; ev.preventDefault(); }
    else if (ev.key === 'G') { body.scrollTop = body.scrollHeight; ev.preventDefault(); }
  });
  d.showModal();
  body.focus();
  if (mode === 'pdf') window.print();
}

// ---------- dispatch ----------

export function runCommandMode(id: CommandModeActionId, inv: Invocation): void {
  switch (id) {
    case 'cm-manual': void openManual(inv); return;
    case 'cm-rotation-abandon': void abandonRotation(inv); return;
    case 'cm-manager-reseat': void reseatManager(inv); return;
    case 'cm-cold-restore': void coldRestore(); return;
    case 'cm-cold-archive': void coldArchive(inv); return;
    case 'cm-context': void setContext(inv); return;
    case 'cm-split': case 'cm-vsplit': moveKeys(); return;
    case 'cm-only': closeOverlay(); return;
    case 'cm-tabnew': dispatch('choose-project', { source: inv.source }); return;
    case 'cm-tabclose': allProjects(); return;
  }
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runCommandMode(id as CommandModeActionId, inv),
  snapshot: () => ({ commandMode: commandModeSnapshot() }),
  /** `:blank <text>` launches directly (the TUI's LaunchBlank); bare `:blank` stays the dialog. */
  claim: (id, inv) => {
    const text = inv.arg?.trim();
    if (id !== 'launch' || !text) return null;
    void launchBlank(text);
    return 'launch';
  },
};
