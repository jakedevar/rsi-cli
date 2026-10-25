// Global Dashboard (hub #3366, slice D of #3314): the pure rules behind the
// desktop's `gd` / `:dashboard` view, ported from the TUI view
// (crates/rsi/src/overlay/global_dashboard.rs). The numbers come from one
// bounded operator-only read, `GetGlobalDashboard {window}` (rsi-common
// global_dashboard.rs); nothing here computes a metric the daemon did not send.
// No DOM: dashboard.ts renders these rows and routes the keys. Every line that names a Vim key
// takes `vimOn` (local #230): with the switch on it is the TUI's wording, with it off it names the
// button instead (nextStep, key-hints.ts), because the letters do nothing then.
import { nextStep } from './key-hints.js';

/** `DashboardWindow` in its serde spelling. */
export type DashboardWindow = '1h' | '24h' | '7d';
export const WINDOWS: readonly DashboardWindow[] = ['1h', '24h', '7d'];

export interface DashboardUsage {
  invocations: number; errors: number; input_tokens: number; output_tokens: number;
  cache_read_tokens: number; cache_creation_tokens: number; reasoning_tokens: number; estimated_cost_usd: number;
}
export interface DashboardModelUsage { provider: string; model: string; usage: DashboardUsage }
export interface DashboardAgentGroup { provider: string; model: string; effort?: string | null; count: number }
export interface DashboardAgents {
  total: number; managers: number; leads: number; workers: number; reviewers: number; other: number;
  by_provider: Array<{ provider: string; count: number }>;
  by_model: DashboardAgentGroup[];
}
export interface DashboardIssues {
  open: number; in_progress: number; created_in_window: number; closed_in_window: number; open_operator_requests: number;
}
export interface DashboardLandings { published: number; refused: number }
export interface DashboardRollingHealth { qa_green_sha?: string | null; qa_green_age_secs?: number | null; open_qa_issues: number }
export interface ProjectDashboard {
  id: string; name: string; path?: string | null; is_home: boolean;
  agents: DashboardAgents; pm_seat?: { session_id: string; status: string } | null;
  usage: DashboardUsage; usage_by_model: DashboardModelUsage[];
  issues: DashboardIssues; landings: DashboardLandings; rolling?: DashboardRollingHealth | null;
}
export interface DashboardTotals {
  agents: DashboardAgents; usage: DashboardUsage; usage_by_model: DashboardModelUsage[];
  issues: DashboardIssues; landings: DashboardLandings; incomplete: boolean;
}
export interface GlobalDashboard {
  as_of: string; window: string; window_seconds: number;
  projects: ProjectDashboard[]; totals: DashboardTotals;
  projects_incomplete: boolean; incomplete_reasons: string[];
}

/** What orders the project rows; the home project always leads (the TUI's `DashboardSort`). */
export type DashboardSort = 'agents' | 'tokens' | 'cost' | 'issues';
export const SORTS: readonly DashboardSort[] = ['agents', 'tokens', 'cost', 'issues'];
export const SORT_LABELS: Readonly<Record<DashboardSort, string>> = { agents: 'agents', tokens: 'tokens', cost: 'cost', issues: 'Issues' };

/** The next value in a cycle (`w` windows: 1h, 24h, 7d; `s` sorts). */
export function cycle<T>(all: readonly T[], at: T): T {
  return all[(all.indexOf(at) + 1) % all.length]!;
}

/** Shown in the pane when no top-level manager is live (the TUI's START_HINT), Vim keys on. */
export const START_HINT = 'press n to start one';
/** The pane's start button (the key shows as "(n)" with the switch on). */
export const START_LABEL = 'Start a top-level manager…';
/** The pane's type button. */
export const TYPE_LABEL = 'Type to it…';

const lowerFirst = (s: string): string => s.charAt(0).toLowerCase() + s.slice(1);

/** "press n to start one" with Vim keys on; "click Start a top-level manager…" with them off. */
export function startHint(vimOn = true): string {
  return vimOn ? START_HINT : lowerFirst(nextStep({ click: START_LABEL }, false));
}

