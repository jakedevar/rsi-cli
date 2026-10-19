// Topology preview (hub #3013, slice 11): the TUI's compact DAG preview of a
// stored TopologyDefinition (crates/rsi/src/ui/overlay/topology_preview.rs
// render_dag_ascii), shown before `gR` runs an Epic's bound topology and in
// the topology picker. Pure; tested in tests/graphs.test.mjs.

/** rsi_common::types::TopologyDefinition (serde names). */
export interface TopologyNode {
  id: string;
  kind?: string;
  label: string;
  prereqs?: string[];
  max_iterations?: number | null;
}
export interface TopologyEdge { from: string; to: string; loop_edge?: boolean }
export interface TopologyDefinition { nodes: TopologyNode[]; edges: TopologyEdge[]; until?: unknown }
export interface Topology { id: string; name: string; definition: TopologyDefinition; created_at?: string; updated_at?: string }

/** Cap on preview lines; the tail becomes an ellipsis row. */
export const PREVIEW_MAX_LINES = 8;

export type PreviewLine =
  | { kind: 'node'; glyph: '▶' | '└─'; label: string; after: string[] }
  | { kind: 'more' }
  | { kind: 'placeholder'; text: string };

/**
 * One line per node in Kahn order (ties by id: the TUI's sorted stack pops
 * the greatest id first), loop edges skipped; the first node gets ▶, the rest
 * └─, each with its prereqs. Never throws: an empty topology, a dangling edge
 * or a non-loop cycle gives the single placeholder line the TUI shows.
 */
export function topologyPreview(def: TopologyDefinition, maxLines = PREVIEW_MAX_LINES): PreviewLine[] {
  if (!def || !Array.isArray(def.nodes) || def.nodes.length === 0) return [{ kind: 'placeholder', text: '[topology preview unavailable — empty]' }];
  const cap = Math.min(maxLines, PREVIEW_MAX_LINES);
  const label = new Map<string, string>(def.nodes.map((n) => [n.id, n.label]));
  const edges = Array.isArray(def.edges) ? def.edges : [];
  for (const e of edges) {
    if (!label.has(e.from) || !label.has(e.to)) return [{ kind: 'placeholder', text: '[topology preview unavailable — dangling edge]' }];
  }
  const indegree = new Map<string, number>(def.nodes.map((n) => [n.id, 0]));
  const outs = new Map<string, string[]>(def.nodes.map((n) => [n.id, []]));
  for (const e of edges) {
    if (e.loop_edge) continue;
    indegree.set(e.to, (indegree.get(e.to) ?? 0) + 1);
    outs.get(e.from)!.push(e.to);
  }
  const queue = [...indegree].filter(([, v]) => v === 0).map(([k]) => k).sort();
  const order: string[] = [];
  const visited = new Set<string>();
  while (queue.length > 0) {
    const id = queue.pop()!;
    if (visited.has(id)) continue;
    visited.add(id);
    order.push(id);
    for (const n of outs.get(id) ?? []) {
      const c = indegree.get(n) ?? 0;
      const left = c > 0 ? c - 1 : 0;
      indegree.set(n, left);
      if (left === 0 && !visited.has(n)) queue.push(n);
    }
  }
  if (order.length !== def.nodes.length) return [{ kind: 'placeholder', text: '[topology preview unavailable — unresolved cycle]' }];
  const lines: PreviewLine[] = [];
  for (let i = 0; i < order.length; i++) {
    if (lines.length + 1 >= cap && i + 1 < order.length) { lines.push({ kind: 'more' }); break; }
    const id = order[i]!;
    const node = def.nodes.find((n) => n.id === id);
    lines.push({ kind: 'node', glyph: i === 0 ? '▶' : '└─', label: label.get(id) ?? '?', after: node?.prereqs ?? [] });
  }
  return lines;
}

/** The plain text of a preview line, as the TUI prints it. */
export function previewText(line: PreviewLine): string {
  switch (line.kind) {
    case 'placeholder': return line.text;
    case 'more': return '  …';
    case 'node': return `${line.glyph} ${line.label}${line.after.length > 0 ? ` (after ${line.after.join(',')})` : ''}`;
  }
}

// ---------- `gR` (RunEpicTopology) ----------

export interface EpicCandidate { id: string; session_kind?: string; workflow_id?: string | null; project_id?: string | null }

export type RunEpicCheck =
  | { kind: 'ready'; topologyId: string; epicId: string; projectId: string | null }
  | { kind: 'refused'; why: string; error: boolean };

