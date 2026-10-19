// The Settings rows backed by daemon state RPCs (hub #3009, local #125), DOM
// side. Each row the TUI Settings page serves with an rsid call gets a real
// control here: the status rows load their line (R refreshes), the action rows
// run the same operator method as the TUI, and the rows that own a whole view
// open the existing admin overlay. The model-control mode is a staged choice
// in the pane itself (settings-model.ts). Pure wording lives in
// settings-state-model.ts; daemon text is set with textContent only.
import { invoke } from './api.js';
import { confirmDialog } from './approvals.js';
import { dispatch } from './commands.js';
import { coldLastRunLine, coldStartedMessage, coldStatusLine } from './commandmode-model.js';
import { el, toast } from './dom.js';
import { focusOwnsKey } from './modal-gate.js';
import { registerRowOverlay } from './settings.js';
import {
  CANCEL_CONFIRM, RECLAIM_CONFIRM, type UsageLine, allowedWork, budgetAlerts, cancelMessage, deniedInvocations,
  liveInvocations, modeOfStatus, poolSummary, reclaimFailure, reclaimMessage, storageSummary, usageOverview,
  usagePerModel, usageRowValue,
} from './settings-state-model.js';
import type { ActionId } from './registry.js';

const errText = (e: unknown): string => (e instanceof Error ? e.message : String(e));
type Json = Record<string, unknown>;

/** Loaded status lines by row id; null until the first read settles. */
const lines = new Map<string, string>();
let usageStats: unknown = null;
let modelControl: unknown = null;

const unavailable = (e: unknown): string => `Unavailable: ${errText(e)}`;

async function loadCold(): Promise<void> {
  try {
    const s = await invoke<Json>('cold_archive_status');
    lines.set('cold_archive_status', coldStatusLine(s));
    lines.set('cold_archive_last_run', coldLastRunLine(s));
  } catch (e) {
    lines.set('cold_archive_status', unavailable(e));
    lines.set('cold_archive_last_run', unavailable(e));
  }
}

async function loadStorage(): Promise<void> {
  try {
    lines.set('sandbox_storage_status', storageSummary(await invoke<unknown>('sandbox_storage_status')));
  } catch (e) {
    lines.set('sandbox_storage_status', unavailable(e));
  }
}

async function loadPool(): Promise<void> {
  try {
    lines.set('target_pool_status', poolSummary(await invoke<unknown>('target_pool_status')));
  } catch (e) {
    lines.set('target_pool_status', unavailable(e));
  }
}

async function loadUsage(): Promise<void> {
  const [stats, status] = await Promise.allSettled([invoke<unknown>('usage_stats'), invoke<unknown>('model_control_overview')]);
  usageStats = stats.status === 'fulfilled' ? stats.value : null;
  modelControl = status.status === 'fulfilled' ? status.value : null;
  const failed = stats.status === 'rejected' ? stats.reason : status.status === 'rejected' ? status.reason : null;
  const line = failed !== null && usageStats === null && modelControl === null ? unavailable(failed) : usageRowValue(usageStats, modelControl);
  lines.set('usage_stats', line);
  lines.set('usage_actions', line);
}

// ---------- actions ----------

async function runCold(): Promise<void> {
  try {
    toast(coldStartedMessage(await invoke<Json>('cold_archive_run', { mode: 'stage' })), true);
  } catch (e) {
    toast(`Cold-storage stage pass: ${errText(e)}`);
  }
  await loadCold();
}

async function runReclaim(dryRun: boolean): Promise<void> {
  if (!dryRun && !(await confirmDialog(RECLAIM_CONFIRM, 'Reclaim now'))) return;
  try {
    const report = await invoke<unknown>('sandbox_cache_reclaim', { dryRun });
    toast(reclaimMessage(report, dryRun), true);
  } catch (e) {
    toast(reclaimFailure(errText(e)));
  }
  await loadStorage();
}

async function cancelInvocation(id: string): Promise<boolean> {
  if (!(await confirmDialog(CANCEL_CONFIRM, 'Cancel invocation'))) return false;
  try {
    toast(cancelMessage(await invoke<unknown>('cancel_model_invocation', { invocationId: id })), true);
  } catch (e) {
    toast(`Failed to cancel model invocation: ${errText(e)}`);
  }
  return true;
}

