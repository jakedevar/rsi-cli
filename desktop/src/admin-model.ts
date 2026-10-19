// Admin overlays, pure rules (slice 13a, hub #3015): scheduled jobs, provider
// keys, MCP servers and diagnostics. No DOM; tested in tests/admin.test.mjs.
// The request shapes mirror the TUI forms (crates/rsi/src/overlay/*) and the
// Tauri commands in src-tauri/src/admin.rs validate them again. No function
// here ever takes or returns a credential value: secrets go from a password
// field straight to the Tauri command.

import { REDACTED } from './settings-model.js';

// ---------- shared ----------

/** `NNs` / `NNm` / `NNh` / `NNd` (the TUI's `format_age`). */
export function formatAge(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** Move a list cursor: j/k/arrows by one, g/G (Home/End) to the ends; null for other keys. */
export function moveCursor(at: number, len: number, key: string): number | null {
  if (len <= 0) return null;
  const clamp = (i: number): number => Math.max(0, Math.min(len - 1, i));
  if (key === 'j' || key === 'ArrowDown') return clamp(at + 1);
  if (key === 'k' || key === 'ArrowUp') return clamp(at - 1);
  if (key === 'g' || key === 'Home') return 0;
  if (key === 'G' || key === 'End') return len - 1;
  return null;
}

/** Comma-separated list text (the TUI forms' `split_list`). */
export function splitList(text: string): string[] {
  return text.split(',').map((p) => p.trim()).filter((p) => p.length > 0);
}

// ---------- scheduled jobs ----------

/** The TUI form's recurrence choices: label, short unit and the `Recurrence` tag. */
export const RECURRENCES: readonly { label: string; unit: string; tag: string }[] = [
  { label: 'Once', unit: 'once', tag: 'Once' },
  { label: 'Seconds', unit: 's', tag: 'EverySeconds' },
  { label: 'Minutes', unit: 'm', tag: 'EveryMinutes' },
  { label: 'Hours', unit: 'h', tag: 'EveryHours' },
  { label: 'Days', unit: 'd', tag: 'EveryDays' },
  { label: 'Weeks', unit: 'w', tag: 'EveryWeeks' },
  { label: 'Months', unit: 'mo', tag: 'EveryMonths' },
  { label: 'Years', unit: 'y', tag: 'EveryYears' },
];

export interface Recurrence { type: string; value?: number }

export interface ScheduledJob {
  id: string;
  name: string;
  message: string;
  schedule: { recurrence: Recurrence; anchor: string };
  last_fired_at: string | null;
  next_fire_at: string;
  enabled: boolean;
  working_dir?: string | null;
  provider?: string | null;
  model?: string | null;
  project_id?: string | null;
  wake_mode?: unknown;
}

export interface JobHold { job_id: string; parent_session_id: string; running_children: string[]; held_since: string; release_at: string }

export interface JobPage { jobs: ScheduledJob[]; next_cursor: string | null; include_history: boolean }

/** `every 2 h`, `once`. */
export function recurrenceLabel(r: Recurrence): string {
  const kind = RECURRENCES.find((k) => k.tag === r.type);
  if (!kind) return r.type;
  if (kind.tag === 'Once') return 'once';
  return `every ${r.value ?? 1} ${kind.unit}`;
}

/** `on_terminal`-style wake modes are objects; the rest are strings. */
export function wakeLabel(mode: unknown): string {
  if (typeof mode === 'string') return mode;
  if (mode && typeof mode === 'object') return Object.keys(mode)[0] ?? 'fresh';
  return 'fresh';
}

/** One browser row: enabled mark, name, recurrence, next fire, last fire and a hold note. */
export function jobRow(job: ScheduledJob, holds: readonly JobHold[], now: number): { mark: string; name: string; detail: string } {
  const hold = holds.find((h) => h.job_id === job.id);
  const next = Date.parse(job.next_fire_at);
  const nextText = Number.isNaN(next) ? job.next_fire_at : next >= now ? `in ${formatAge(next - now)}` : `${formatAge(now - next)} overdue`;
  const last = job.last_fired_at ? Date.parse(job.last_fired_at) : NaN;
  const parts = [recurrenceLabel(job.schedule.recurrence), `next ${nextText}`];
  if (!Number.isNaN(last)) parts.push(`last ${formatAge(now - last)} ago`);
  const wake = wakeLabel(job.wake_mode);
  if (wake !== 'fresh') parts.push(`wake:${wake}`);
  if (hold) parts.push(`held for ${hold.running_children.length} running child${hold.running_children.length === 1 ? '' : 'ren'}`);
  return { mark: job.enabled ? '●' : '○', name: job.name, detail: parts.join('  ') };
}

export interface ScheduleFormState {
  name: string;
  message: string;
  recurrence: number;
  interval: string;
  date: string;
  time: string;
  /** Create only (the TUI uses its cwd, selected provider and model). Empty: the daemon default. */
  workingDir: string;
  provider: string;
  model: string;
  editingId: string | null;
}

const pad = (n: number): string => String(n).padStart(2, '0');

export function newScheduleForm(now: Date): ScheduleFormState {
  const date = `${now.getUTCFullYear()}-${pad(now.getUTCMonth() + 1)}-${pad(now.getUTCDate())}`;
  return { name: '', message: '', recurrence: 0, interval: '1', date, time: '00:00:00', workingDir: '', provider: '', model: '', editingId: null };
}

export function editScheduleForm(job: ScheduledJob): ScheduleFormState {
  const idx = RECURRENCES.findIndex((k) => k.tag === job.schedule.recurrence.type);
  const anchor = job.schedule.anchor;
  return {
    name: job.name,
    message: job.message,
    recurrence: Math.max(0, idx),
    interval: String(job.schedule.recurrence.value ?? 1),
    date: anchor.slice(0, 10),
    time: anchor.slice(11, 19),
    workingDir: job.working_dir ?? '',
    provider: job.provider ?? '',
    model: job.model ?? '',
    editingId: job.id,
  };
}

/** Left / Right on the recurrence field. */
export function cycleRecurrence(index: number, dir: 1 | -1): number {
  return (index + dir + RECURRENCES.length) % RECURRENCES.length;
}

/** What `create_scheduled_job` / `update_scheduled_job` take (src-tauri/src/admin.rs `ScheduleForm`). */
export interface ScheduleWire {
  name: string;
  message: string;
  recurrence: string;
  interval: number | null;
  anchor: string;
  working_dir?: string;
  provider?: string;
  model?: string;
}

function validDate(text: string): boolean {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(text);
  if (!m) return false;
  const d = new Date(Date.UTC(+m[1]!, +m[2]! - 1, +m[3]!));
  return d.getUTCFullYear() === +m[1]! && d.getUTCMonth() === +m[2]! - 1 && d.getUTCDate() === +m[3]!;
}

/** `HH:MM:SS`, `HH:MM` or empty (midnight) to `HH:MM:SS`; null when invalid. */
export function normalizeTime(text: string): string | null {
  const t = text.trim();
  if (t === '') return '00:00:00';
  const m = /^(\d{1,2}):(\d{2})(?::(\d{2}))?$/.exec(t);
  if (!m) return null;
  const [h, mi, s] = [+m[1]!, +m[2]!, +(m[3] ?? 0)];
  if (h > 23 || mi > 59 || s > 59) return null;
  return `${pad(h)}:${pad(mi)}:${pad(s)}`;
}

/** The TUI's submit checks, in its order, then the request. */
export function scheduleRequest(f: ScheduleFormState): { ok: true; wire: ScheduleWire } | { ok: false; error: string } {
  const name = f.name.trim();
  if (!name) return { ok: false, error: 'Job name is required' };
  const message = f.message.trim();
  if (!message) return { ok: false, error: 'Job message/prompt is required' };
  const kind = RECURRENCES[f.recurrence] ?? RECURRENCES[0]!;
  const n = Math.max(1, Number.parseInt(f.interval.trim(), 10) || 1);
  if (!validDate(f.date.trim())) return { ok: false, error: 'Invalid date format (use YYYY-MM-DD)' };
  const time = normalizeTime(f.time);
  if (!time) return { ok: false, error: 'Invalid time format (use HH:MM:SS or HH:MM)' };
  const wire: ScheduleWire = {
    name, message, recurrence: kind.tag, interval: kind.tag === 'Once' ? null : n, anchor: `${f.date.trim()}T${time}Z`,
  };
  if (!f.editingId) {
    if (f.workingDir.trim()) wire.working_dir = f.workingDir.trim();
    if (f.provider) wire.provider = f.provider;
    if (f.model.trim()) wire.model = f.model.trim();
  }
  return { ok: true, wire };
}

/** `dd` deletes: the first `d` arms, a second on the same job runs. */
export function deleteChord(armed: string | null, jobId: string | undefined): { armed: string | null; run: string | null } {
  if (!jobId) return { armed: null, run: null };
  if (armed === jobId) return { armed: null, run: jobId };
  return { armed: jobId, run: null };
}

// ---------- provider keys ----------

/** `ProviderCredentialSlot::ALL`, in the TUI's row order. */
export const KEY_SLOTS: readonly string[] = [
  'openrouter', 'anthropic', 'openai', 'bedrock', 'pioneer', 'inception', 'deepseek', 'groq', 'mistral', 'xai',
  'cerebras', 'perplexity', 'together', 'fireworks', 'cohere', 'sambanova', 'lepton', 'nvidia', 'siliconflow',
  'huggingface', 'cloudflare', 'anyscale', 'octoai', 'serper', 'exa',
];
/** The CLIs with a login row and a contract row (`PROVIDER_LOGIN_NAMES`, `PROVIDER_CONTRACT_NAMES`). */
export const CLI_NAMES: readonly string[] = ['claude', 'codex'];
/** The second `d` on the same slot must come within this window (TUI `CLEAR_CONFIRM_TTL_MS`). */
export const CLEAR_CONFIRM_MS = 2000;

export interface CredentialCheck { at: string; class: string; credit_remaining?: number; detail_code?: string; http_status?: number }
export interface CredentialMeta {
  slot: string; state: string; fingerprint?: string; check?: CredentialCheck; route: string; cli_exposure: string; generation?: number;
}
export interface CredentialList { env_compat: boolean; check_ttl_secs: number; credentials: CredentialMeta[] }
export interface LoginRow { provider: string; remedy?: string; login: { state: string; method: string; checked_at?: string | null; hosts?: { host: string; state: string }[] } }
export interface LoginState { providers: LoginRow[] }
export interface ContractRow {
  provider: string;
  contract: { contract_state: string; reason?: string | null; cli_version?: string | null; checked_at?: string | null; catalog?: { trust: string; reason: string } | null };
}

/** rsi-common `CODEX_CUSTODY_REASONS`. */
export const CUSTODY_REASONS: readonly string[] = ['codex_hook_untrusted', 'codex_hook_hash_mismatch', 'codex_hook_missing'];

/** `ProviderContractV1::custody_failed`: degraded for a custody reason. */
export function custodyFailed(c: ContractRow['contract']): boolean {
  return c.contract_state === 'degraded' && CUSTODY_REASONS.includes(c.reason ?? '');
}
export interface ContractState { providers: ContractRow[]; codex_refuse_untrusted_custody_hook?: boolean }

export function formatCheck(check: CredentialCheck | undefined, now: number): string {
  if (!check) return 'no check';
  return `${check.class}(${formatAge(now - Date.parse(check.at))})`;
}

/** The TUI row value: state, fingerprint, check, credit, route and CLI exposure. Never key bytes. */
export function credentialValue(m: CredentialMeta, now: number): string {
  const credit = m.check?.credit_remaining;
  return [
    m.state,
    `fp:${m.fingerprint ?? '-'}`,
    `check:${formatCheck(m.check, now)}`,
    `credit:${credit === undefined ? '-' : credit.toFixed(2)}`,
    `route:${m.route}`,
    `cli:${m.cli_exposure}`,
  ].join('  ');
}

/** Set (vault, env_compat, generator) or not (cleared, absent). */
export function credentialSet(state: string): boolean {
  return state === 'vault' || state === 'env_compat' || state === 'generator';
}

export function loginValue(row: LoginRow, now: number): string {
  const l = row.login;
  const parts = [l.state, `method:${l.method}`, l.checked_at ? `checked ${formatAge(now - Date.parse(l.checked_at))} ago` : 'never probed'];
  if (l.state === 'logged_out') parts.push(`fix: ${row.remedy ?? ''}`);
  else if (l.state === 'unknown') parts.push('launches are not refused');
  for (const h of l.hosts ?? []) parts.push(`${h.host}:${h.state}`);
  return parts.join('  ');
}

export function contractValue(row: ContractRow, refuseUntrusted: boolean, now: number): string {
  const c = row.contract;
  const parts = [c.contract_state];
  if (c.reason) parts.push(`reason:${c.reason}`);
  parts.push(c.cli_version ?? 'version unknown');
  parts.push(c.checked_at ? `checked ${formatAge(now - Date.parse(c.checked_at))} ago` : 'never checked');
  if (c.catalog) parts.push(`catalog:${c.catalog.trust} reason:${c.catalog.reason}`);
  if (custodyFailed(c)) parts.push(refuseUntrusted ? 'Codex launches are refused' : 'Codex launches run without custody');
  return parts.join('  ');
}

export type KeyRowKind = 'slot' | 'login' | 'contract';
export interface KeyRow { kind: KeyRowKind; name: string; label: string; value: string; set: boolean | null }

/** The fixed Provider Keys list: 25 slots, then a login and a contract row per CLI. */
export function keyRows(list: CredentialList | null, login: LoginState | null, contract: ContractState | null, now: number): KeyRow[] {
  const rows: KeyRow[] = KEY_SLOTS.map((slot) => {
    const m = list?.credentials.find((c) => c.slot === slot);
    return { kind: 'slot', name: slot, label: slot, value: m ? credentialValue(m, now) : 'loading…', set: m ? credentialSet(m.state) : null };
  });
  for (const p of CLI_NAMES) {
    const r = login?.providers.find((x) => x.provider === p);
    rows.push({ kind: 'login', name: p, label: `${p} login`, value: r ? loginValue(r, now) : 'loading…', set: null });
  }
  const refuse = contract?.codex_refuse_untrusted_custody_hook ?? true;
  for (const p of CLI_NAMES) {
    const r = contract?.providers.find((x) => x.provider === p);
    rows.push({ kind: 'contract', name: p, label: `${p} contract`, value: r ? contractValue(r, refuse, now) : 'loading…', set: null });
  }
  return rows;
}

/** `d` on a slot: arm, then a second `d` on the same slot within 2 s clears. */
export function clearStep(armed: { slot: string; at: number } | null, slot: string, now: number): { armed: { slot: string; at: number } | null; run: boolean } {
  if (armed && armed.slot === slot && now - armed.at <= CLEAR_CONFIRM_MS) return { armed: null, run: true };
  return { armed: { slot, at: now }, run: false };
}

/** `c` on a row: the slot check, or a login / contract probe. */
export function checkTarget(row: KeyRow | undefined): { cmd: string; args: Record<string, unknown> } | null {
  if (!row) return null;
  if (row.kind === 'slot') return { cmd: 'check_provider_credential', args: { slot: row.name } };
  if (row.kind === 'login') return { cmd: 'get_provider_login_state', args: { refresh: true } };
  return { cmd: 'get_provider_contract_state', args: { refresh: true } };
}

/** The masked echo of a write-only field: one dot per character, never the text. */
export function maskedEcho(length: number): string {
  return length === 0 ? '' : `${'•'.repeat(Math.min(length, 64))} (${length})`;
}

// ---------- MCP servers ----------

export interface McpDefinition { id: string; command: string; args: string[]; secret_env_names: string[]; working_dir: string | null; enabled: boolean }
export interface McpSummary { definition: McpDefinition; credential: { id: string; state: string; fingerprint?: string; generation?: number } }

/** The TUI's `format_server`. */
export function mcpValue(s: McpSummary): string {
  const d = s.definition;
  return [
    `enabled:${d.enabled ? 'on' : 'off'}`,
    `command:${d.command}`,
    `args:${d.args.length}`,
    `secrets:${d.secret_env_names.join(',')}`,
    `state:${s.credential.state}`,
    `fp:${s.credential.fingerprint ?? '-'}`,
    `generation:${s.credential.generation ?? 0}`,
  ].join('  ');
}

export interface McpFormState { id: string; command: string; args: string; envNames: string; workingDir: string; enabled: boolean; editing: boolean }

export function mcpForm(s?: McpSummary): McpFormState {
  if (!s) return { id: '', command: '', args: '', envNames: '', workingDir: '', enabled: false, editing: false };
  const d = s.definition;
  return { id: d.id, command: d.command, args: d.args.join(','), envNames: d.secret_env_names.join(','), workingDir: d.working_dir ?? '', enabled: d.enabled, editing: true };
}

/** `upsert_mcp_server {server}`. */
export function mcpRequest(f: McpFormState): { ok: true; server: McpDefinition } | { ok: false; error: string } {
  const id = f.id.trim();
  if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(id) || id.length > 64) return { ok: false, error: 'mcp server id must be canonical lowercase ascii' };
  const command = f.command.trim();
  if (!command.startsWith('/')) return { ok: false, error: 'mcp command must be an absolute path' };
  const workingDir = f.workingDir.trim();
  if (workingDir && !workingDir.startsWith('/')) return { ok: false, error: 'mcp working_dir must be an absolute path within bounds' };
  const args = splitList(f.args);
  // A listed definition masks credential arguments (admin.rs redact_args); never write the mask back.
  if (args.some((a) => a.includes(REDACTED))) return { ok: false, error: `an argument still holds ${REDACTED}; type its value again` };
  return {
    ok: true,
    server: { id, command, args, secret_env_names: splitList(f.envNames), working_dir: workingDir || null, enabled: f.enabled },
  };
}

