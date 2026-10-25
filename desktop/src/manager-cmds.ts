// Manager `:` commands (hub #3000 gap sweep G3): the DOM side of
// `:manager global`, `:manager portfolio`, `:manager escalations`,
// `:manager node` and `:manager restart`, as the TUI's
// crates/rsi/src/overlay/*_command.rs run them: read, plan (the
// src-tauri/src/manager_ops.rs planners over the TUI's types), send, and
// report in one toast. A refusal that would lower project caps opens a
// review; `y` resends it with the confirmation (#1544). Pure rules are in
// manager-cmds-model.ts. Daemon text is set with textContent only.
import { toast } from './dom.js';
import {
  type NamedProject, type PortfolioNodeLite, capPreview, escalationRows, freeProjects, grantSummary, needsCapConfirm,
  nodeSummary, parseEscalationsCommand, parseGlobalCommand, parseNodeCommand, parsePortfolioCommand, parseRestart,
  portfolioSummary, resolveId, resolveNode, resolveProjects, restartDescribe,
} from './manager-cmds-model.js';
import { managersApi } from './managers-api.js';
import { ask, chooseProject, openList, openTree, refreshWorkspaceIfOpen, seatCandidate } from './managers.js';
import { state } from './state.js';
import { loadView } from './view-load.js';

type Json = Record<string, unknown>;

const errText = (e: unknown): string => String(e instanceof Error ? e.message : e);
const mint = (): string => crypto.randomUUID();
const projects = (): NamedProject[] => state.projects.map((p) => ({ id: p.id, name: p.name }));

/** Send; on a cap-reduction refusal show its preview and resend confirmed on `y`. */
export async function withCapConfirm<T>(title: string, send: (confirm: boolean) => Promise<T>): Promise<T | null> {
  try {
    return await send(false);
  } catch (e) {
    const text = errText(e);
    if (!needsCapConfirm(text)) throw e;
    const ok = await ask({
      title, lines: [capPreview(text), 'This lowers project caps: y confirms, Esc cancels.'], ok: 'Lower the caps', destructive: true,
    });
    return ok ? send(true) : null;
  }
}

function report(ok: string | null, fail?: string): void {
  if (fail) toast(fail);
  else if (ok) toast(ok, true);
}

function parseJson(raw: string, what: string): Json {
  let v: unknown;
  try { v = JSON.parse(raw); } catch (e) { throw new Error(`${what} JSON: ${errText(e)}`); }
  if (!v || typeof v !== 'object' || Array.isArray(v)) throw new Error(`${what} JSON must be an object`);
  return v as Json;
}

// ---------- :manager global ----------

async function globalCommand(arg: string | undefined): Promise<string | null> {
  const cmd = parseGlobalCommand(arg);
  if ('error' in cmd) throw new Error(cmd.error);
  const all = projects();
  switch (cmd.verb) {
    case 'show': {
      const g = await managersApi.globalGrant();
      return g ? grantSummary(g as unknown as Json, all) : 'No global manager is appointed. Focus a session and run :manager global appoint.';
    }
    case 'revoke': {
      const g = await managersApi.globalGrant();
      if (!g) throw new Error('No global manager is appointed.');
      const revoked = await managersApi.revokeGlobal(g.grant_version, mint());
      return `Global manager revoked: ${grantSummary(revoked, all)}`;
    }
    case 'configure': {
      const body = parseJson(cmd.json, 'Global manager request');
      const confirm = body.confirm_cap_reductions === true;
      delete body.confirm_cap_reductions;
      const g = await managersApi.configureGlobal(body, confirm);
      return `Global manager saved: ${grantSummary(g, all)}`;
    }
    case 'add-project': {
      const ids = resolveProjects(all, cmd.names);
      if (!Array.isArray(ids)) throw new Error(ids.error);
      const request = await managersApi.planGlobalAddProjects(await managersApi.globalGrant(), ids);
      const g = await withCapConfirm('Add projects to the global manager’s grant', (c) => managersApi.configureGlobal(request, c));
      return g ? `Global manager grant updated: ${grantSummary(g, all)}` : null;
    }
    case 'set': {
      const plan = await managersApi.planGlobalSet(await managersApi.globalGrant(), cmd.field, cmd.value);
      const send = (c: boolean) => managersApi.configureGlobal(plan.request, c);
      const g = cmd.confirm ? await send(true) : await withCapConfirm('Change the global manager caps', send);
      return g ? `Global manager ${plan.change}: ${grantSummary(g, all)}` : null;
    }
    case 'appoint': case 'prefill': {
      const seat = cmd.verb === 'appoint' ? seatCandidate() : null;
      if (cmd.verb === 'appoint' && !seat) throw new Error('Focus the session to appoint as the global manager.');
      const current = await managersApi.globalGrant();
      const active = current && current.state === 'active' ? current : null;
      let ids: string[];
      if (active && !cmd.names.trim()) ids = active.project_ids;
      else {
        const r = resolveProjects(all, cmd.names);
        if (!Array.isArray(r)) throw new Error(r.error);
        ids = r;
      }
      const nodes = current && current.state !== 'active' && !cmd.useDefaults ? await managersApi.portfolioNodes(true) : { nodes: [] };
      const plan = await managersApi.planGlobalAppoint(current, nodes, seat?.id ?? null, ids, cmd.useDefaults);
      if (!plan.request) return plan.note;
      const g = await withCapConfirm('Appoint the global manager', (c) => managersApi.configureGlobal(plan.request!, c));
      return g ? `Global manager appointed: ${grantSummary(g, all)}${plan.note}` : null;
    }
  }
}

