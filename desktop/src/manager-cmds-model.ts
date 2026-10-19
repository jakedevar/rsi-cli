// Manager `:` commands (hub #3000 gap sweep G3, extending the managers slice):
// pure parsers, resolvers and summaries, no DOM. They follow the TUI's
// crates/rsi/src/overlay/{global_manager_command,portfolio_command,
// escalations_command,operator_restart_command,harness_manager}.rs; the
// requests are built by the src-tauri/src/manager_ops.rs planners over the
// TUI's own types. Tested in tests/manager-cmds.test.mjs.

type Json = Record<string, unknown>;

export interface NamedProject { id: string; name: string }

const words = (s: string): string[] => s.split(/\s+/).filter(Boolean);
const short = (id: unknown): string => String(id ?? '').slice(0, 8);
const firstLine = (t: unknown): string => String(t ?? '').split('\n')[0] ?? '';

/** The daemon's refusal when a configure lowers a covered project's caps (#1544). */
export const CAP_REDUCTION = 'portfolio_cap_reduction_confirmation_required';
export const needsCapConfirm = (error: string): boolean => error.includes(CAP_REDUCTION);
/** The preview part of a cap-reduction refusal (before its "Confirm with" tail). */
export const capPreview = (error: string): string => error.split('. Confirm with')[0] ?? error;

// ---------- projects ----------

function findProject(projects: readonly NamedProject[], name: string): string | null {
  return projects.find((p) => p.name.toLowerCase() === name.toLowerCase())?.id ?? null;
}

/**
 * Project names to ids (global_manager_command.rs resolve_projects): empty is
 * every project; a comma list names projects that may contain spaces; else
 * the whole text may name one project, otherwise each word names one.
 */
export function resolveProjects(projects: readonly NamedProject[], args: string): string[] | { error: string } {
  const t = args.trim();
  if (!t) return projects.length ? projects.map((p) => p.id) : { error: 'No projects to grant.' };
  const names = t.includes(',') ? t.split(',').map((n) => n.trim()).filter(Boolean) : null;
  if (!names) {
    const whole = findProject(projects, t);
    if (whole) return [whole];
  }
  const out: string[] = [];
  for (const n of names ?? words(t)) {
    const id = findProject(projects, n);
    if (!id) return { error: `Unknown project: ${n}` };
    out.push(id);
  }
  return out;
}

const projectNames = (ids: readonly string[], projects: readonly NamedProject[]): string =>
  ids.map((id) => projects.find((p) => p.id === id)?.name ?? short(id)).join(', ');

// ---------- :manager global ----------

export const GLOBAL_USAGE = 'Use :manager global [appoint [--defaults] [project names...]|prefill [project names...]|add-project <names...>|revoke|set <field> <value> [confirm]|configure <JSON>].';
export const SET_USAGE = 'Use :manager global set <active|sessions|containers|spend|groups> <value> [confirm] (spend: USD or none; groups: on|off).';

export type GlobalCommand =
  | { verb: 'show' } | { verb: 'revoke' }
  | { verb: 'configure'; json: string }
  | { verb: 'add-project'; names: string }
  | { verb: 'set'; field: string; value: string; confirm: boolean }
  | { verb: 'appoint' | 'prefill'; names: string; useDefaults: boolean };

const SET_FIELDS = ['active', 'max_active_sessions', 'sessions', 'created', 'max_created_sessions', 'containers', 'max_created_containers', 'spend', 'max_spend_usd', 'groups', 'allow_create_groups'];

/** The text after `:manager global` (`appoint` also arrives from `:manager global appoint`). */
export function parseGlobalCommand(arg: string | undefined): GlobalCommand | { error: string } {
  const t = (arg ?? '').trim();
  if (t === '' || t === 'show') return { verb: 'show' };
  if (t === 'revoke') return { verb: 'revoke' };
  const rest = (w: string): string | null => (t === w ? '' : t.startsWith(`${w} `) ? t.slice(w.length + 1).trim() : null);
  const configure = rest('configure');
  if (configure) return { verb: 'configure', json: configure };
  const add = rest('add-project');
  if (add !== null) return add ? { verb: 'add-project', names: add } : { error: 'Use :manager global add-project <project names...>.' };
  const set = rest('set');
  if (set !== null) {
    const w = words(set);
    const confirm = w.at(-1) === 'confirm';
    if (confirm) w.pop();
    if (w.length !== 2 || !SET_FIELDS.includes(w[0]!.toLowerCase())) return { error: SET_USAGE };
    return { verb: 'set', field: w[0]!, value: w[1]!, confirm };
  }
  for (const verb of ['prefill', 'appoint'] as const) {
    const r = rest(verb);
    if (r !== null) {
      const useDefaults = words(r).includes('--defaults');
      return { verb, names: words(r).filter((x) => x !== '--defaults').join(' '), useDefaults };
    }
  }
  return { error: GLOBAL_USAGE };
}