/** "press r to retry" with Vim keys on; "click Refresh to retry" with them off. */
export function retryHint(vimOn = true): string {
  return vimOn ? 'press r to retry' : lowerFirst(nextStep({ click: 'Refresh', to: 'retry' }, false));
}

/** A failed refresh, keeping the last snapshot when there is one. */
export function refreshFailedText(reason: string, hasSnapshot: boolean, vimOn = true): string {
  return `Refresh failed: ${reason}${hasSnapshot ? ' (showing the last snapshot)' : ''}; ${retryHint(vimOn)}`;
}

/** The table area before the first snapshot could be read. */
export function unreadableText(vimOn = true): string {
  return vimOn ? 'The dashboard could not be read yet. Press r to retry.' : `The dashboard could not be read yet. ${nextStep({ click: 'Refresh', to: 'retry' }, false)}.`;
}

/** The project the row names is not in the project filter (yet). */
export function notListedText(name: string, vimOn = true): string {
  return `${name} is not in the project list yet; ${vimOn ? 'press r' : 'click Refresh to check again'}`;
}

/** Opening the All projects row. */
export const NOT_A_PROJECT = 'All projects adds up every project; select one project row to open it';

/** The notice when the conversation log takes the keys. */
export function conversationNotice(vimOn = true): string {
  return vimOn
    ? 'Conversation: j/k, Ctrl-D/U scroll, g/G ends; Tab or Esc returns to the projects'
    : 'Conversation: Up/Down and PageUp/PageDown scroll; F6 or Esc returns to the projects';
}

/** What a notice says when no top-level manager is live (or still being read). */
export function noManagerNotice(seat: TopSeat, vimOn = true): string {
  return seat.kind === 'unknown' ? 'Still reading the top-level manager' : `No top-level manager is live; ${startHint(vimOn)}`;
}

/** Starting while one is already live. */
export function alreadyLiveNotice(vimOn = true): string {
  return vimOn
    ? 'A top-level manager is already live; press i to type to it, or gm to replace it'
    : `A top-level manager is already live; click ${TYPE_LABEL} to message it, or Manager workspace to replace it`;
}
/** While open, re-read the snapshot at most this often (the TUI's AUTO_REFRESH). */
export const AUTO_REFRESH_MS = 10_000;

// ---------- formatting (the TUI's fmt_* helpers) ----------

/** Compact count: `812`, `1.2k`, `340k`, `1.2M`, `12M`, `1.2B`. */
export function fmtCount(value: number): string {
  const scaled = (unit: number, suffix: string): string => {
    const n = value / unit;
    return n < 10 ? `${n.toFixed(1)}${suffix}` : `${n.toFixed(0)}${suffix}`;
  };
  if (value <= 999) return String(value);
  if (value <= 999_499) return scaled(1e3, 'k');
  if (value <= 999_499_999) return scaled(1e6, 'M');
  return scaled(1e9, 'B');
}

/** Dollars: `$0`, `$3.20`, `$123`, `$1.2k`. */
export function fmtCost(usd: number): string {
  if (!(usd > 0)) return '$0';
  if (usd < 10) return `$${usd.toFixed(2)}`;
  if (usd < 1000) return `$${usd.toFixed(0)}`;
  return `$${(usd / 1000).toFixed(1)}k`;
}