// ---------- :manager portfolio ----------

async function portfolioCommand(arg: string | undefined): Promise<string | null> {
  const cmd = parsePortfolioCommand(arg);
  if ('error' in cmd) throw new Error(cmd.error);
  const all = projects();
  const nodeOf = async (argText: string, includeRevoked: boolean): Promise<PortfolioNodeLite & Json> => {
    const list = (await managersApi.portfolioNodes(includeRevoked)).nodes as unknown as PortfolioNodeLite[];
    const id = resolveNode(list, argText);
    if (typeof id !== 'string') throw new Error(id.error);
    const node = await managersApi.portfolioNode(id);
    if (!node) throw new Error(`No portfolio node ${argText}`);
    return node as unknown as PortfolioNodeLite & Json;
  };
  switch (cmd.verb) {
    case 'list': return null;
    case 'show': return portfolioSummary(await nodeOf(cmd.node, true), all);
    case 'revoke': {
      const n = await nodeOf(cmd.node, false);
      const ok = await ask({ title: `Revoke ${n.tier_label} node ${n.node_id.slice(0, 8)}?`, lines: [portfolioSummary(n, all), 'Operator-granted children move up a level; the node’s own appointees are revoked with their subtree.'], ok: 'Revoke', destructive: true });
      if (!ok) return null;
      const r = await managersApi.revokePortfolio({ node_id: n.node_id, expected_grant_version: n.grant.grant_version, expected_authority_epoch: n.authority_epoch, idempotency_key: mint() });
      return `Portfolio node revoked: ${portfolioSummary(r as unknown as PortfolioNodeLite, all)}`;
    }
    case 'configure': {
      const body = parseJson(cmd.json, 'Portfolio request');
      const confirm = body.confirm_cap_reductions === true;
      delete body.confirm_cap_reductions;
      const n = await managersApi.configurePortfolioRequest(body, confirm);
      return `Portfolio node saved: ${portfolioSummary(n as unknown as PortfolioNodeLite, all)}`;
    }
    case 'launch': {
      const n = await nodeOf(cmd.node, false);
      const request = await managersApi.planPortfolioLaunch(n, cmd.add, cmd.launches);
      const saved = await managersApi.portfolioLaunches(request, false);
      return `Portfolio launches updated (${saved.grant.allowed_launches.length} allowed): ${portfolioSummary(saved as unknown as PortfolioNodeLite, all)}`;
    }
    case 'appoint': {
      const seat = seatCandidate();
      if (!seat) throw new Error('Focus the session to appoint as the node’s seat.');
      const nodes = await managersApi.portfolioNodes(true);
      const active = (nodes.nodes as unknown as PortfolioNodeLite[]).filter((n) => n.state === 'active');
      const adopt: string[] = [];
      for (const a of cmd.adopt) {
        const id = resolveNode(active, a);
        if (typeof id !== 'string') throw new Error(id.error);
        adopt.push(id);
      }
      let ids: string[] = [];
      if (cmd.projects.trim()) {
        const r = resolveProjects(all, cmd.projects);
        if (!Array.isArray(r)) throw new Error(r.error);
        ids = r;
      } else if (!adopt.length) {
        ids = freeProjects(all, active);
      }
      const plan = await managersApi.planPortfolioAppoint(nodes, cmd.label, seat.id, ids, adopt);
      const n = await withCapConfirm('Appoint the portfolio node', (c) => managersApi.configurePortfolioRequest(plan.request, c));
      if (!n) return null;
      const summary = portfolioSummary(n as unknown as PortfolioNodeLite, all);
      return adopt.length ? `Portfolio node appointed above ${adopt.length} node(s): ${summary}` : `Portfolio node appointed: ${summary}${plan.note ? `. ${plan.note}` : ''}`;
    }
  }
}

// ---------- :manager escalations ----------

