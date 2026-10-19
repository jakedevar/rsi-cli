// Pure helpers for the Settings rows backed by daemon state RPCs (hub #3009,
// local #125): the model-control mode, usage and model-control telemetry,
// sandbox storage, warm build pools and the sandbox cache reclaim. The wording
// mirrors the TUI (crates/rsi/src/settings.rs, model_control_stats.rs,
// action_handler/daemon_config.rs, rsi-common target_pool.rs). Tested in
// tests/settings-state.test.mjs; the DOM side is settings-state.ts.

export type Obj = Record<string, unknown>;

const isObj = (v: unknown): v is Obj => typeof v === 'object' && v !== null && !Array.isArray(v);
const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);
const str = (v: unknown): string => (typeof v === 'string' ? v : '');

// ---------- model-control mode ----------

/** The TUI row's options, in its cycle order (`DaemonFeatureEntry::defaults`). */
export const MODEL_CONTROL_MODES: readonly string[] = ['normal', 'pause-background', 'deny-paid', 'local-only', 'stop-all'];

/** The daemon's snake_case mode as the TUI's hyphenated slug (`model_control_mode_slug`). */
export function modeSlug(wire: unknown): string | null {
  const slug = str(wire).replace(/_/g, '-');
  return MODEL_CONTROL_MODES.includes(slug) ? slug : null;
}

/** The mode of a `GetModelControlStatus` answer, or null when it carries none. */
export function modeOfStatus(status: unknown): string | null {
  return isObj(status) ? modeSlug(status.mode) : null;
}

/** The "Allowed work" line of the TUI's stats view. */
export function allowedWork(slug: string | null): string {
  switch (slug) {
    case 'normal': return 'Normal · budgets apply';
    case 'pause-background': return 'Background work paused';
    case 'deny-paid': return 'Paid work blocked';
    case 'local-only': return 'Local models only';
    case 'stop-all': return 'All model work stopped';
    default: return 'unknown';
  }
}

/** The toast after a mode change (`set_model_control_mode`). */
export function modeMessage(report: unknown, requested: string): string {
  const slug = isObj(report) ? modeSlug(report.current_mode) : null;
  return `Model control mode: ${slug ?? requested}`;
}

// ---------- bytes ----------

/** `format_binary_bytes` of settings.rs. */
export function formatBinaryBytes(bytes: number): string {
  const GIB = 1024 ** 3;
  const MIB = 1024 ** 2;
  if (bytes >= GIB) return `${(bytes / GIB).toFixed(1)} GiB`;
  if (bytes >= MIB) return `${(bytes / MIB).toFixed(1)} MiB`;
  return `${bytes} B`;
}

// ---------- sandbox storage and the reclaim ----------

/** A storage answer is the report itself or a v2 wrapper holding it as `report`. */
export function storageReport(answer: unknown): Obj | null {
  if (!isObj(answer)) return null;
  return isObj(answer.report) ? answer.report : answer;
}

/** `sandbox_storage_report_summary` (settings.rs). */
export function storageSummary(answer: unknown): string {
  const r = storageReport(answer);
  if (!r) return 'Unavailable: the daemon sent no report';
  const fs = isObj(r.filesystem_before) ? r.filesystem_before : {};
  return `${num(r.candidates_considered)} checked · ${num(r.eligible_candidates)} eligible · ${num(fs.used_percent)}% used · ${formatBinaryBytes(num(fs.available_bytes))} free · capacity estimate unavailable`;
}

/** `sandbox_cache_reclaim_success_message` (daemon_config.rs). */
export function reclaimMessage(answer: unknown, dryRun: boolean): string {
  const r = storageReport(answer);
  if (!r) return `Sandbox cache reclaim failed: the daemon sent no report`;
  if (dryRun) {
    return `Preview: ${num(r.eligible_candidates)} eligible of ${num(r.candidates_considered)} checked; capacity estimate unavailable`;
  }
  return `Reclaim: ${num(r.fully_removed_count)} removed, ${num(r.newly_staged_count)} staged, ${num(r.pending_count)} pending, ${formatBinaryBytes(num(r.reclaimed_bytes))} measured free`;
}

export const reclaimFailure = (error: string): string => `Sandbox cache reclaim failed: ${error}`;

/** The desktop asks before a real pass (the registry marks the row destructive; the TUI does not ask). */
export const RECLAIM_CONFIRM = 'Reclaim sandbox build caches now? This removes eligible target caches from disk; use Preview first to see what it would free.';

// ---------- warm build pools ----------

const short = (sha: string): string => sha.slice(0, 8);
const gib = (bytes: number): string => `${(bytes / 2 ** 30).toFixed(1)} GiB`;

