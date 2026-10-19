// Command Mode rows (hub #3000 gap sweep G3): pure rules, no DOM. The argument
// parsers, confirm gate and status lines follow the TUI's
// crates/rsi/src/{commands.rs,cold_storage.rs,app/session_actions.rs}; every
// request is checked again by src-tauri/src/commandmode.rs and by the daemon.
// Tested in tests/commandmode.test.mjs.

type Json = Record<string, unknown>;

/** `SessionProvider` serde names (the TUI's parse_abandon_target list). */
export const PROVIDER_NAMES = [
  'Claude', 'Codex', 'Pioneer', 'OpenRouter', 'Bedrock', 'Local', 'Antigravity', 'CodexAppServer', 'Harness',
] as const;

export interface LaunchTarget { provider: string | null; model: string | null }

/** `provider[/model]`, the provider matched case-insensitively; none keeps the holder's launch. */
export function parseAbandonTarget(arg: string | undefined): LaunchTarget | { error: string } {
  const t = (arg ?? '').trim();
  if (!t) return { provider: null, model: null };
  const slash = t.indexOf('/');
  const name = (slash < 0 ? t : t.slice(0, slash)).trim();
  const model = slash < 0 ? '' : t.slice(slash + 1).trim();
  const provider = PROVIDER_NAMES.find((p) => p.toLowerCase() === name.toLowerCase());
  if (!provider) return { error: `Unknown provider \`${name}\`; use one of ${PROVIDER_NAMES.join(', ')}` };
  return { provider, model: model || null };
}

export const RESEAT_USAGE = 'Usage: :manager-reseat <provider>[/<model>] <reason>';

/** `<provider>[/<model>] <reason>`: both are required (the reason is the audit record). */
export function parseReseatArgs(arg: string | undefined): { provider: string; model: string | null; reason: string } | { error: string } {
  const t = (arg ?? '').trim();
  const m = /^(\S+)\s+([\s\S]+)$/.exec(t);
  if (!m) return { error: RESEAT_USAGE };
  const reason = m[2]!.trim();
  if (!reason) return { error: RESEAT_USAGE };
  const target = parseAbandonTarget(m[1]);
  if ('error' in target) return target;
  if (!target.provider) return { error: RESEAT_USAGE };
  return { provider: target.provider, model: target.model, reason };
}

/** The TUI's "run the same command again within 10 seconds" confirm. */
export const CONFIRM_WINDOW_MS = 10_000;

export interface Armed { key: string; at: number }

/** Whether this press confirms the armed one; `next` is the gate to keep. */
export function secondPress(armed: Armed | null, key: string, now: number, windowMs = CONFIRM_WINDOW_MS): { confirmed: boolean; next: Armed | null } {
  const confirmed = !!armed && armed.key === key && now - armed.at < windowMs;
  return { confirmed, next: confirmed ? null : { key, at: now } };
}

export const ABANDON_NOTICE = 'Abandon moves this blocked rotation’s sandbox and seat to a fresh replacement session. Run the same :rotation-abandon again within 10 seconds to confirm.';
export const RESEAT_NOTICE = 'Reseat launches a new manager on another provider and moves this project’s appointment to it. Run the same :manager-reseat again within 10 seconds to confirm.';

export function abandonMessage(r: Json): string {
  return `Rotation ${String(r.rotation_id ?? '?')} abandoned: the replacement rotates from ${String(r.holder_id ?? '?')}`;
}

export function reseatMessage(r: Json): string {
  return `Manager reseated: ${String(r.successor_session_id ?? '?')} replaces ${String(r.predecessor_session_id ?? '?')}`;
}

// ---------- cold storage ----------

export type ColdCommand = { kind: 'status' } | { kind: 'run'; mode: 'stage' | 'move' | 'verify' | 'compact' } | { error: string };

/** `:cold-archive [status|run|move|verify|compact]` (`run` and `stage` start a stage pass). */
export function parseColdArchive(arg: string | undefined): ColdCommand {
  const t = (arg ?? '').trim();
  switch (t) {
    case '': case 'status': return { kind: 'status' };
    case 'run': case 'stage': return { kind: 'run', mode: 'stage' };
    case 'move': case 'verify': case 'compact': return { kind: 'run', mode: t };
    default: return { error: `Unknown cold-archive action \`${t}\`; use :cold-archive [status|run|move|verify|compact]` };
  }
}

/** `1.5 GiB`, `612.4 MiB`, `3.0 KiB` or `12 B`. */
export function bytesText(bytes: number): string {
  const b = Math.max(0, bytes);
  const K = 1024;
  if (b >= K ** 3) return `${(b / K ** 3).toFixed(1)} GiB`;
  if (b >= K ** 2) return `${(b / K ** 2).toFixed(1)} MiB`;
  if (b >= K) return `${(b / K).toFixed(1)} KiB`;
  return `${b} B`;
}

const countText = (n: number, noun: string): string => `${n} ${noun}${n === 1 ? '' : 's'}`;
const PROBLEM_MAX_CHARS = 160;

function clipped(text: string, max = PROBLEM_MAX_CHARS): string {
  const flat = text.split(/\s+/).filter(Boolean).join(' ');
  const chars = [...flat];
  return chars.length <= max ? flat : `${chars.slice(0, max - 1).join('')}…`;
}

const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);
const obj = (v: unknown): Json => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Json) : {});
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);

function openProblems(s: Json): string | null {
  const errors = list(s.errors).map(String);
  const totals = obj(s.totals);
  const parts: string[] = [];
  if (errors.length) parts.push(countText(errors.length, 'open problem'));
  if (num(totals.sessions_with_error) > 0) parts.push(`${countText(num(totals.sessions_with_error), 'session')} with archive errors`);
  if (errors[0]) parts.push(`problem: ${clipped(errors[0])}`);
  return parts.length ? parts.join(' · ') : null;
}