/** The `:manager global` line (grant_summary). */
export function grantSummary(g: Json, projects: readonly NamedProject[]): string {
  const launches = (Array.isArray(g.allowed_launches) ? g.allowed_launches : []).map((l: Json) => `${String(l.provider)}/${String(l.model)}/${String(l.effort ?? 'any effort')}`);
  const policy = (g.project_policy && typeof g.project_policy === 'object' ? g.project_policy : {}) as Json;
  return `Global manager v${String(g.grant_version)} seat ${short(g.seat_session_id)} (${String(g.state)}): projects [${projectNames((g.project_ids as string[]) ?? [], projects)}]; launches [${launches.join(', ')}]; PM policy ${String(policy.mode ?? '?')}`;
}

// ---------- :manager portfolio ----------

export const PORTFOLIO_USAGE = 'Use :manager portfolio [list|show <node>|appoint <label> [--adopt <node,...>] [projects...]|configure <JSON>|launch add|remove <node> <Provider/model[/effort]>...|revoke <node>].';

export type PortfolioCommand =
  | { verb: 'list' } | { verb: 'show'; node: string } | { verb: 'revoke'; node: string }
  | { verb: 'configure'; json: string }
  | { verb: 'launch'; add: boolean; node: string; launches: string[] }
  | { verb: 'appoint'; label: string; projects: string; adopt: string[] };

const LAUNCH_USAGE = 'Use :manager portfolio launch add|remove <node> <Provider/model[/effort]>...';

export function parsePortfolioCommand(arg: string | undefined): PortfolioCommand | { error: string } {
  const t = (arg ?? '').trim();
  const sp = t.search(/\s/);
  const verb = sp < 0 ? t : t.slice(0, sp);
  const rest = sp < 0 ? '' : t.slice(sp).trim();
  switch (verb) {
    case '': case 'list': return rest ? { error: PORTFOLIO_USAGE } : { verb: 'list' };
    case 'show': return rest ? { verb: 'show', node: rest } : { error: PORTFOLIO_USAGE };
    case 'revoke': return rest ? { verb: 'revoke', node: rest } : { error: PORTFOLIO_USAGE };
    case 'configure': return rest ? { verb: 'configure', json: rest } : { error: PORTFOLIO_USAGE };
    case 'launch': {
      const w = words(rest);
      if (w[0] !== 'add' && w[0] !== 'remove') return { error: LAUNCH_USAGE };
      if (!w[1] || w.length < 3) return { error: LAUNCH_USAGE };
      return { verb: 'launch', add: w[0] === 'add', node: w[1], launches: w.slice(2) };
    }
    case 'appoint': {
      if (!rest) return { error: PORTFOLIO_USAGE };
      const lsp = rest.search(/\s/);
      const label = lsp < 0 ? rest : rest.slice(0, lsp);
      const tail = words(lsp < 0 ? '' : rest.slice(lsp));
      const projects: string[] = [];
      const adopt: string[] = [];
      for (let i = 0; i < tail.length; i++) {
        const w = tail[i]!;
        if (w === '--adopt') {
          const nodes = tail[++i];
          if (!nodes) return { error: '--adopt needs <node,...>' };
          adopt.push(...nodes.split(',').map((x) => x.trim()).filter(Boolean));
        } else if (w.startsWith('--adopt=')) {
          adopt.push(...w.slice(8).split(',').map((x) => x.trim()).filter(Boolean));
        } else {
          projects.push(w);
        }
      }
      return { verb: 'appoint', label, projects: projects.join(' '), adopt };
    }
    default: return { error: PORTFOLIO_USAGE };
  }
}

