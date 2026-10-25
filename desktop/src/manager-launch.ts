// The workspace's `n` (hub #3000 gap sweep G3): launch a fresh session and
// appoint it as the global seat or a project's PM in one flow, as
// crates/rsi/src/overlay/global_manager_workspace_launch.rs does, and the
// grant editor (crates/rsi/src/overlay/grant_editor.rs) the form opens to edit
// what that manager may do. The global seat is appointed through
// ConfigureGlobalManager (with the cap-reduction review), a PM through
// ConfigureHarnessManagerWithPolicy (appointment and policy in one
// transaction). When the launch succeeded but the appointment was refused,
// the next `n` for the same seat retries only the appointment.
import { api } from './api.js';
import { toast } from './dom.js';
import { withCapConfirm } from './manager-cmds.js';
import { managersApi } from './managers-api.js';
import {
  CAPABILITIES, type LaunchChoice, type LaunchPlan, type LaunchRole, MODES, NOT_GRANTED_HINT, NOT_GRANTED_MARK, PRESETS,
  ancestorLaunches, defaultLaunchPrompt, launchCatalog, launchHost, launchScope, launchText, launchedSessionId, uncoveredLaunchTexts,
  validateLaunch,
} from './managers-model.js';
import { formDialog } from './mgr-form.js';
import { ownProjects } from './project-model.js';
import { state } from './state.js';

type Json = Record<string, unknown>;

const errText = (e: unknown): string => String(e instanceof Error ? e.message : e);
const short = (id: string): string => id.slice(0, 8);

// ---------- the grant editor ----------

/**
 * The launches the live global or portfolio grants above `projectId` admit, or null when none covers the project or
 * rsid would not answer (the appointment still refuses what is not covered; this only feeds the marks on the editor).
 */
export async function launchCoverage(projectId: string): Promise<LaunchChoice[] | null> {
  try { return ancestorLaunches((await managersApi.portfolioNodes(false)).nodes, projectId); } catch { return null; }
}

/** The marks for a launch checklist: each catalog launch `ancestors` do not cover gets the warning, and the list gets the fix. */
export function launchMarks(catalog: readonly LaunchChoice[], ancestors: readonly LaunchChoice[] | null): { marks: ReadonlyMap<string, string>; note: string | undefined } {
  const marks = new Map<string, string>();
  for (const text of uncoveredLaunchTexts(catalog, ancestors)) marks.set(text, NOT_GRANTED_MARK);
  return { marks, note: marks.size ? NOT_GRANTED_HINT : undefined };
}

/**
 * Edit a manager policy and, for the global grant, its launch list. A preset
 * applies and reopens the editor on the result; the rest is checked by
 * ManagerPolicyV2::validate (manager_ops.rs) before it is returned. For a
 * project manager, `ancestors` (the launches the global grant admits, from
 * `launchCoverage`) marks each launch outside them, as the TUI does (#3270).
 */