// ---------- diagnostics ----------

export interface DiagMetrics {
  /** Last daemon round trip (`daemon_info`), ms. */
  roundTripMs: number | null;
  /** Median frame interval over the sample, ms. */
  frameMs: number | null;
  /** Sessions in the last list poll. */
  sessions: number | null;
  /** `connected` or the daemon error. */
  daemon: string | null;
}

export function median(values: readonly number[]): number | null {
  if (values.length === 0) return null;
  const v = [...values].sort((a, b) => a - b);
  const mid = Math.floor(v.length / 2);
  return v.length % 2 ? v[mid]! : (v[mid - 1]! + v[mid]!) / 2;
}

/** The overlay's label / value lines (an em dash for an unknown value, as the TUI). */
export function diagLines(m: DiagMetrics): [string, string][] {
  const ms = (v: number | null): string => (v === null ? '—' : `${v.toFixed(1)} ms`);
  return [
    ['daemon round trip', ms(m.roundTripMs)],
    ['frame interval', ms(m.frameMs)],
    ['sessions listed', m.sessions === null ? '—' : String(m.sessions)],
    ['daemon', m.daemon ?? '—'],
  ];
}

// ---------- satellite registry ----------

export interface SatLink {
  link_id: string; direction: string; socket_path: string; ssh_target: string | null; trust_reference: string; enabled: boolean; priority: number;
}
export interface SatObservation {
  state: string; installation_id: string | null; incarnation_id: string | null; last_observed_at: string | null;
  last_error: string | null; cached_session_count: number; stale: boolean;
}
export interface SatPeerConfig {
  peer_id: string; label: string; expected_installation_id: string | null; enabled: boolean; read_enabled: boolean; dispatch_enabled?: boolean;
}
export interface SatPeer {
  config: SatPeerConfig; row_version: number; links: SatLink[]; observation: SatObservation | null;
  dispatch_scope?: string[]; dispatch_project_scope?: string[];
}
export interface SatRegistry { revision: number; peers: SatPeer[] }
export interface SatSession { key: { peer_id: string; remote_session_id: string }; summary: { session_id: string; title: string | null; provider: string; status: string; working_dir: string | null; updated_at: string } }
export interface SatSessionPage { peer_id: string; offset: number; sessions: SatSession[]; next_offset: number | null }
export interface SatProbe { direction: string; trust_reference: string; identity: { installation_id?: string; daemon_incarnation_id?: string; [k: string]: unknown }; matches_expected: boolean | null }