export interface PortfolioNodeLite {
  node_id: string;
  tier_label: string;
  state: string;
  authority_epoch?: number;
  grant: { grant_version: number; seat_session_id: string; project_ids: string[]; project_policy?: Json };
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** A node argument: a full id, a unique 4+ character id prefix or a unique active tier label. */
export function resolveNode(nodes: readonly PortfolioNodeLite[], arg: string): string | { error: string } {
  if (UUID.test(arg)) return arg.toLowerCase();
  const byPrefix = arg.length >= 4 ? nodes.filter((n) => n.node_id.startsWith(arg)) : [];
  const matches = byPrefix.length ? byPrefix : nodes.filter((n) => n.state === 'active' && n.tier_label === arg);
  if (matches.length === 1) return matches[0]!.node_id;
  if (!matches.length) return { error: `No portfolio node matches ${arg}` };
  return { error: `${arg} names ${matches.length} portfolio nodes; use the node id` };
}

/** `appoint` without projects: every project no active node covers (the daemon refuses an overlap). */
export function freeProjects(projects: readonly NamedProject[], nodes: readonly PortfolioNodeLite[]): string[] {
  return projects.map((p) => p.id).filter((id) => !nodes.some((n) => n.state === 'active' && n.grant.project_ids.includes(id)));
}

/** One line per node (portfolio_command.rs node_summary). */
export function portfolioSummary(n: PortfolioNodeLite, projects: readonly NamedProject[]): string {
  return `${n.tier_label} ${short(n.node_id)} (${n.state}) v${n.grant.grant_version} epoch ${n.authority_epoch ?? '?'} seat ${short(n.grant.seat_session_id)}: projects [${projectNames(n.grant.project_ids, projects)}]; PM policy ${String(n.grant.project_policy?.mode ?? '?')}`;
}

// ---------- :manager escalations ----------

export const ESCALATIONS_USAGE = 'Use :manager escalations [list|all|rule <hop> <text>|ack <notice>|undelivered [<cursor>]].';

export type EscalationsCommand =
  | { verb: 'list'; includeClosed: boolean }
  | { verb: 'rule'; hop: string; ruling: string }
  | { verb: 'ack'; notice: string }
  | { verb: 'undelivered'; after: string | null };

export function parseEscalationsCommand(arg: string | undefined): EscalationsCommand | { error: string } {
  const t = (arg ?? '').trim();
  const sp = t.search(/\s/);
  const verb = sp < 0 ? t : t.slice(0, sp);
  const rest = sp < 0 ? '' : t.slice(sp).trim();
  switch (verb) {
    case '': case 'list': return rest ? { error: ESCALATIONS_USAGE } : { verb: 'list', includeClosed: false };
    case 'all': return rest ? { error: ESCALATIONS_USAGE } : { verb: 'list', includeClosed: true };
    case 'ack': return rest && !/\s/.test(rest) ? { verb: 'ack', notice: rest } : { error: ESCALATIONS_USAGE };
    case 'undelivered': return /\s/.test(rest) ? { error: ESCALATIONS_USAGE } : { verb: 'undelivered', after: rest || null };
    case 'rule': {
      const m = /^(\S+)\s+([\s\S]+)$/.exec(rest);
      const ruling = m?.[2]?.trim();
      return m && ruling ? { verb: 'rule', hop: m[1]!, ruling } : { error: ESCALATIONS_USAGE };
    }
    default: return { error: ESCALATIONS_USAGE };
  }
}

/** A full id, or a unique prefix of 4+ characters among `ids` (escalations_command.rs resolve_id). */
export function resolveId(ids: readonly string[], arg: string, what: string): string | { error: string } {
  if (UUID.test(arg)) return arg.toLowerCase();
  if (arg.length < 4) return { error: `Name the ${what} by its id or a 4+ character prefix` };
  const matches = ids.filter((id) => id.startsWith(arg));
  if (matches.length === 1) return matches[0]!;
  if (!matches.length) return { error: `No open ${what} matches ${arg}` };
  return { error: `${arg} names ${matches.length} ${what}s; use more characters` };
}

export function hopSummary(h: Json): string {
  const ruling = typeof h.ruling === 'string' ? ` -> ruled: ${firstLine(h.ruling)}` : '';
  return `escalation ${short(h.hop_id)} (${String(h.state)}, hop ${String(h.hop)}) from ${String(h.source_ref)} in project ${short(h.project_id)}: ${firstLine(h.reason)}${ruling}`;
}

export function noticeSummary(n: Json): string {
  return `report ${short(n.message_id)} (${n.state === 'queued' ? 'unread' : 'read'}) from ${String(n.source_ref)}: ${firstLine(n.body)}`;
}

export function undeliveredSummary(m: Json): string {
  return `${String(m.kind)} ${short(m.message_id)} ${String(m.state)} (${String(m.source_ref)} -> ${String(m.target_ref)}): ${firstLine(m.settle_reason)} [${firstLine(m.body)}]`;
}

/** The queue as list rows: escalations, reports, undelivered mail, then the next-page hint. */
export function escalationRows(q: Json): Json[] {
  const list = (k: string): Json[] => (Array.isArray(q[k]) ? (q[k] as Json[]) : []);
  const rows: Json[] = [
    ...list('escalations').map((h) => ({ ...h, title: hopSummary(h), state: String(h.state), queue: 'escalation' })),
    ...list('notices').map((n) => ({ ...n, title: noticeSummary(n), state: n.state === 'queued' ? 'unread' : 'read', queue: 'notice' })),
    ...list('undelivered').map((m) => ({ ...m, title: undeliveredSummary(m), state: String(m.state), queue: 'undelivered' })),
  ];
  if (typeof q.next_undelivered_after === 'string') {
    rows.push({ title: `More undelivered mail: :manager escalations undelivered ${q.next_undelivered_after}`, state: 'more', queue: 'more' });
  }
  return rows;
}

// ---------- :manager node ----------

export const NODE_USAGE = 'Use :manager node [list|get <UUID>|configure <JSON>|revoke <UUID> <epoch>].';

export type NodeCommand =
  | { verb: 'list' } | { verb: 'tree' }
  | { verb: 'get'; id: string }
  | { verb: 'configure'; json: string }
  | { verb: 'revoke'; id: string; epoch: number };

export function parseNodeCommand(arg: string | undefined): NodeCommand | { error: string } {
  const t = (arg ?? '').trim();
  if (t === '' || t === 'list') return { verb: 'list' };
  if (t === 'tree') return { verb: 'tree' };
  const w = words(t);
  if (w[0] === 'get') return w.length === 2 && UUID.test(w[1]!) ? { verb: 'get', id: w[1]!.toLowerCase() } : { error: 'Use :manager node get <node UUID>.' };
  if (w[0] === 'configure' && t.length > 10) return { verb: 'configure', json: t.slice(9).trim() };
  if (w[0] === 'revoke') {
    const epoch = Number(w[2]);
    if (w.length === 3 && UUID.test(w[1]!) && Number.isInteger(epoch) && epoch > 0) return { verb: 'revoke', id: w[1]!.toLowerCase(), epoch };
    return { error: 'Use :manager node revoke <node UUID> <epoch>.' };
  }
  return { error: NODE_USAGE };
}

/** One line per area node (harness_manager.rs node_summary). */
export function nodeSummary(n: Json): string {
  const sel = n.selector as Json | null | undefined;
  const scope = !sel ? 'revoked' : sel.mode === 'project' ? 'project'
    : `${Array.isArray(sel.group_ids) ? sel.group_ids.length : 0} Groups, ${Array.isArray(sel.epic_ids) ? sel.epic_ids.length : 0} Epics`;
  return `${String(n.node_id)} seat=${String(n.seat_root_session_id)} ${String(n.state)}/${String(n.grant_state)} scope=${scope} grant=${String(n.grant_version)} epoch=${String(n.authority_epoch)} reports=${String(n.direct_reports)}`;
}

// ---------- :manager restart ----------

export type RestartCommand = 'status' | 'now' | 'cancel';

export function parseRestart(arg: string | undefined): RestartCommand | { error: string } {
  const t = (arg ?? '').trim() || 'status';
  return t === 'status' || t === 'now' || t === 'cancel' ? t : { error: 'Use :manager restart [status|now|cancel].' };
}

/** OperatorRestartStatusV1::summary, else the last outcome (operator_restart_command.rs describe). */
export function restartDescribe(s: Json): string {
  const blockers = Array.isArray(s.blockers) ? s.blockers.map(String) : [];
  if (s.pending) {
    if (s.state === 'restarting') return 'restart in progress';
    if (s.forced) return 'restart pending: restarting now';
    const releaseBy = typeof s.release_by === 'string' ? `, release by ${s.release_by}` : '';
    const waiting: string[] = [];
    if (blockers.includes('landing_in_progress')) waiting.push('a landing');
    const turns = typeof s.turns_in_flight === 'number' ? s.turns_in_flight : 0;
    if (turns > 0) waiting.push(`${turns} turns`);
    if (blockers.includes('job_running')) waiting.push('a job');
    return waiting.length ? `restart pending: waiting for ${waiting.join(' + ')}${releaseBy}` : `restart pending: quiet, restarting${releaseBy}`;
  }
  if (typeof s.state === 'string') return typeof s.reason === 'string' ? `Last restart ${s.state}: ${s.reason}` : `Last restart ${s.state}`;
  return 'No restart pending.';
}