/** One line: the live database, the archive files, staged / verified / cold, and the pass state (cold_storage.rs status_line). */
export function coldStatusLine(s: Json): string {
  const live = obj(s.live_db);
  const files = list(s.archives).map(obj);
  const totals = obj(s.totals);
  const compaction = obj(s.compaction);
  const parts = [
    `archive ${obj(s.settings).enabled ? 'on' : 'off'}`,
    `live DB ${typeof live.file_bytes === 'number' ? bytesText(live.file_bytes) : 'unknown'}`,
    `${countText(files.length, 'file')} ${bytesText(files.reduce((a, f) => a + num(f.bytes), 0))}`,
    `${num(totals.staged_sessions)} staged (${num(totals.verified_sessions)} verified)`,
    `${num(totals.cold_sessions)} cold`,
  ];
  if (num(totals.restored_sessions) > 0) parts.push(`${num(totals.restored_sessions)} restored`);
  if (compaction.pending) parts.push('compaction at next restart');
  else if (compaction.due) parts.push('compaction due');
  parts.push(s.running ? 'running' : 'idle');
  const problems = openProblems(s);
  if (problems) parts.unshift(problems);
  return parts.join(' · ');
}

/** The newest pass, one line (cold_storage.rs last_run_line). */
export function coldLastRunLine(s: Json): string {
  const run = s.last_run && typeof s.last_run === 'object' ? (s.last_run as Json) : null;
  const errors = list(s.errors).map(String);
  if (!run) return errors[0] ? `no pass yet · problem: ${clipped(errors[0])}` : 'no pass has run yet';
  const parts = [`${String(run.mode)} (${String(run.trigger)})`, String(run.outcome), String(run.started_at)];
  if (run.mode === 'stage' || run.mode === 'move') {
    parts.push(`${num(run.sessions_staged)} staged`);
    if (run.mode === 'move') parts.push(`${num(run.sessions_moved)} moved`);
    if (num(run.raw_bytes) > 0) parts.push(`${bytesText(num(run.raw_bytes))} → ${bytesText(num(run.compressed_bytes))}`);
  }
  if (typeof run.error_code === 'string') {
    parts.push(`problem: ${run.error_code}${typeof run.error_detail === 'string' ? `: ${clipped(run.error_detail)}` : ''}`);
  }
  let line = parts.join(' · ');
  const problems = openProblems(s);
  if (problems) line += ` · ${problems}`;
  return line;
}

export const coldHasProblems = (s: Json): boolean => openProblems(s) !== null;

/** The toast of a finished restore (cold_storage.rs restore_message). */
export function restoreMessage(r: Json): string {
  if (r.outcome === 'restored') {
    const n = num(r.events);
    return `Restored from cold storage: ${n} event${n === 1 ? '' : 's'} back in the live database`;
  }
  if (r.outcome === 'already_hot') return 'Already in the live database: nothing to restore';
  return `Cold-storage restore: ${String(r.outcome)}`;
}

/** The toast after `RunColdArchiveNow` (cold_storage.rs run_pass). */
export function coldStartedMessage(r: Json): string {
  const short = String(r.run_id ?? '').slice(0, 8);
  return r.scheduled
    ? `Cold-storage ${String(r.mode)} scheduled for the next supervised restart (run ${short})`
    : `Cold-storage ${String(r.mode)} pass started (run ${short})`;
}

/** `move` deletes verified rows from the live database: the desktop asks first. */
export const COLD_MOVE_CONFIRM = 'Move deletes the verified rows from the live database (only while a fresh rsi.db backup exists) and reclaims their pages. The archive keeps every transcript whole.';

// ---------- :context, :blank ----------

export const ACTIVE_TASK_MAX = 500;

/** `:context <text>` sets the active task (500 bytes at most); bare `:context` clears it. */
export function contextText(arg: string | undefined): string | null {
  const t = (arg ?? '').trim();
  if (!t) return null;
  let out = '';
  let bytes = 0;
  for (const ch of t) {
    const n = new TextEncoder().encode(ch).length;
    if (bytes + n > ACTIVE_TASK_MAX) break;
    out += ch;
    bytes += n;
  }
  return out;
}

export const contextMessage = (t: string | null): string => (t === null ? 'Active task cleared' : `Active task set: ${t}`);

export interface LaunchDefaults { provider: string; model: string }

/** `:blank <text>` launches directly: the default provider and model, in the filtered project's directory. */
export function blankLaunch(text: string, defaults: LaunchDefaults | null, project: { id: string; path?: string | null } | null): Json {
  const req: Json = { query: text, provider: defaults?.provider || 'Claude' };
  if (defaults?.model) req.model = defaults.model;
  if (project) {
    req.project_id = project.id;
    if (project.path) req.working_dir = project.path;
  }
  return req;
}

// ---------- window and tab commands ----------

export type WindowCommand = 'split' | 'vsplit' | 'only' | 'tabnew' | 'tabclose';

/** What each modalkit window or tab command does in the desktop's one fixed window. */
export const WINDOW_SUBSTITUTES: Readonly<Record<WindowCommand, string>> = {
  split: 'The desktop window is already split: the session list beside the open session. :split moves the keys to the other pane.',
  vsplit: 'The desktop window is already split: the session list beside the open session. :vsplit moves the keys to the other pane.',
  only: 'Closes the open overlay pane (terminal, file viewer, graph, manager or Issues view), as :close does; the list and the session stay.',
  tabnew: 'A desktop tab is the project filter: :tabnew opens the project picker.',
  tabclose: 'A desktop tab is the project filter: :tabclose returns it to all projects.',
};