export type SatTab = 'peers' | 'links' | 'sessions';
/** Tab cycles peers → links → cached sessions. */
export function nextSatTab(t: SatTab): SatTab {
  return t === 'peers' ? 'links' : t === 'links' ? 'sessions' : 'peers';
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
/** rsi-common `SATELLITE_SCOPE_PROJECT_PREFIX`. */
export const SCOPE_PROJECT_PREFIX = 'project:';
/** rsi-common `SATELLITE_SCOPE_MAX_SESSIONS` (per list). */
export const SCOPE_MAX_IDS = 64;
const NIL_UUID = '00000000-0000-0000-0000-000000000000';

/** `parse_satellite_scope_text`: a bare uuid is a session, `project:<uuid>` a project. */
export function parseScope(text: string): { ok: true; sessions: string[]; projects: string[] } | { ok: false; error: string } {
  const sessions: string[] = [];
  const projects: string[] = [];
  for (const part of splitList(text)) {
    const isProject = part.startsWith(SCOPE_PROJECT_PREFIX);
    const id = isProject ? part.slice(SCOPE_PROJECT_PREFIX.length).trim() : part;
    if (!UUID.test(id)) return { ok: false, error: `Invalid scope ${isProject ? 'project' : 'session'} ID \`${id}\`: UUID must be canonical lowercase hyphenated text` };
    const list = isProject ? projects : sessions;
    // The daemon refuses a nil or repeated id and more than SCOPE_MAX_IDS per list; refuse
    // here too, before any save starts (hub #3111).
    if (id === NIL_UUID || list.includes(id)) return { ok: false, error: `Scope ${isProject ? 'project' : 'session'} ID \`${id}\` is nil or repeated` };
    list.push(id);
    if (list.length > SCOPE_MAX_IDS) return { ok: false, error: `A scope holds at most ${SCOPE_MAX_IDS} ${isProject ? 'projects' : 'sessions'}` };
  }
  return { ok: true, sessions, projects };
}

export function formatScope(sessions: readonly string[], projects: readonly string[]): string {
  return [...sessions, ...projects.map((p) => `${SCOPE_PROJECT_PREFIX}${p}`)].join(',');
}

/** A plain id list (hub installations): project entries are refused. */
export function parseIdList(text: string): { ok: true; ids: string[] } | { ok: false; error: string } {
  const r = parseScope(text);
  if (!r.ok) return r;
  if (r.projects.length) return { ok: false, error: 'Hub installation IDs cannot be project: entries' };
  return { ok: true, ids: r.sessions };
}

export function peerRow(p: SatPeer): { label: string; value: string } {
  const c = p.config;
  const o = p.observation;
  const parts = [
    `enabled:${c.enabled ? 'on' : 'off'}`, `read:${c.read_enabled ? 'on' : 'off'}`, `dispatch:${c.dispatch_enabled ? 'on' : 'off'}`,
    `state:${o ? `${o.state}${o.stale ? ' (stale)' : ''}` : 'unobserved'}`, `links:${p.links.length}`, `cached:${o?.cached_session_count ?? 0}`,
  ];
  if (o?.last_error) parts.push(`error:${o.last_error}`);
  return { label: c.label, value: parts.join('  ') };
}

export function linkRow(l: SatLink): { label: string; value: string } {
  return {
    label: l.socket_path,
    value: [l.direction, `ssh:${l.ssh_target ?? '-'}`, `trust:${l.trust_reference || '-'}`, `enabled:${l.enabled ? 'on' : 'off'}`, `priority:${l.priority}`].join('  '),
  };
}

export function satSessionRow(s: SatSession, now: number): { label: string; value: string } {
  const u = Date.parse(s.summary.updated_at);
  return {
    label: s.summary.title || s.summary.session_id,
    value: [s.summary.provider, s.summary.status, Number.isNaN(u) ? '' : `${formatAge(now - u)} ago`].filter(Boolean).join('  '),
  };
}

export interface PeerFormState {
  peerId: string; label: string; expectedInstallationId: string; enabled: boolean; readEnabled: boolean; dispatchEnabled: boolean;
  scope: string; repairQuarantine: boolean; isNew: boolean;
}

/** `a` (a new peer with a fresh id), `e` / Enter (edit) or `R` (an explicit quarantine repair edit). */
export function peerForm(peer: SatPeer | undefined, newId: string, repair = false): PeerFormState {
  if (!peer) return { peerId: newId, label: '', expectedInstallationId: '', enabled: false, readEnabled: false, dispatchEnabled: false, scope: '', repairQuarantine: false, isNew: true };
  const c = peer.config;
  return {
    peerId: c.peer_id, label: c.label, expectedInstallationId: c.expected_installation_id ?? '', enabled: c.enabled, readEnabled: c.read_enabled,
    dispatchEnabled: !!c.dispatch_enabled, scope: formatScope(peer.dispatch_scope ?? [], peer.dispatch_project_scope ?? []), repairQuarantine: repair, isNew: false,
  };
}

export interface PeerEdit {
  expected_registry_revision: number; peer_id: string; label: string; expected_installation_id: string | null;
  enabled: boolean; read_enabled: boolean; dispatch_enabled: boolean; repair_quarantine: boolean;
}

/** The peer edit and the scope that follows it (the TUI saves both, scope at the new revision). */
export function peerRequest(f: PeerFormState, revision: number):
  { ok: true; edit: PeerEdit; scope: { sessions: string[]; projects: string[] } } | { ok: false; error: string } {
  const expected = f.expectedInstallationId.trim();
  if (expected && !UUID.test(expected)) return { ok: false, error: 'Invalid installation ID: UUID must be canonical lowercase hyphenated text' };
  const scope = parseScope(f.scope);
  if (!scope.ok) return scope;
  if (!f.label.trim()) return { ok: false, error: 'A peer needs a label' };
  return {
    ok: true,
    edit: {
      expected_registry_revision: revision, peer_id: f.peerId, label: f.label, expected_installation_id: expected || null,
      enabled: f.enabled, read_enabled: f.readEnabled, dispatch_enabled: f.dispatchEnabled, repair_quarantine: f.repairQuarantine,
    },
    scope: { sessions: scope.sessions, projects: scope.projects },
  };
}

/** `d` on a peer: flip enabled; disabling also clears reads and dispatch (the daemon refuses them on a disabled peer). */
export function togglePeerEdit(p: SatPeer, revision: number): PeerEdit {
  const enabled = !p.config.enabled;
  return {
    expected_registry_revision: revision, peer_id: p.config.peer_id, label: p.config.label,
    expected_installation_id: p.config.expected_installation_id, enabled,
    read_enabled: enabled ? p.config.read_enabled : false, dispatch_enabled: enabled ? !!p.config.dispatch_enabled : false,
    repair_quarantine: false,
  };
}

export const LINK_DIRECTIONS: readonly string[] = ['dial_home_reverse', 'direct_local_forward'];

export interface LinkFormState {
  peerId: string; linkId: string; socketPath: string; sshTarget: string; trustReference: string; direction: string; enabled: boolean; priority: string; isNew: boolean;
}

export function linkForm(peerId: string, link: SatLink | undefined, newId: string): LinkFormState {
  if (!link) return { peerId, linkId: newId, socketPath: '', sshTarget: '', trustReference: '', direction: LINK_DIRECTIONS[0]!, enabled: false, priority: '0', isNew: true };
  return {
    peerId, linkId: link.link_id, socketPath: link.socket_path, sshTarget: link.ssh_target ?? '', trustReference: link.trust_reference,
    direction: link.direction, enabled: link.enabled, priority: String(link.priority), isNew: false,
  };
}

export interface LinkEdit {
  expected_registry_revision: number; peer_id: string; link_id: string; direction: string; socket_path: string;
  ssh_target: string | null; trust_reference: string; enabled: boolean; priority: number;
}

export function linkRequest(f: LinkFormState, revision: number): { ok: true; edit: LinkEdit } | { ok: false; error: string } {
  if (!f.socketPath.trim()) return { ok: false, error: 'A link needs a socket path' };
  const priority = Number(f.priority.trim() || '0');
  if (!Number.isInteger(priority) || priority < 0 || priority > 255) return { ok: false, error: 'Priority is 0 to 255' };
  if (!LINK_DIRECTIONS.includes(f.direction)) return { ok: false, error: 'Unknown link direction' };
  return {
    ok: true,
    edit: {
      expected_registry_revision: revision, peer_id: f.peerId, link_id: f.linkId, direction: f.direction, socket_path: f.socketPath,
      ssh_target: f.sshTarget.trim() || null, trust_reference: f.trustReference, enabled: f.enabled, priority,
    },
  };
}

/** `d` on a link: flip enabled. */
export function toggleLinkEdit(peerId: string, l: SatLink, revision: number): LinkEdit {
  return {
    expected_registry_revision: revision, peer_id: peerId, link_id: l.link_id, direction: l.direction, socket_path: l.socket_path,
    ssh_target: l.ssh_target, trust_reference: l.trust_reference, enabled: !l.enabled, priority: l.priority,
  };
}

// ---------- remote settings ----------

export interface RemotePeer { id: string; name: string; os: string; online: boolean; allowed: boolean }
export interface RemoteProject { id: string; name: string; exposed: boolean }
export interface RemoteStatus {
  enabled: boolean; canonical_host: string; url: string | null; unit_installed: boolean; gateway_running: boolean;
  serve_route_present: boolean; funnel_off: boolean; serve_pending_command: string | null;
  tailscale: { reachable: boolean; backend_state: string; version: string; pinned_version: string; version_qualified: boolean };
  peers: RemotePeer[]; projects: RemoteProject[]; allow_answers?: boolean; allow_messages?: boolean; errors: string[];
}
/** rsi-common `MAX_REMOTE_PROJECTS`. */
export const MAX_REMOTE_PROJECTS = 32;

/** Space / Enter on a device: allow or remove it (the whole allowed list is sent). */
export function remoteDeviceEdit(s: RemoteStatus, index: number): { allowed_node_ids: string[] } | null {
  const peer = s.peers[index];
  if (!peer) return null;
  const allowed = s.peers.filter((p) => p.allowed).map((p) => p.id);
  return { allowed_node_ids: peer.allowed ? allowed.filter((id) => id !== peer.id) : [...allowed, peer.id] };
}

/** Space / Enter on a project: expose or hide it (at most 32 exposed). */
export function remoteProjectEdit(s: RemoteStatus, index: number): { ok: true; edit: { project_ids: string[] } } | { ok: false; error: string } | null {
  const project = s.projects[index];
  if (!project) return null;
  const exposed = s.projects.filter((p) => p.exposed).map((p) => p.id);
  if (!project.exposed && exposed.length >= MAX_REMOTE_PROJECTS) return { ok: false, error: `At most ${MAX_REMOTE_PROJECTS} projects can be exposed` };
  return { ok: true, edit: { project_ids: project.exposed ? exposed.filter((id) => id !== project.id) : [...exposed, project.id] } };
}

/** The status block above the lists. */
export function remoteSummary(s: RemoteStatus): [string, string][] {
  const onOff = (v: boolean | undefined): string => (v ? 'on' : 'off');
  const ts = s.tailscale;
  const rows: [string, string][] = [
    ['Remote', s.enabled ? 'enabled' : 'disabled'],
    ['URL', s.url ?? (s.canonical_host || '—')],
    ['gateway', `${s.unit_installed ? 'installed' : 'not installed'}, ${s.gateway_running ? 'running' : 'stopped'}`],
    ['serve route', s.serve_route_present ? 'present' : 'absent'],
    ['funnel', s.funnel_off ? 'off' : 'ON (RSI never enables it)'],
    ['tailscale', ts.reachable ? `${ts.backend_state} ${ts.version}${ts.version_qualified ? '' : ` (qualified: ${ts.pinned_version})`}` : 'unreachable'],
    ['answers from phone', onOff(s.allow_answers)],
    ['messages from phone', onOff(s.allow_messages)],
  ];
  if (s.serve_pending_command) rows.push(['run once', s.serve_pending_command]);
  for (const e of s.errors) rows.push(['problem', e]);
  return rows;
}

// ---------- legacy scratch adoption ----------

export interface ScratchCandidate { path: string; kind: string; blocker: string | null; bytes: number; detail?: string | null }
export interface ScratchList { candidates: ScratchCandidate[]; refused_roots?: number; budget_exhausted?: boolean }
export interface AdoptResult { path: string; adopted: boolean; refusal?: string | null; detail?: string | null }
/** rsi-common `MAX_ADOPT_PATHS`. */
export const MAX_ADOPT_PATHS = 32;

export function formatBytes(n: number): string {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i += 1; }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${units[i]}`;
}

/** `adoptable now`, or the refusal in words (`dirty_worktree` → `dirty worktree`). */
export function scratchVerdict(c: ScratchCandidate): string {
  return c.blocker ? c.blocker.replace(/_/g, ' ') : 'adoptable now';
}

/** `a` / Enter: the one selected directory, or why it cannot be adopted. */
export function adoptOne(c: ScratchCandidate | undefined): { ok: true; paths: string[] } | { ok: false; error: string } {
  if (!c) return { ok: false, error: 'Nothing selected' };
  if (c.blocker) return { ok: false, error: `Not adoptable: ${scratchVerdict(c)}${c.detail ? ` (${c.detail})` : ''}` };
  return { ok: true, paths: [c.path] };
}

/** `A`: every directory adoptable now, at most 32. */
export function adoptAll(list: readonly ScratchCandidate[]): { ok: true; paths: string[] } | { ok: false; error: string } {
  const paths = list.filter((c) => !c.blocker).map((c) => c.path).slice(0, MAX_ADOPT_PATHS);
  return paths.length ? { ok: true, paths } : { ok: false, error: 'No directory is adoptable now' };
}

export function adoptSummary(results: readonly AdoptResult[]): string {
  const done = results.filter((r) => r.adopted).length;
  const refused = results.filter((r) => !r.adopted).map((r) => `${r.path}: ${(r.refusal ?? 'refused').replace(/_/g, ' ')}`);
  return [`Adopted ${done} of ${results.length}`, ...refused].join('; ');
}

// ---------- source-worktree settlement ----------

export interface Cohort { repository_identity: string; canonical_repo_dir: string; live_roots: number; terminal_roots: number }
export interface SettlementCounts { observed: number; eligible: number; retained: number; settled: number; refused: number; recovery_required: number; unattempted: number }
export interface CohortAudit {
  repository_identity: string; canonical_repo_dir: string; target_ref: string | null; target_oid: string | null; plan_digest: string;
  authorization_phrase: string | null; applyable: boolean; counts: SettlementCounts; items: unknown[]; run_id?: string | null; refusal?: string | null;
}
export interface SettlementRunSummary { run_id: string; repository_identity: string; state: string; counts: SettlementCounts; created_at: string }

export function countsLine(c: SettlementCounts): string {
  return `observed ${c.observed} · eligible ${c.eligible} · retained ${c.retained} · settled ${c.settled} · refused ${c.refused} · recovery ${c.recovery_required} · unattempted ${c.unattempted}`;
}

/** Why `A` cannot begin authorization (the TUI's `apply_unavailable_reason`), or null. */
export function applyBlock(audit: CohortAudit | null, selectedIdentity: string | undefined): string | null {
  if (audit && selectedIdentity === audit.repository_identity) {
    if (audit.applyable && audit.authorization_phrase) return null;
    if (audit.refusal) return `Apply refused by audit: ${audit.refusal}`;
    if (!audit.applyable) return 'Apply refused by audit: no eligible worktree roots to settle';
  }
  return 'Run a fresh applyable audit before Apply';
}

/** The retry key for one authorization (the TUI uses `tui-<uuid>`). */
export function settlementKey(uuid: string): string {
  return `desktop-${uuid}`;
}

/** Enter in authorization: apply only when the typed phrase matches the audited phrase exactly. */
export function settlementApply(audit: CohortAudit, typed: string, key: string):
  { ok: true; apply: { repository_identity: string; plan_digest: string; authorization: string; idempotency_key: string } } | { ok: false; error: string } {
  if (!audit.authorization_phrase || typed !== audit.authorization_phrase) return { ok: false, error: 'Authorization phrase does not match exactly' };
  return { ok: true, apply: { repository_identity: audit.repository_identity, plan_digest: audit.plan_digest, authorization: typed, idempotency_key: key } };
}

/** J / K and PageDown / PageUp move a page (10 rows); the rest as `moveCursor`. */
export function pageCursor(at: number, len: number, key: string): number | null {
  if (len <= 0) return null;
  if (key === 'J' || key === 'PageDown') return Math.min(len - 1, at + 10);
  if (key === 'K' || key === 'PageUp') return Math.max(0, at - 10);
  return moveCursor(at, len, key);
}
