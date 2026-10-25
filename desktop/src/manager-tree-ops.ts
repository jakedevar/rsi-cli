// The manager tree's in-place actions on global, portfolio and area rows
// (hub #3000 gap sweep G3), as crates/rsi/src/overlay/manager_tree/actions.rs
// runs them: `a` replaces a global or portfolio seat with the focused session,
// `e` edits a global or portfolio grant (granted projects and per-project caps)
// or carves an area node's allowance, `A` appoints the focused session above a
// portfolio node and `m` (here, then on a sibling) moves a node under that
// sibling. Each action re-reads the node, refreshes instead when the tree was
// stale, shows the change and its descendant impact, and needs `y` when it is
// destructive. The requests are built by src-tauri/src/manager_ops.rs.
import { toast } from './dom.js';
import { withCapConfirm } from './manager-cmds.js';
import { managersApi } from './managers-api.js';
import {
  type GrantCaps, NODE_FIELDS, type SeatCandidate, type TreeRow, capsFromForm, capsOf, descendants, editorProjects,
  grantEditReview, nodeEditValues,
} from './managers-model.js';
import { ask } from './managers.js';
import { formDialog } from './mgr-form.js';
import { state } from './state.js';

type Json = Record<string, unknown>;

const errText = (e: unknown): string => String(e instanceof Error ? e.message : e);
const short = (id: unknown): string => String(id ?? '').slice(0, 8);
const projects = () => state.projects.map((p) => ({ id: p.id, name: p.name }));

export interface TreeContext {
  rows: readonly TreeRow[];
  globalVersion: number | null;
  candidate: SeatCandidate | null;
  /** Reload the tree, with a notice when the action found it stale. */
  reload(notice?: string): void;
}

function impact(ctx: TreeContext, r: TreeRow): string {
  return `${descendants(ctx.rows, r.key).length} row(s) below it are affected.`;
}

async function confirmAndSend(title: string, lines: string[], destructive: boolean, rpc: string, send: (confirm: boolean) => Promise<unknown>, ctx: TreeContext, done: string): Promise<void> {
  // Plain words first; the RPC and the versions it expects sit under Details (local #223).
  if (!(await ask({ title, lines, details: rpc, ok: destructive ? 'Send (y)' : 'Send', destructive }))) return;
  try {
    if ((await withCapConfirm(title, send)) !== null) toast(done, true);
  } catch (e) {
    toast(`The daemon refused: ${errText(e)}`);
  }
  ctx.reload();
}

// ---------- current grants, with the stale check ----------

async function currentGlobal(ctx: TreeContext): Promise<Json | null> {
  const g = (await managersApi.globalGrant()) as Json | null;
  if (!g || g.state !== 'active' || g.grant_version !== ctx.globalVersion) {
    ctx.reload('The global grant changed: the tree was refreshed');
    return null;
  }
  return g;
}

async function currentPortfolio(ctx: TreeContext, r: TreeRow): Promise<Json | null> {
  const n = (await managersApi.portfolioNode(r.node_id!)) as unknown as Json | null;
  const grant = n?.grant as Json | undefined;
  if (!n || n.state !== 'active' || grant?.grant_version !== r.grant?.grant_version) {
    ctx.reload('The node’s grant changed: the tree was refreshed');
    return null;
  }
  return n;
}

// ---------- a: replace a global or portfolio seat ----------

export async function appointSeat(ctx: TreeContext, r: TreeRow): Promise<void> {
  const c = ctx.candidate!;
  try {
    if (r.kind === 'portfolio') {
      const n = await currentPortfolio(ctx, r);
      if (!n) return;
      const grant = n.grant as Json;
      const request = await managersApi.planPortfolioEdit(n, c.id, null, null);
      await confirmAndSend(`Replace the seat of the ${String(n.tier_label)} node ${short(n.node_id)}`,
        [`Seat: ${short(grant.seat_session_id)} → ${short(c.id)}; projects, launches and policy unchanged; a new authority epoch opens.`, impact(ctx, r)],
        true, `ConfigurePortfolioNode (expects grant v${String(grant.grant_version)} · epoch ${String(n.authority_epoch)})`,
        (confirm) => managersApi.configurePortfolioRequest(request, confirm), ctx, `Seat of ${r.label} replaced`);
      return;
    }
    const g = await currentGlobal(ctx);
    if (!g) return;
    const request = await managersApi.planGlobalEdit(g, c.id, g.project_ids as string[], null);
    await confirmAndSend('Replace the global manager seat',
      [`Seat: ${short(g.seat_session_id)} → ${short(c.id)}; projects, launches and PM policy unchanged.`, impact(ctx, r)],
      true, `ConfigureGlobalManager (expects grant v${String(g.grant_version)})`,
      (confirm) => managersApi.configureGlobal(request, confirm), ctx, 'Global manager seat replaced');
  } catch (e) {
    toast(`Manager tree: ${errText(e)}`);
  }
}