/** `45s`, `12m`, `3h`, `2d`. */
export function fmtAge(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** The top providers, `Claude 4 · Codex 2`, busiest first; `+n` past `limit`. */
export function providersText(agents: DashboardAgents, limit: number): string {
  const ps = agents.by_provider.filter((p) => p.count > 0)
    .sort((a, b) => b.count - a.count || (a.provider < b.provider ? -1 : a.provider > b.provider ? 1 : 0));
  if (!ps.length) return '-';
  const extra = ps.length - limit;
  const text = ps.slice(0, limit).map((p) => `${p.provider} ${p.count}`).join(' · ');
  return extra > 0 ? `${text} +${extra}` : text;
}

/** The project manager glyph and its words (the glyph alone is never the only cue). */
export function pmSeat(status: string | null | undefined): { glyph: string; label: string } {
  switch (status) {
    case null: case undefined: return { glyph: '○', label: 'no project manager' };
    case 'Starting': case 'Running': return { glyph: '●', label: `Project manager ${status}` };
    case 'WaitingApproval': return { glyph: '!', label: 'Project manager waiting for you' };
    case 'Failed': case 'Interrupted': return { glyph: '×', label: `Project manager ${status}` };
    default: return { glyph: '✓', label: `Project manager ${status}` };
  }
}

// ---------- rows ----------

/** One table column: a short header (the TUI's), a plain-words title, alignment. */
export interface DashboardColumn {
  key: ColumnKey;
  header: string;
  title: string;
  numeric: boolean;
}
export type ColumnKey =
  | 'project' | 'pm' | 'agents' | 'managers' | 'leads' | 'workers' | 'reviewers' | 'providers'
  | 'tokensIn' | 'tokensOut' | 'cost' | 'open' | 'inProgress' | 'created' | 'closed' | 'requests' | 'landings' | 'qa';

/** The TUI's columns in its order, with headers a first-time user can read. */
export function columns(window: string, narrow = false): DashboardColumn[] {
  const w = window || '24h';
  // Spelled out when the table has room; the TUI's short headers when it is narrow (local #230).
  const h = (full: string, short: string): string => (narrow ? short : full);
  return [
    { key: 'project', header: 'Project', title: 'Project (the RSI home project first)', numeric: false },
    { key: 'pm', header: h('Project manager', 'PM'), title: 'Project manager', numeric: false },
    { key: 'agents', header: 'Agents', title: 'Live agents (starting, running or waiting)', numeric: true },
    { key: 'managers', header: h('Managers', 'Mgr'), title: 'Live managers', numeric: true },
    { key: 'leads', header: h('Epic leads', 'Lead'), title: 'Live Epic leads', numeric: true },
    { key: 'workers', header: h('Workers', 'Wrk'), title: 'Live workers', numeric: true },
    { key: 'reviewers', header: h('Reviewers', 'Rev'), title: 'Live reviewers', numeric: true },
    { key: 'providers', header: 'Providers', title: 'Live agents by provider, busiest first', numeric: false },
    { key: 'tokensIn', header: 'Tokens in', title: `Input tokens in the last ${w}`, numeric: true },
    { key: 'tokensOut', header: 'Tokens out', title: `Output tokens in the last ${w}`, numeric: true },
    { key: 'cost', header: 'Cost', title: `Estimated cost in the last ${w}`, numeric: true },
    { key: 'open', header: 'Open', title: 'Issues open now', numeric: true },
    { key: 'inProgress', header: h('In progress', 'WIP'), title: 'Issues in progress now', numeric: true },
    { key: 'created', header: `New ${w}`, title: `Issues created in the last ${w}`, numeric: true },
    { key: 'closed', header: `Done ${w}`, title: `Issues closed in the last ${w}`, numeric: true },
    { key: 'requests', header: h('Requests', 'Req'), title: 'Open operator requests', numeric: true },
    { key: 'landings', header: 'Landed', title: `Landings published in the last ${w}`, numeric: true },
    { key: 'qa', header: 'QA', title: 'Rolling QA: age of the last full-library green, or open red QA Issues', numeric: false },
  ];
}

export interface DashboardCell { text: string; dim: boolean; title?: string }
export interface DashboardRow {
  /** The project id; `null` on TOTAL. */
  id: string | null;
  name: string;
  isHome: boolean;
  isTotal: boolean;
  cells: Record<ColumnKey, DashboardCell>;
}

/** The projects in display order: the home project first, then by `sort` (largest first), then by name. */
export function sortedProjects(snapshot: GlobalDashboard | null, sort: DashboardSort): ProjectDashboard[] {
  if (!snapshot) return [];
  const key = (p: ProjectDashboard): number => {
    switch (sort) {
      case 'tokens': return p.usage.input_tokens + p.usage.output_tokens;
      case 'cost': return p.usage.estimated_cost_usd;
      case 'issues': return p.issues.open + p.issues.in_progress;
      default: return p.agents.total;
    }
  };
  const lower = (s: string) => s.toLowerCase();
  return [...snapshot.projects].sort((a, b) =>
    Number(b.is_home) - Number(a.is_home)
    || key(b) - key(a)
    || (lower(a.name) < lower(b.name) ? -1 : lower(a.name) > lower(b.name) ? 1 : 0)
    || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

/** The pinned last row's name (the TOTAL row of the TUI). */
export const ALL_PROJECTS = 'All projects';

const num = (n: number): DashboardCell => ({ text: String(n), dim: n === 0 });

function rowCells(
  agents: DashboardAgents, usage: DashboardUsage, issues: DashboardIssues, landings: DashboardLandings,
  pm: DashboardCell, qa: DashboardCell,
): Omit<Record<ColumnKey, DashboardCell>, 'project'> {
  const providers = providersText(agents, 2);
  return {
    pm,
    agents: num(agents.total), managers: num(agents.managers), leads: num(agents.leads),
    workers: num(agents.workers), reviewers: num(agents.reviewers),
    providers: { text: providers, dim: providers === '-', title: providersText(agents, 99) },
    tokensIn: { text: fmtCount(usage.input_tokens), dim: usage.input_tokens === 0, title: String(usage.input_tokens) },
    tokensOut: { text: fmtCount(usage.output_tokens), dim: usage.output_tokens === 0, title: String(usage.output_tokens) },
    cost: { text: fmtCost(usage.estimated_cost_usd), dim: !(usage.estimated_cost_usd > 0) },
    open: num(issues.open), inProgress: num(issues.in_progress),
    created: num(issues.created_in_window), closed: num(issues.closed_in_window),
    requests: num(issues.open_operator_requests), landings: num(landings.published),
    qa,
  };
}

/** The QA cell: a red count when `qa` Issues are open, else the last green's age, else `-`. */
function qaCell(rolling: DashboardRollingHealth | null | undefined): DashboardCell {
  if (!rolling) return { text: '-', dim: true, title: 'No full-library QA run yet' };
  if (rolling.open_qa_issues > 0) return { text: `${rolling.open_qa_issues} red`, dim: false, title: `${rolling.open_qa_issues} open QA Issue(s)` };
  if (rolling.qa_green_age_secs != null) return { text: fmtAge(rolling.qa_green_age_secs), dim: false, title: `Last full-library green ${fmtAge(rolling.qa_green_age_secs)} ago` };
  return { text: '-', dim: true, title: 'No full-library green yet' };
}

/** The table rows: the sorted projects, then a pinned TOTAL row. Empty before the first snapshot. */
export function dashboardRows(snapshot: GlobalDashboard | null, sort: DashboardSort): DashboardRow[] {
  if (!snapshot) return [];
  const rows: DashboardRow[] = sortedProjects(snapshot, sort).map((p) => {
    const seat = pmSeat(p.pm_seat?.status);
    return {
      id: p.id, name: p.name, isHome: p.is_home, isTotal: false,
      cells: {
        project: { text: p.is_home ? `${p.name} (home)` : p.name, dim: false, title: p.path ?? p.name },
        ...rowCells(p.agents, p.usage, p.issues, p.landings, { text: seat.glyph, dim: !p.pm_seat, title: seat.label }, qaCell(p.rolling)),
      },
    };
  });
  const red = snapshot.projects.reduce((n, p) => n + (p.rolling?.open_qa_issues ?? 0), 0);
  const t = snapshot.totals;
  rows.push({
    id: null, name: ALL_PROJECTS, isHome: false, isTotal: true,
    cells: {
      project: { text: ALL_PROJECTS, dim: false, title: 'Every project; agents and usage also count sessions in no project' },
      ...rowCells(t.agents, t.usage, t.issues, t.landings, { text: '', dim: true },
        red > 0 ? { text: `${red} red`, dim: false, title: `${red} open QA Issue(s)` } : { text: '', dim: true }),
    },
  });
  return rows;
}

/** Whether the daemon reported a capped read: the sums are then a lower bound. */
export function isIncomplete(snapshot: GlobalDashboard | null): boolean {
  return !!snapshot && (snapshot.totals.incomplete || snapshot.projects_incomplete || snapshot.incomplete_reasons.length > 0);
}

/** The incomplete badge's words, or `null` for a complete snapshot. */
export function incompleteText(snapshot: GlobalDashboard | null): string | null {
  if (!isIncomplete(snapshot)) return null;
  const why = snapshot!.incomplete_reasons.length ? `: ${snapshot!.incomplete_reasons.join(', ')}` : '';
  const projects = snapshot!.projects_incomplete ? ' Not every project is listed.' : '';
  return `Incomplete: a read hit its cap, so these numbers are a lower bound${why}.${projects}`;
}

/** The snapshot age, `12s ago`. */
export function snapshotAge(asOf: string, nowMs: number): string {
  const at = Date.parse(asOf);
  return Number.isFinite(at) ? `${fmtAge((nowMs - at) / 1000)} ago` : 'age unknown';
}

/** `items` joined by ` · `, at most `limit`, with a `+n more` tail. */
function joined(items: string[], limit: number): string {
  const extra = items.length - limit;
  const text = items.slice(0, limit).join(' · ');
  return extra > 0 ? `${text} · +${extra} more` : text;
}

const plural = (n: number, word: string): string => `${n} ${word}${n === 1 ? '' : 's'}`;

/** The detail band for the selected row (the TUI's detail_lines). */
export function detailLines(snapshot: GlobalDashboard | null, sort: DashboardSort, selected: number): string[] {
  if (!snapshot) return [];
  const window = snapshot.window;
  const project = sortedProjects(snapshot, sort)[selected] ?? null;
  let name: string;
  let agents: DashboardAgents; let usage: DashboardModelUsage[]; let issues: DashboardIssues; let landings: DashboardLandings;
  let extra: string | null = null;
  if (project) {
    const seat = project.pm_seat ? `project manager ${project.pm_seat.status}` : 'no project manager';
    name = `${project.name}${project.is_home ? ' · home project' : ''} · ${seat}`;
    ({ agents, usage_by_model: usage, issues, landings } = project);
    const r = project.rolling;
    if (r) extra = r.open_qa_issues > 0 ? `QA ${r.open_qa_issues} red` : r.qa_green_age_secs != null ? `QA green ${fmtAge(r.qa_green_age_secs)} ago` : 'QA no full-library green yet';
  } else {
    name = `${ALL_PROJECTS} · every project`;
    ({ agents, usage_by_model: usage, issues, landings } = snapshot.totals);
    if (snapshot.incomplete_reasons.length) extra = `Incomplete (lower bound): ${snapshot.incomplete_reasons.join(', ')}`;
  }
  let roles = `Agents ${agents.total} live · ${plural(agents.managers, 'manager')} · ${plural(agents.leads, 'lead')} · ${plural(agents.workers, 'worker')} · ${plural(agents.reviewers, 'reviewer')}`;
  if (agents.other > 0) roles += ` · ${agents.other} other`;
  const byModel = agents.by_model.filter((g) => g.count > 0)
    .map((g) => (g.effort ? `${g.provider} ${g.model} ${g.effort} ×${g.count}` : `${g.provider} ${g.model} ×${g.count}`));
  const use = usage.map((u) => `${u.provider} ${u.model} ${fmtCount(u.usage.input_tokens)} in ${fmtCount(u.usage.output_tokens)} out ${fmtCost(u.usage.estimated_cost_usd)}`);
  const lines = [
    name,
    roles,
    byModel.length ? `By model: ${joined(byModel, 6)}` : 'By model: no live agents',
    use.length ? `Usage ${window}: ${joined(use, 4)}` : `Usage ${window}: none`,
    `Issues: ${issues.open} open · ${issues.in_progress} in progress · ${issues.created_in_window} created · ${issues.closed_in_window} closed in ${window}`
      + ` · ${issues.open_operator_requests} operator request${issues.open_operator_requests === 1 ? '' : 's'}`
      + ` · landings ${landings.published} published, ${landings.refused} refused`,
  ];
  if (extra) lines.push(extra);
  return lines;
}

/** Keep the selection on the same project (or TOTAL) across a new snapshot or sort. */
export function reselect(
  before: { snapshot: GlobalDashboard | null; sort: DashboardSort; selected: number },
  after: { snapshot: GlobalDashboard | null; sort: DashboardSort },
): number {
  const prevRows = dashboardRows(before.snapshot, before.sort);
  const prev = prevRows[before.selected];
  const rows = dashboardRows(after.snapshot, after.sort);
  if (!rows.length) return 0;
  if (prev?.isTotal) return rows.length - 1;
  if (prev?.id) {
    const at = rows.findIndex((r) => r.id === prev.id);
    if (at >= 0) return at;
  }
  return Math.min(Math.max(0, before.selected), rows.length - 1);
}

// ---------- the top-level manager seat ----------

/** The seat the pane talks to, from `GetGlobalManager`: unknown until read, absent without an active grant. */
export type TopSeat = { kind: 'unknown' } | { kind: 'absent' } | { kind: 'seat'; sessionId: string };

export function seatFromGrant(grant: { state?: string; seat_session_id?: string } | null | undefined): TopSeat {
  return grant?.state === 'active' && grant.seat_session_id ? { kind: 'seat', sessionId: grant.seat_session_id } : { kind: 'absent' };
}

/** The live seat: the grant's seat while its session is still there and not archived or deleted. */
export function liveSeat(seat: TopSeat, status: string | null): string | null {
  if (seat.kind !== 'seat' || !status) return null;
  return status === 'Archived' || status === 'Deleted' ? null : seat.sessionId;
}

/** What the pane's call-to-action offers: type to the live seat, or start one. */
export function seatAction(seat: TopSeat, status: string | null, vimOn = true): { kind: 'loading' | 'type' | 'start'; text: string } {
  if (seat.kind === 'unknown') return { kind: 'loading', text: 'Reading the top-level manager…' };
  if (liveSeat(seat, status)) return { kind: 'type', text: `Top-level manager ${seat.kind === 'seat' ? seat.sessionId.slice(0, 8) : ''} · ${status} · ${vimOn ? 'press i to type to it' : `click ${TYPE_LABEL}`}` };
  return { kind: 'start', text: `No top-level manager is live; ${startHint(vimOn)} (it launches and appoints one)` };
}

/** The host for a started seat: the home project when the snapshot names one (the TUI's start_seat). */
export function startHost(snapshot: GlobalDashboard | null): string | null {
  return snapshot?.projects.find((p) => p.is_home)?.id ?? null;
}

// ---------- keys ----------

export type DashboardKeyAction =
  | 'down' | 'up' | 'first' | 'last' | 'open' | 'window' | 'sort' | 'pane' | 'type' | 'start'
  | 'conversation' | 'refresh' | 'close' | 'workspace';

/** The table's keys (the TUI's handle_key; `g` then `m` opens the manager workspace, gm). */
export const DASHBOARD_KEYS: Readonly<Record<string, DashboardKeyAction>> = {
  j: 'down', ArrowDown: 'down', k: 'up', ArrowUp: 'up', g: 'first', Home: 'first', G: 'last', End: 'last',
  Enter: 'open', l: 'open', w: 'window', s: 'sort', t: 'pane', i: 'type', n: 'start',
  Tab: 'conversation', r: 'refresh', Escape: 'close', q: 'close',
};

/** The key hint line, in the TUI's words (Vim keys on). */
export const DASHBOARD_HINTS = 'j/k move · Enter/l open project · w window · s sort · Tab conversation · i type · n start manager · t pane · r refresh · gm workspace · Esc close';

/** The footer: the TUI's key line with Vim keys on; with them off only the keys that still work (the buttons carry the rest). */
export function dashboardHints(vimOn = true): string {
  return vimOn ? DASHBOARD_HINTS : 'Up/Down move · Enter open project · F6 conversation · Esc close';
}