async function escalationsCommand(arg: string | undefined): Promise<string | null> {
  const cmd = parseEscalationsCommand(arg);
  if ('error' in cmd) throw new Error(cmd.error);
  switch (cmd.verb) {
    case 'list':
      await openList(cmd.includeClosed ? 'Operator escalations (all)' : 'Operator escalations', async () => escalationRows(await managersApi.escalations(cmd.includeClosed)));
      return null;
    case 'undelivered':
      await openList('Undelivered tier mail', async () => {
        const rows = escalationRows({ ...(await managersApi.undelivered(cmd.after)), escalations: [], notices: [] });
        return rows.length ? rows : [{ title: 'No undelivered tier mail on this page.', state: '' }];
      });
      return null;
    case 'rule': {
      const q = await managersApi.escalations(false);
      const ids = ((q.escalations as Json[] | undefined) ?? []).map((h) => String(h.hop_id));
      const hop = resolveId(ids, cmd.hop, 'escalation');
      if (typeof hop !== 'string') throw new Error(hop.error);
      const ruled = await managersApi.ruleEscalation(hop, cmd.ruling, mint());
      return `Escalation ruled: ${escalationRows({ escalations: [ruled] })[0]!.title as string}`;
    }
    case 'ack': {
      const q = await managersApi.escalations(false);
      const ids = ((q.notices as Json[] | undefined) ?? []).map((n) => String(n.message_id));
      const id = resolveId(ids, cmd.notice, 'report');
      if (typeof id !== 'string') throw new Error(id.error);
      const read = await managersApi.ackNotice(id);
      return `Report read: ${escalationRows({ notices: [read] })[0]!.title as string}`;
    }
  }
}

// ---------- :manager node ----------

function nodeProject(): string {
  const pid = seatCandidate()?.project_id ?? chooseProject();
  if (!pid) throw new Error('Select a project first: a manager node belongs to one project.');
  return pid;
}

async function nodeCommand(arg: string | undefined): Promise<string | null> {
  const cmd = parseNodeCommand(arg);
  if ('error' in cmd) throw new Error(cmd.error);
  switch (cmd.verb) {
    case 'tree': await openTree(); return null;
    case 'list': {
      const pid = nodeProject();
      await openList('Manager area nodes', async () => {
        const page = await managersApi.managerNodes(pid, null);
        const rows: Json[] = page.rows.map((n) => ({ title: nodeSummary(n), state: String(n.state), ...n }));
        if (page.next_after_node_id) rows.push({ title: 'More nodes available through ListManagerNodes RPC.', state: 'more' });
        return rows.length ? rows : [{ title: 'No manager nodes for this project.', state: '' }];
      });
      return null;
    }
    case 'get': {
      const n = await managersApi.areaNode(nodeProject(), cmd.id);
      if (!n) throw new Error('Manager node unavailable.');
      return nodeSummary(n);
    }
    case 'configure': {
      const body = parseJson(cmd.json, 'Manager node request');
      if (body.project_id !== nodeProject()) throw new Error('Manager node project differs from the selected project.');
      return `Manager node saved: ${nodeSummary(await managersApi.configureNode(body))}`;
    }
    case 'revoke': {
      const pid = nodeProject();
      const n = await managersApi.areaNode(pid, cmd.id);
      if (!n) throw new Error('Manager node unavailable.');
      if (n.authority_epoch !== cmd.epoch) throw new Error('Manager node changed. Run :manager node get and review before revoking.');
      const r = await managersApi.revokeArea({ project_id: pid, node_id: cmd.id, expected_grant_version: n.grant_version, expected_authority_epoch: cmd.epoch, idempotency_key: mint() });
      return `Manager node revoked: ${nodeSummary(r)}`;
    }
  }
}

// ---------- :manager restart ----------

async function restartCommand(arg: string | undefined): Promise<string | null> {
  const cmd = parseRestart(arg);
  if (typeof cmd !== 'string') throw new Error(cmd.error);
  if (cmd === 'status') {
    await loadView({
      key: 'mgr-restart', title: 'Operator quiet-point restart', failed: 'Restart status unavailable',
      read: () => managersApi.restartStatus(),
      open: (status) => report(restartDescribe(status)),
    });
    return null;
  }
  const s = cmd === 'now' ? await managersApi.restartForce() : await managersApi.restartCancel();
  return restartDescribe(s);
}

// ---------- entry points ----------

type Runner = (arg: string | undefined) => Promise<string | null>;

const RUNNERS: Readonly<Record<string, readonly [Runner, string]>> = {
  global: [globalCommand, 'Global manager'],
  portfolio: [portfolioCommand, 'Portfolio'],
  escalations: [escalationsCommand, 'Escalations'],
  node: [nodeCommand, 'Manager node'],
  restart: [restartCommand, 'Restart'],
};

/** Run one `:manager <family> [arg]` command and toast its outcome; refresh an open workspace after a change. */
export async function runManagerCommand(family: keyof typeof RUNNERS, arg: string | undefined): Promise<void> {
  const [run, label] = RUNNERS[family]!;
  try {
    report(await run(arg));
  } catch (e) {
    const text = errText(e);
    report(null, text.startsWith(`${label}:`) || text.startsWith('Use :') ? text : `${label}: ${text}`);
  }
  if (family !== 'escalations') void refreshWorkspaceIfOpen();
}