export async function grantEditor(title: string, start: Json, startLaunches: LaunchChoice[] | null, catalog: readonly LaunchChoice[], ancestors: readonly LaunchChoice[] | null = null): Promise<{ policy: Json; launches: LaunchChoice[] } | null> {
  let policy = start;
  for (;;) {
    const inPolicy = startLaunches === null;
    const launches = inPolicy ? ((policy.allowed_launches as LaunchChoice[] | undefined) ?? []) : startLaunches;
    const offered = launchCatalog(catalog, launches);
    const { marks, note } = inPolicy ? launchMarks(offered, ancestors) : { marks: new Map<string, string>(), note: undefined };
    const got = await formDialog({
      title,
      lines: [inPolicy ? 'The policy this project manager receives. No launch checked means any valid choice.' : 'The launches the global seat may start project managers from, and the policy body they receive. At least one launch.'],
      fields: [
        { key: 'preset', label: 'Preset', kind: 'select', value: 'keep', options: [['keep', 'Keep these fields'], ...PRESETS.map((p) => [p.id, `Apply ${p.label}`] as const)] },
        { key: 'mode', label: 'Mode', kind: 'select', value: String(policy.mode ?? 'status'), options: MODES.map((m) => [m, m] as const) },
        { key: 'max_created_containers', label: 'Created containers', kind: 'number', value: String(policy.max_created_containers ?? 0), hint: '0-64' },
        { key: 'max_created_sessions', label: 'Created sessions', kind: 'number', value: String(policy.max_created_sessions ?? 0), hint: '0-1024' },
        { key: 'max_active_sessions', label: 'Active sessions', kind: 'number', value: String(policy.max_active_sessions ?? 4), hint: '1-100' },
        { key: 'max_spend_usd', label: 'Spend ceiling (USD)', kind: 'text', value: typeof policy.max_spend_usd === 'number' ? String(policy.max_spend_usd) : '', hint: 'blank = uncapped' },
      ],
      lists: [
        { key: 'caps', title: 'Capabilities', items: CAPABILITIES.map((c) => ({ value: c, label: c, on: ((policy.capabilities as string[] | undefined) ?? []).includes(c) })) },
        { key: 'launches', title: 'Launches', note, items: offered.map((l) => ({ value: launchText(l), label: launchText(l), on: launches.some((x) => launchText({ ...x, effort: x.effort ?? null }) === launchText(l)), mark: marks.get(launchText(l)) })) },
      ],
      ok: 'Check and use',
    });
    if (!got) return null;
    const preset = String(got.values.preset);
    if (preset !== 'keep') {
      try { policy = await managersApi.policyPreset(policy, preset, 'project', false); } catch (e) { toast(errText(e)); }
      continue;
    }
    const int = (k: string) => Number(String(got.values[k]).trim());
    const spendText = String(got.values.max_spend_usd).trim();
    const picked = offered.filter((l) => (got.lists.launches ?? []).includes(launchText(l)));
    const draft: Json = {
      ...policy, mode: got.values.mode, capabilities: got.lists.caps ?? [],
      max_created_containers: int('max_created_containers'), max_created_sessions: int('max_created_sessions'), max_active_sessions: int('max_active_sessions'),
      max_spend_usd: spendText ? Number(spendText) : null,
      ...(inPolicy ? { allowed_launches: picked } : {}),
    };
    if (!inPolicy && !picked.length) { toast('A global grant must allow at least one launch'); policy = draft; continue; }
    try {
      return { policy: await managersApi.policyCheck(draft), launches: picked };
    } catch (e) {
      toast(errText(e));
      policy = draft;
    }
  }
}

// ---------- n: launch and appoint ----------

/** A launched session whose appointment was refused: `n` for the same seat retries only the appointment. */
let pending: { key: string; sessionId: string; plan: LaunchPlan; grant: { policy: Json; launches: LaunchChoice[] } | null } | null = null;

const roleKey = (r: LaunchRole): string => (r.kind === 'global' ? 'global' : `project:${r.projectId}`);

async function appoint(plan: LaunchPlan, sessionId: string, grant: { policy: Json; launches: LaunchChoice[] } | null): Promise<string> {
  if (plan.role.kind === 'global') {
    const current = await managersApi.globalGrant();
    const replaced = current?.state === 'active' && current.seat_session_id !== sessionId;
    const nodes = current && current.state !== 'active' && !grant ? await managersApi.portfolioNodes(true) : { nodes: [] };
    const out = await managersApi.planGlobalAppoint(current, nodes, sessionId, plan.scope, false);
    const request = { ...out.request! };
    if (grant) { request.allowed_launches = grant.launches; request.project_policy = grant.policy; }
    const g = await withCapConfirm('Appoint the global manager', (c) => managersApi.configureGlobal(request, c));
    if (!g) throw new Error('the cap-reduction review was cancelled');
    const n = (g.project_ids as string[] | undefined)?.length ?? plan.scope.length;
    return `Global manager ${short(sessionId)} appointed (grant v${String(g.grant_version)}, ${n} project${n === 1 ? '' : 's'})${replaced ? '; it replaces the previous seat' : ''}${grant ? '' : out.note}`;
  }
  const pid = plan.role.projectId;
  const [config, saved] = await Promise.all([managersApi.config(pid), managersApi.policy(pid)]);
  const request = await managersApi.planPmAppoint(pid, sessionId, config?.row_version ?? 0, saved?.row_version ?? 0, grant?.policy ?? null);
  await managersApi.appointWithPolicy(request);
  const replaced = !!config && config.manager_session_id !== sessionId && (config.epic_ids.length > 0 || config.scope_mode === 'project');
  return `Project manager ${short(sessionId)}: appointed for the whole project${replaced ? '; it replaces the previous manager' : ''}`;
}

/**
 * Open the form for `role`, launch, appoint, and answer the notice (or null
 * when the operator cancelled). Errors stay actionable: a refused
 * appointment keeps the launched session for the retry.
 */