/** `TargetPoolStatus::summary_line` (rsi-common target_pool.rs). */
export function poolSummary(status: unknown): string {
  if (!isObj(status)) return 'Unavailable: the daemon sent no status';
  const newest = isObj(status.newest) ? status.newest : null;
  const failedLatest = isObj(status.failed_latest) ? status.failed_latest : null;
  const failedNewer = num(status.failed_newer);
  let line: string;
  if (!newest) {
    line = failedLatest ? `No pool · last build ${short(str(failedLatest.sha))} failed` : 'No pool yet';
  } else {
    const age = newest.age_commits === null || newest.age_commits === undefined ? 'age unknown'
      : num(newest.age_commits) === 0 ? 'at tip' : `${num(newest.age_commits)} commits behind`;
    const degraded = newest.degraded ? ' (libraries only)' : '';
    const failed = failedNewer > 0 ? ` · ${failedNewer} newer build(s) failed` : '';
    line = `${short(str(newest.sha))}${degraded} ${age} · ${num(status.pools)} pool(s), ${gib(num(status.total_bytes))}${failed}`;
  }
  const newestAt = newest && typeof newest.built_at === 'number' ? newest.built_at : null;
  if (failedLatest && typeof failedLatest.reason === 'string' && failedLatest.reason
    && (newestAt === null || (typeof failedLatest.built_at === 'number' && newestAt < failedLatest.built_at))) {
    line += ` · ${failedLatest.reason}`;
  }
  const skip = isObj(status.last_skip) ? status.last_skip : null;
  if (skip) line += ` · ${str(skip.reason)}: ${str(skip.detail)}`;
  const reclaim = isObj(status.last_reclaim) ? status.last_reclaim : null;
  if (reclaim) line += ` · ${str(reclaim.reason)}: ${num(reclaim.removed)} pool(s)`;
  return line;
}

// ---------- usage and model-control telemetry ----------

export interface UsageLine { label: string; value: string }

export function workTime(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

/** The TUI's LIFETIME USAGE rows (Total spend, Chats, Work time). */
export function usageOverview(stats: unknown): UsageLine[] {
  if (!isObj(stats)) return [];
  return [
    { label: 'Total spend', value: `$${num(stats.total_cost_usd).toFixed(2)}` },
    { label: 'Chats', value: String(num(stats.lifetime_chats)) },
    { label: 'Work time', value: workTime(num(stats.total_work_time_ms)) },
  ];
}

/** SPEND BY MODEL: one line per model the usage aggregate lists. */
export function usagePerModel(stats: unknown): UsageLine[] {
  if (!isObj(stats) || !Array.isArray(stats.per_model)) return [];
  return stats.per_model.filter(isObj).map((m) => ({
    label: str(m.model) || 'unknown model',
    value: `$${num(m.cost_usd).toFixed(2)} · ${num(m.chats)} chat${num(m.chats) === 1 ? '' : 's'} · ${num(m.input_tokens)} in / ${num(m.output_tokens)} out`,
  }));
}

export interface LiveInvocation { id: string; label: string; value: string; running: boolean }

function invocationOf(view: unknown): LiveInvocation | null {
  if (!isObj(view) || !isObj(view.record)) return null;
  const r = view.record;
  const id = str(r.id);
  if (!id) return null;
  const purpose = str(r.purpose).replace(/_/g, ' ');
  return {
    id,
    label: str(view.owner_summary) || id.slice(0, 8),
    value: `${str(r.model) || 'Model unknown'} · ${purpose} · ${str(r.status).replace(/_/g, ' ')}`,
    running: r.status === 'running',
  };
}

/** LIVE MODEL WORK; only a Running invocation can be cancelled (the TUI row action). */
export function liveInvocations(status: unknown): LiveInvocation[] {
  if (!isObj(status) || !Array.isArray(status.active_invocations)) return [];
  const seen = new Set<string>();
  const out: LiveInvocation[] = [];
  for (const v of status.active_invocations) {
    const inv = invocationOf(v);
    if (inv && !seen.has(inv.id)) { seen.add(inv.id); out.push(inv); }
  }
  return out;
}

/** BLOCKED REQUESTS: the recent denials with the daemon's reason. */
export function deniedInvocations(status: unknown): UsageLine[] {
  if (!isObj(status) || !Array.isArray(status.recent_denials)) return [];
  return status.recent_denials.filter(isObj).map((v) => ({
    label: str(v.owner_summary) || 'denied request',
    value: str(v.denial_reason) || 'denied',
  }));
}

/** WARNINGS & LIMITS: a budget approaching its limit. */
export function budgetAlerts(status: unknown): UsageLine[] {
  if (!isObj(status) || !Array.isArray(status.recent_budget_alerts)) return [];
  return status.recent_budget_alerts.filter(isObj).map((a) => ({
    label: `${str(a.scope_kind)}${a.scope_id ? `/${str(a.scope_id)}` : ''}`,
    value: `${str(a.metric).replace(/_/g, ' ')}: ${String(a.remaining)} left of ${String(a.limit)}`,
  }));
}

/** The one-line value the Usage row shows once loaded. */
export function usageRowValue(stats: unknown, status: unknown): string {
  const o = usageOverview(stats);
  const spend = o.find((l) => l.label === 'Total spend')?.value;
  const chatCount = o.find((l) => l.label === 'Chats')?.value;
  const chats = chatCount === undefined ? undefined : `${chatCount} chat${chatCount === '1' ? '' : 's'}`;
  const live = liveInvocations(status).length;
  const parts = [spend && chats ? `${spend} · ${chats}` : 'usage unknown', allowedWork(modeOfStatus(status)), `${live} live`];
  return parts.join(' · ');
}

/** The toast after `CancelModelInvocation` (daemon_config.rs `cancel_model_invocation`). */
export function cancelMessage(report: unknown): string {
  if (!isObj(report)) return 'Failed to cancel model invocation: the daemon sent no report';
  const id = str(report.invocation_id).slice(0, 8);
  return report.cancelled === true
    ? `Invocation ${id} cancelled via ${str(report.mechanism)}`
    : `Invocation ${id} unchanged: ${str(report.message)}`;
}

export const CANCEL_CONFIRM = 'Cancel this live model invocation? The work it is doing stops.';