// ---------- e: a global or portfolio grant's projects and caps ----------

function parentCapsLine(ctx: TreeContext, parentNodeId: unknown): string | null {
  if (typeof parentNodeId !== 'string') return null;
  const row = ctx.rows.find((x) => x.node_id === parentNodeId);
  const g = row?.grant;
  if (!row || !g) return null;
  return `Parent ${row.label}: active ${g.max_active_sessions} · sessions ${g.max_created_sessions} · containers ${g.max_created_containers} · spend ${g.max_spend_usd === null ? 'none' : g.max_spend_usd} (a lower parent cap binds first).`;
}

async function grantForm(title: string, granted: readonly string[], caps: GrantCaps, note: string | null): Promise<{ ids: string[]; caps: GrantCaps } | null> {
  const got = await formDialog({
    title,
    lines: [note ?? 'Granted projects and the caps each covered project’s manager receives.'],
    fields: [
      { key: 'max_active_sessions', label: 'Max active sessions', kind: 'number', value: String(caps.max_active_sessions), hint: '1-100' },
      { key: 'max_created_sessions', label: 'Max created sessions', kind: 'number', value: String(caps.max_created_sessions), hint: '0-1024' },
      { key: 'max_created_containers', label: 'Max created containers', kind: 'number', value: String(caps.max_created_containers), hint: '0-64' },
      { key: 'max_spend_usd', label: 'Max spend (USD)', kind: 'text', value: caps.max_spend_usd === null ? '' : String(caps.max_spend_usd), hint: 'blank = no cap' },
      { key: 'allow_create_groups', label: 'May create Groups', kind: 'check', value: caps.allow_create_groups },
    ],
    checks: { title: 'Granted projects', items: editorProjects(projects(), granted).map((p) => ({ value: p.id, label: p.name, on: p.granted })) },
    ok: 'Review',
  });
  if (!got) return null;
  const parsed = capsFromForm(got.values);
  if ('error' in parsed) { toast(parsed.error); return null; }
  return { ids: got.checked, caps: parsed };
}

export async function editGrant(ctx: TreeContext, r: TreeRow): Promise<void> {
  try {
    const portfolio = r.kind === 'portfolio';
    const current = portfolio ? await currentPortfolio(ctx, r) : await currentGlobal(ctx);
    if (!current) return;
    const grant = (portfolio ? current.grant : current) as Json;
    const before = { ids: (grant.project_ids as string[]) ?? [], caps: capsOf((grant.project_policy as Json) ?? {}) };
    const title = portfolio ? `Edit the ${String(current.tier_label)} node’s grant` : 'Edit the global grant';
    const after = await grantForm(title, before.ids, before.caps, portfolio ? parentCapsLine(ctx, current.parent_node_id) : null);
    if (!after) return;
    const review = grantEditReview(projects(), before, after);
    if ('error' in review) { toast(review.error); return; }
    const caps = after.caps as unknown as Json;
    if (portfolio) {
      const request = await managersApi.planPortfolioEdit(current, null, after.ids, caps);
      await confirmAndSend(`Edit the ${String(current.tier_label)} node’s ${review.scope}`, [...review.lines, impact(ctx, r)], review.destructive,
        `ConfigurePortfolioNode (expects grant v${String(grant.grant_version)} · epoch ${String(current.authority_epoch)})`,
        (confirm) => managersApi.configurePortfolioRequest(request, confirm), ctx, `${r.label} saved`);
    } else {
      const request = await managersApi.planGlobalEdit(current, null, after.ids, caps);
      await confirmAndSend(`Edit the global grant’s ${review.scope}`, [...review.lines, impact(ctx, r)], review.destructive,
        `ConfigureGlobalManager (expects grant v${String(grant.grant_version)})`,
        (confirm) => managersApi.configureGlobal(request, confirm), ctx, 'Global grant saved');
    }
  } catch (e) {
    toast(`Manager tree: ${errText(e)}`);
  }
}

// ---------- e: an area node's allowance ----------