export async function launchAndAppoint(role: LaunchRole, activeProject: string | null): Promise<{ message: string; sessionId: string } | null> {
  const key = roleKey(role);
  if (pending?.key === key) {
    const p = pending;
    try {
      const message = await appoint(p.plan, p.sessionId, p.grant);
      pending = null;
      return { message, sessionId: p.sessionId };
    } catch (e) {
      throw new Error(`Session ${short(p.sessionId)} is still unappointed: ${errText(e)}. n retries the appointment.`);
    }
  }
  const projects = state.projects;
  if (!ownProjects(projects).length) throw new Error('No projects exist yet; create one before appointing a manager');
  const current = role.kind === 'global' ? await managersApi.globalGrant() : null;
  const active = current?.state === 'active' ? current : null;
  const defaults = await managersApi.defaultGrant();
  const grantLaunches = ((current as { allowed_launches?: LaunchChoice[] } | null)?.allowed_launches ?? []);
  const catalog = launchCatalog(grantLaunches, defaults.launches as unknown as LaunchChoice[]);
  const scope = role.kind === 'global' ? launchScope(projects, active?.project_ids ?? null) : [];
  const names = (ids: readonly string[]) => ids.map((id) => projects.find((p) => p.id === id)?.name ?? short(id));
  let note = '';
  if (role.kind === 'global' && current && current.state !== 'active') {
    try { note = (await managersApi.planGlobalAppoint(current, await managersApi.portfolioNodes(true), null, scope, false)).note; } catch { note = ''; }
  }
  const got = await formDialog({
    title: role.kind === 'global' ? 'Launch and appoint the global manager' : `Launch and appoint the project manager of ${role.name}`,
    lines: [note || 'A fresh session is launched with this instruction, then appointed.'],
    fields: [
      { key: 'launch', label: 'Provider / model / effort', kind: 'select', value: launchText(catalog[0]!), options: catalog.map((l) => [launchText(l), launchText(l)] as const) },
      { key: 'prompt', label: 'Prompt', kind: 'textarea', value: defaultLaunchPrompt(role, names(scope)) },
      ...(role.kind === 'global' ? [{ key: 'host', label: 'Host project', kind: 'select' as const, value: launchHost(scope, null, activeProject) ?? '', options: projects.map((p) => [p.id, p.name] as const), hint: 'the session’s home; must be in scope' }] : []),
      { key: 'edit', label: 'Edit the grant first', kind: 'check', value: false },
    ],
    ...(role.kind === 'global' ? { checks: { title: 'Scope', items: projects.map((p) => ({ value: p.id, label: p.name, on: scope.includes(p.id) })) } } : {}),
    ok: 'Launch and appoint',
  });
  if (!got) return null;
  const launch = catalog.find((l) => launchText(l) === got.values.launch) ?? null;
  const chosenScope = role.kind === 'global' ? got.checked : [];
  const plan = validateLaunch(role, launch, catalog, String(got.values.prompt), chosenScope, role.kind === 'global' ? launchHost(chosenScope, String(got.values.host), activeProject) : null, projects);
  if ('error' in plan) throw new Error(plan.error);
  let grant: { policy: Json; launches: LaunchChoice[] } | null = null;
  if (got.values.edit === true) {
    if (role.kind === 'global') {
      grant = await grantEditor('Global grant', (active?.project_policy as Json | undefined) ?? defaults.policy, active ? grantLaunches : (defaults.launches as unknown as LaunchChoice[]), catalog);
    } else {
      const saved = await managersApi.policy(role.projectId);
      grant = await grantEditor(`Policy of ${role.name}’s manager`, saved?.policy ?? defaults.policy, null, catalog, await launchCoverage(role.projectId));
    }
    if (!grant) return null;
  }
  const home = projects.find((p) => p.id === plan.home);
  const launched = launchedSessionId(await api.launch({
    query: plan.prompt, provider: plan.launch.provider, model: plan.launch.model, effort: plan.launch.effort ?? undefined,
    project_id: plan.home, working_dir: home?.path ?? undefined,
  }));
  if (!launched) throw new Error('Launch refused: the daemon returned no session id');
  try {
    return { message: await appoint(plan, launched, grant), sessionId: launched };
  } catch (e) {
    pending = { key, sessionId: launched, plan, grant };
    throw new Error(`Launched session ${short(launched)} but the appointment was refused: ${errText(e)}. n retries the appointment.`);
  }
}