// ---------- the usage and model-control view ----------

function section(title: string, rows: UsageLine[], empty: string): HTMLElement[] {
  const head = el('h3', undefined, title);
  if (rows.length === 0) return [head, el('div', 'pick-empty', empty)];
  const ul = el('ul', 'pick-list usage-rows');
  for (const r of rows) {
    const li = el('li', 'pick-item');
    li.append(el('span', 'usage-label', r.label), document.createTextNode(' '), el('span', 'usage-value', r.value));
    ul.append(li);
  }
  return [head, ul];
}

/**
 * Settings › Usage: the TUI's stats view over `GetUsageStats` and
 * `GetModelControlStatus`, with its two destructive actions: cancel a live model
 * invocation (j/k pick, Enter or c) and stop all (X). Both ask first; the TUI does not.
 */
export function openUsage(): void {
  const dlg = el('dialog', 'pick usage-dialog');
  dlg.setAttribute('aria-label', 'Usage and model control');
  const body = el('div', 'usage-body');
  const live = el('ul', 'pick-list usage-live');
  dlg.append(el('h2', undefined, 'Usage and model control'), body, el('h3', undefined, 'Live model work'), live,
    el('div', 'pick-hint', 'j/k pick live work · Enter or c cancels it (asks) · X stops all model work (asks) · R refresh · Esc or q close'));
  document.body.append(dlg);
  let sel = 0;
  const draw = (): void => {
    const status = modelControl;
    const mode = modeOfStatus(status);
    const mc: UsageLine[] = [{ label: 'Allowed work', value: allowedWork(mode) }];
    body.replaceChildren(
      ...section('Lifetime usage', usageOverview(usageStats), 'Usage statistics are unavailable'),
      ...section('Spending controls', mc, ''),
      ...section('Spend by model', usagePerModel(usageStats), 'No per-model spend recorded'),
      ...section('Warnings and limits', budgetAlerts(status), 'No budget warnings'),
      ...section('Blocked requests', deniedInvocations(status), 'Nothing blocked recently'),
    );
    const items = liveInvocations(status);
    sel = Math.max(0, Math.min(items.length - 1, sel));
    live.replaceChildren(...(items.length === 0
      ? [el('li', 'pick-empty', 'No live model work · Idle')]
      : items.map((it, i) => {
        const li = el('li', `pick-item${i === sel ? ' sel' : ''}`);
        li.append(el('span', 'usage-label', it.label), document.createTextNode(' '), el('span', 'usage-value', it.value));
        li.addEventListener('click', () => { sel = i; draw(); });
        li.addEventListener('dblclick', () => void cancel());
        return li;
      })));
    live.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  };
  const refresh = async (): Promise<void> => { await loadUsage(); draw(); };
  const cancel = async (): Promise<void> => {
    const it = liveInvocations(modelControl)[sel];
    if (!it) { toast('No live model work to cancel'); return; }
    if (!it.running) { toast('Only running model work can be cancelled'); return; }
    if (await cancelInvocation(it.id)) await refresh();
  };
  dlg.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    let handled = true;
    switch (ev.key) {
      case 'j': case 'ArrowDown': sel++; draw(); break;
      case 'k': case 'ArrowUp': sel--; draw(); break;
      case 'Enter': case 'c': void cancel(); break;
      case 'X': dispatch('emergency-stop' as ActionId, { source: 'button' }); break;
      case 'R': void refresh(); break;
      case 'q': dlg.close(); break;
      default: handled = false;
    }
    if (handled) { ev.preventDefault(); ev.stopPropagation(); }
  });
  dlg.addEventListener('close', () => dlg.remove());
  dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg.close(); });
  draw();
  dlg.showModal();
  dlg.focus();
  void refresh();
}

// ---------- registration ----------

function openAdmin(id: string): () => void {
  return () => dispatch(id as ActionId, { source: 'button' });
}