export async function editArea(ctx: TreeContext, r: TreeRow): Promise<void> {
  try {
    const node = await managersApi.areaNode(r.project_id!, r.node_id!);
    if (!node || node.grant_version !== r.grant?.grant_version) { ctx.reload('The node’s grant changed: the tree was refreshed'); return; }
    if (typeof node.parent_node_id !== 'string') { toast('The project root’s grant is edited from the project (its policy), not here'); return; }
    if (typeof node.direct_reports === 'number' && node.direct_reports > 0) { ctx.reload('The node gained direct reports: the tree was refreshed'); return; }
    const parent = await managersApi.areaNode(r.project_id!, node.parent_node_id);
    if (!parent) { toast('The parent node is unavailable'); return; }
    const grant = (node.grant ?? {}) as Json;
    const allowance = (grant.allowance ?? {}) as Json;
    const original = [Number(allowance.max_active_sessions), Number(allowance.max_created_sessions), Number(grant.max_direct_reports)];
    const got = await formDialog({
      title: `Edit the allowance of ${r.label}`,
      lines: ['The daemon keeps a node strictly narrower than its parent.'],
      fields: NODE_FIELDS.map((f, i) => ({ key: String(i), label: f.label, kind: 'number', value: String(original[i]), hint: `${f.min}-${f.max}` })),
      ok: 'Review',
    });
    if (!got) return;
    const v = nodeEditValues(NODE_FIELDS.map((_, i) => String(got.values[String(i)] ?? '')), original);
    if ('error' in v) { toast(v.error); return; }
    const request = await managersApi.planNodeEdit(node, parent, v.values);
    await confirmAndSend(`Edit the allowance of ${r.label}`, [...v.lines, impact(ctx, r)], v.destructive,
      `ConfigureManagerNode (expects grant v${String(node.grant_version)} · parent v${String(parent.grant_version)}/${String(parent.policy_version)}/epoch ${String(parent.authority_epoch)})`,
      () => managersApi.configureNode(request), ctx, `${r.label} saved`);
  } catch (e) {
    toast(`Manager tree: ${errText(e)}`);
  }
}

// ---------- A: appoint above; m: move under a sibling ----------

export async function appointAbove(ctx: TreeContext, r: TreeRow): Promise<void> {
  const c = ctx.candidate!;
  try {
    const n = await currentPortfolio(ctx, r);
    if (!n) return;
    const grant = n.grant as Json;
    const plan = await managersApi.planPortfolioAbove(n, c.id);
    await confirmAndSend(`Appoint a ${plan.label} manager above the ${String(n.tier_label)} node ${short(n.node_id)}`, [
      `Appoints ${short(c.id)} as a new ${plan.label} manager over the node’s ${(grant.project_ids as string[]).length} project(s), ${typeof n.parent_node_id === 'string' ? `under its parent ${short(n.parent_node_id)}` : 'as a root'}.`,
      'The node and its descendants move one level down under new grant versions, keeping their seats, grantors, epochs, ledgers and workers; their queued mail is retired.',
      impact(ctx, r),
    ], true, `ConfigurePortfolioNode (adopts ${short(n.node_id)} at grant v${String(grant.grant_version)})`,
    (confirm) => managersApi.configurePortfolioRequest(plan.request, confirm), ctx, `${plan.label} manager appointed above ${r.label}`);
  } catch (e) {
    toast(`Manager tree: ${errText(e)}`);
  }
}

/** The node `m` marked, waiting for `m` on a sibling (Esc cancels). */
let moveSource: { nodeId: string; label: string; key: string } | null = null;

export const pendingMove = (): string | null => moveSource?.label ?? null;
export function cancelMove(): boolean {
  const had = moveSource !== null;
  moveSource = null;
  return had;
}

export async function moveUnder(ctx: TreeContext, r: TreeRow): Promise<void> {
  if (!moveSource || moveSource.nodeId === r.node_id) {
    moveSource = { nodeId: r.node_id!, label: r.label, key: r.key };
    toast(`Moving ${r.label}: select the sibling to move it under and press m (Esc cancels).`, true);
    return;
  }
  const source = moveSource;
  moveSource = null;
  try {
    const [target, moved] = await Promise.all([managersApi.portfolioNode(r.node_id!), managersApi.portfolioNode(source.nodeId)]);
    if (!target || !moved) { ctx.reload('A node of the move is gone: the tree was refreshed'); return; }
    const plan = await managersApi.planPortfolioMove(target, moved);
    const names = plan.added.map((id) => state.projects.find((p) => p.id === id)?.name ?? short(id));
    const movedRow = ctx.rows.find((x) => x.key === source.key);
    await confirmAndSend(`Move ${source.label} under ${r.label}`, [
      `${r.label} widens to cover ${plan.added.length} more project(s) [${names.join(', ')}] and adopts ${source.label}; its grant widens only as far as the moved node needs, and a new authority epoch opens.`,
      `${source.label} and its descendants move one level down under new grant versions, keeping their seats, grantors, epochs, ledgers and workers; their queued mail is retired.`,
      movedRow ? impact(ctx, movedRow) : '',
    ].filter(Boolean), true,
    `ConfigurePortfolioNode (expects grant v${target.grant.grant_version} · epoch ${target.authority_epoch}; adopts ${short(moved.node_id)})`,
    (confirm) => managersApi.configurePortfolioRequest(plan.request, confirm), ctx, `${source.label} moved under ${r.label}`);
  } catch (e) {
    toast(`Manager tree: ${errText(e)}`);
  }
}