/** The TUI's validate_run_epic_topology: an Epic with a topology binding (`workflow_id`). */
export function checkRunEpic(s: EpicCandidate | null): RunEpicCheck {
  if (!s) return { kind: 'refused', why: 'No session selected', error: false };
  if (s.session_kind !== 'Epic') return { kind: 'refused', why: 'Not an Epic — gR only fires on Epic containers', error: true };
  if (!s.workflow_id) return { kind: 'refused', why: 'Epic has no topology binding — set one via gv overlay first', error: true };
  return { kind: 'ready', topologyId: s.workflow_id, epicId: s.id, projectId: s.project_id ?? null };
}

/** ExecuteTopology params (client.rs execute_topology): `inputs` is `{}` for gR, `{issue: N}` for `:topology run`. */
export function executeTopologyParams(topologyId: string, projectId: string | null, inputs: Record<string, unknown>, parentId: string | null): Record<string, unknown> {
  return { topology_id: topologyId, project_id: projectId, inputs, parent_id: parentId };
}

/** Operator text for an ExecuteTopology refusal (overlay/graph.rs explain_execute_topology_error). */
export function explainExecuteTopologyError(error: string): string {
  let hint: string;
  if (error.includes('operator_owner_no_manager')) hint = 'this project has no appointed live project manager to own review and land nodes; appoint one first';
  else if (error.includes('operator_owner_manager_unauthorized')) hint = 'the project manager must hold Automation, have the Epic in scope, be in execute mode and not paused';
  else if (error.includes('operator_owner_epic_required')) hint = 'review and land nodes need a live Epic; run from an Issue under an Epic';
  else if (error.includes('operator_owner_project_mismatch') || error.includes('operator_owner_project_required')) hint = 'the Epic must belong to the Issue\'s project';
  else return `ExecuteTopology failed: ${error}`;
  return `ExecuteTopology refused: ${hint} (${error})`;
}

// ---------- `:topology run <name> [<epic>]` ----------

export function parseTopologyRun(args: string): { name: string; epic: string | null } | { error: string } {
  const usage = 'usage: :topology run <name> [<epic>] (on the focused Issue)';
  const t = args.trim().split(/\s+/).filter(Boolean);
  if (t[0] !== 'run' || !t[1] || t.length > 3) return { error: usage };
  return { name: t[1], epic: t[2] ?? null };
}

export interface EpicSession { id: string; title?: string | null; session_kind?: string; status: string; project_id?: string | null; parent_id?: string | null }

const live = (s: EpicSession) => s.session_kind === 'Epic' && s.status !== 'Archived' && s.status !== 'Deleted';

/**
 * The Epic a topology started on an Issue runs under (choose_topology_epic):
 * the one named (id prefix of 4+ or exact title), else the list cursor's own
 * or enclosing Epic in the Issue's project, else the project's only live Epic.
 */
export function chooseTopologyEpic(sessions: readonly EpicSession[], projectId: string, named: string | null, cursorId: string | null): { epic: string } | { error: string } {
  const epics = sessions.filter((s) => live(s) && s.project_id === projectId);
  const label = (s: EpicSession) => `${s.title ?? 'untitled'} (${s.id.slice(0, 8)})`;
  if (named !== null) {
    const needle = named.toLowerCase();
    const byId = epics.filter((e) => needle.length >= 4 && e.id.startsWith(needle));
    const byTitle = epics.filter((e) => (e.title ?? '').toLowerCase() === needle && e.title);
    if (byId.length === 1) return { epic: byId[0]!.id };
    if (byId.length === 0 && byTitle.length === 1) return { epic: byTitle[0]!.id };
    if (byId.length === 0 && byTitle.length === 0) return { error: `no live Epic "${named}" in this Issue's project; Epics: ${epics.map(label).join(', ')}` };
    return { error: `"${named}" names more than one Epic; use its id` };
  }
  const byId = new Map(sessions.map((s) => [s.id, s]));
  let cur = cursorId;
  for (let i = 0; i < 16 && cur; i++) {
    const s = byId.get(cur);
    if (!s) break;
    if (epics.some((e) => e.id === s.id)) return { epic: s.id };
    cur = s.parent_id ?? null;
  }
  if (epics.length === 1) return { epic: epics[0]!.id };
  if (epics.length === 0) return { error: 'this Issue\'s project has no live Epic to run a topology under' };
  return { error: `name an Epic: :topology run <name> <epic>; Epics: ${epics.map(label).join(', ')}` };
}