/** Serve the daemon-state Settings rows (called from the settings slice's init). */
export function registerStateRows(): void {
  const view = (id: string) => (): string | null => lines.get(id) ?? null;
  registerRowOverlay('emergency_stop', {
    enter: 'Asks, then denies new model work, cancels active invocations and interrupts running sessions (UpdateModelControlPolicy stop_all)',
    stored: 'the daemon (model control)',
    run: openAdmin('emergency-stop'),
  });
  for (const id of ['usage_stats', 'usage_actions']) {
    registerRowOverlay(id, {
      enter: 'Opens Usage: lifetime spend, model control, live model work (Enter cancels the picked one, X stops all; both ask first)',
      stored: 'the daemon (GetUsageStats, GetModelControlStatus)',
      value: view(id),
      refresh: loadUsage,
      run: () => openUsage(),
    });
  }
  const admin: ReadonlyArray<[string, string, string, string]> = [
    ['source_worktree_settlement', 'admin-settlement', 'Opens the settlement audit (applying asks for the exact APPLY phrase)', 'the daemon (source-worktree cohorts)'],
    ['legacy_scratch_adoption', 'admin-scratch', 'Opens the unrecorded scratch directories (adopting lists every full path to confirm)', 'the daemon (ListLegacyScratch, AdoptLegacyScratch)'],
    ['satellite_registry', 'admin-satellites', 'Opens the satellite registry: peers, links, cached remote sessions and the inbound policy', 'the daemon (satellite registry)'],
    ['remote_access', 'admin-remote', 'Opens remote access: the read-only phone view, devices and exposed projects', 'the daemon (remote access)'],
    ['mcp_server_configurations', 'admin-mcp', 'Opens the MCP servers (add, edit, enable or disable; credentials are write-only)', 'the daemon (ListMcpServers, UpsertMcpServer)'],
  ];
  for (const [row, action, enter, stored] of admin) registerRowOverlay(row, { enter, stored, run: openAdmin(action) });

  for (const id of ['cold_archive_status', 'cold_archive_last_run']) {
    registerRowOverlay(id, {
      enter: 'Refreshes the status (GetColdArchiveStatus) and announces it',
      stored: 'the daemon (GetColdArchiveStatus)',
      value: view(id),
      refresh: loadCold,
      run: () => {
        void loadCold().then(() => toast(`Cold storage: ${lines.get(id) ?? 'unavailable'}`, !(lines.get(id) ?? '').startsWith('Unavailable')));
      },
    });
  }
  registerRowOverlay('cold_archive_run_now', {
    enter: 'Starts a copy-only archive pass (stage and verify) in the background; :cold-archive move and verify are commands',
    stored: 'the daemon (RunColdArchiveNow stage)',
    value: () => 'Enter starts a pass',
    run: () => void runCold(),
  });
  registerRowOverlay('sandbox_storage_status', {
    enter: 'Refreshes the sandbox storage report (GetSandboxStorageStatus)',
    stored: 'the daemon (GetSandboxStorageStatus)',
    value: view('sandbox_storage_status'),
    refresh: loadStorage,
    run: () => {
      void loadStorage().then(() => toast(lines.get('sandbox_storage_status') ?? 'unavailable', !(lines.get('sandbox_storage_status') ?? '').startsWith('Unavailable')));
    },
  });
  registerRowOverlay('preview_reclaim', {
    enter: 'Runs a dry-run reclaim pass and reports what it would free (nothing is deleted)',
    stored: 'the daemon (RunSandboxBuildCacheReclaim dry_run)',
    value: () => 'Enter previews',
    run: () => void runReclaim(true),
  });
  registerRowOverlay('reclaim_now', {
    enter: 'Asks, then runs a real reclaim pass over the eligible sandbox target caches',
    stored: 'the daemon (RunSandboxBuildCacheReclaim)',
    value: () => 'Enter reclaims (asks)',
    run: () => void runReclaim(false),
  });
  registerRowOverlay('target_pool_status', {
    enter: 'Refreshes the warm build pools (GetTargetPoolStatus)',
    stored: 'the daemon (GetTargetPoolStatus)',
    value: view('target_pool_status'),
    refresh: loadPool,
    run: () => {
      void loadPool().then(() => toast(lines.get('target_pool_status') ?? 'unavailable', !(lines.get('target_pool_status') ?? '').startsWith('Unavailable')));
    },
  });
}
