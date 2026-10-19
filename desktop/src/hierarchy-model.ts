// Session hierarchy creation and lead assignment (hub #3000 gap sweep G2),
// pure rules with no DOM: the containment matrix (rsi_common::legal_children),
// tag normalisation (rsi_common::tag::normalize_tag), the create-entity form's
// field visibility, validation and submit request (overlay/create_entity_form.rs)
// and `gL` (action_handler/session.rs SetEpicLead). Unit-tested under
// tests/hierarchy.test.mjs.

export type EntityKind =
  | 'Group' | 'Epic' | 'Standard' | 'Story' | 'Task' | 'Bug' | 'Feature' | 'Refactor' | 'Research';

export const CONTAINER_KINDS: readonly EntityKind[] = ['Group', 'Epic'];
export const LEAF_KINDS: readonly EntityKind[] = ['Standard', 'Story', 'Task', 'Bug', 'Feature', 'Refactor', 'Research'];
/** The order the Kind selector shows. */
export const ALL_KINDS: readonly EntityKind[] = ['Group', 'Epic', 'Story', 'Task', 'Bug', 'Feature', 'Refactor', 'Research', 'Standard'];

export function isContainer(kind: string | null | undefined): boolean {
  return kind === 'Group' || kind === 'Epic';
}

export function isEntityKind(kind: string): kind is EntityKind {
  return (ALL_KINDS as readonly string[]).includes(kind);
}

/** What a parent may hold; `null` is the root (rsi_common::types::legal_children). */
export function legalChildren(parent: string | null | undefined): readonly EntityKind[] {
  switch (parent ?? null) {
    case null: return ['Standard', 'Group'];
    case 'Group': return ['Standard', 'Epic'];
    case 'Epic': return ['Story', 'Task', 'Bug', 'Feature', 'Refactor', 'Research'];
    default: return [];
  }
}

// ---------- the scope a chord creates in ----------

export interface HierSession {
  id: string;
  status?: string;
  session_kind?: string;
  title?: string | null;
  query?: string;
  parent_id?: string | null;
  project_id?: string | null;
  working_dir?: string;
  lead_session_id?: string | null;
}

/**
 * The container a creation chord targets (the TUI's descent head): the
 * cursor row when it is a Group or Epic, else that row's parent container,
 * else the root (null). The desktop tree is inline, so the cursor stands in
 * for the TUI's descended container.
 */
export function scopeParent(row: HierSession | null, byId: (id: string) => HierSession | undefined): HierSession | null {
  if (!row) return null;
  if (isContainer(row.session_kind)) return row;
  const p = row.parent_id ? byId(row.parent_id) : undefined;
  return p && isContainer(p.session_kind) ? p : null;
}

/** The chord's precheck: a toast when the kind cannot live in that scope (null when it can). */
export function createBlock(kind: EntityKind, parent: HierSession | null): string | null {
  if (legalChildren(parent?.session_kind ?? null).includes(kind)) return null;
  const where = parent ? `${parent.session_kind} “${displayName(parent)}”` : 'the top level';
  const hint = kind === 'Epic' ? ' (put the cursor on a Group)'
    : LEAF_KINDS.includes(kind) && kind !== 'Standard' ? ' (put the cursor on an Epic or one of its sessions)' : '';
  return `${kind} not allowed under ${where}${hint}`;
}

export function displayName(s: HierSession): string {
  const t = s.title?.trim();
  if (t) return t;
  const q = (s.query ?? '').split('\n')[0]?.trim() ?? '';
  return q ? q.slice(0, 60) : s.id.slice(0, 8);
}

/** Containers a kind may be created under (the form's parent choices), root first when legal. */
export function parentChoices(kind: EntityKind, sessions: readonly HierSession[]): Array<{ id: string | null; label: string }> {
  const out: Array<{ id: string | null; label: string }> = [];
  if (legalChildren(null).includes(kind)) out.push({ id: null, label: '(top level)' });
  for (const s of sessions) {
    if (!isContainer(s.session_kind) || s.status === 'Archived' || s.status === 'Deleted') continue;
    if (legalChildren(s.session_kind).includes(kind)) out.push({ id: s.id, label: `${s.session_kind}: ${displayName(s)}` });
  }
  return out;
}

// ---------- tags ----------

const TAG_RE = /^[a-z0-9][a-z0-9\-_/]*$/;

/** rsi_common::tag::normalize_tag: trim, lowercase, whitespace runs to `-`, then the charset check. */
export function normalizeTag(input: string): { ok: true; tag: string } | { ok: false; why: string } {
  const trimmed = input.trim();
  if (trimmed === '') return { ok: false, why: 'empty tag' };
  const collapsed = trimmed.toLowerCase().replace(/\s+/g, '-');
  if (!TAG_RE.test(collapsed)) return { ok: false, why: `“${input.trim()}” is not a valid tag (a-z, 0-9, - _ /)` };
  return { ok: true, tag: collapsed };
}

/** A typed tag line split on spaces and commas, normalised and de-duplicated. */
export function parseTags(line: string): { tags: string[]; bad: string[] } {
  const tags: string[] = [];
  const bad: string[] = [];
  for (const raw of line.split(/[\s,]+/)) {
    if (raw === '') continue;
    const r = normalizeTag(raw);
    if (!r.ok) bad.push(raw);
    else if (!tags.includes(r.tag)) tags.push(r.tag);
  }
  return { tags, bad };
}

// ---------- the form ----------

export type EntityField = 'kind' | 'name' | 'tags' | 'parent' | 'topology' | 'provider' | 'model' | 'effort' | 'sandbox' | 'prompt';

/** Kind-driven field visibility (create_entity_form::visibility::field_visibility), plus the leaf prompt. */
export function visibleFields(kind: EntityKind): EntityField[] {
  if (kind === 'Group') return ['kind', 'name', 'tags', 'parent'];
  if (kind === 'Epic') return ['kind', 'name', 'tags', 'parent', 'topology'];
  return ['kind', 'name', 'tags', 'parent', 'provider', 'model', 'effort', 'sandbox', 'prompt'];
}

export const EFFORT_LEVELS: readonly string[] = ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'];

export interface EntityDraft {
  kind: EntityKind;
  name: string;
  tags: string;
  parentId: string | null;
  topologyId: string | null;
  provider: string;
  model: string;
  effort: string;
  sandbox: boolean;
  prompt: string;
}

export function emptyDraft(kind: EntityKind, parentId: string | null, provider = 'Claude'): EntityDraft {
  return { kind, name: '', tags: '', parentId, topologyId: null, provider, model: '', effort: '', sandbox: false, prompt: '' };
}

/** The last successful submit's choices, prefilled on the next open (ModalDefaults). */
export interface EntityDefaults {
  tags?: string;
  topologyId?: string | null;
  provider?: string;
  model?: string;
  effort?: string;
  sandbox?: boolean;
}

/** A fresh draft for a chord: its kind and scope, the remembered defaults for the rest. */
export function draftFor(kind: EntityKind, parentId: string | null, d: EntityDefaults, provider: string): EntityDraft {
  return {
    ...emptyDraft(kind, parentId, d.provider || provider),
    tags: d.tags ?? '',
    topologyId: kind === 'Epic' ? (d.topologyId ?? null) : null,
    model: d.model ?? '',
    effort: d.effort ?? '',
    sandbox: d.sandbox ?? false,
  };
}

export interface CreateContext {
  /** The parent's kind, when a parent is chosen. */
  parentKind: string | null;
  /** The parent's project, else the project the list is filtered to. */
  projectId: string | null;
  /** The working directory a leaf launches in. */
  workingDir: string | null;
}

export type EntityRequest =
  | { command: 'create_container'; args: { kind: 'Group' | 'Epic'; name: string; parentId: string | null; projectId: string | null; tags: string[]; topologyId: string | null } }
  | { command: 'launch_entity'; args: { request: LeafLaunch } };

export interface LeafLaunch {
  kind: EntityKind;
  title: string;
  query: string;
  provider: string;
  model: string | null;
  effort: string | null;
  parent_id: string | null;
  project_id: string | null;
  working_dir: string | null;
  sandbox: boolean;
  tags: string[];
}

/** Validate a draft and build its submit (submit_create_entity_form), or say what is wrong. */
export function entityRequest(d: EntityDraft, ctx: CreateContext): { ok: true; request: EntityRequest } | { ok: false; why: string } {
  const name = d.name.trim();
  if (name === '') return { ok: false, why: 'Name required' };
  if (/[\p{Cc}]/u.test(name)) return { ok: false, why: 'The name must be one line' };
  const { tags, bad } = parseTags(d.tags);
  if (bad.length > 0) return { ok: false, why: 'fix invalid tags before submitting' };
  if (tags.length === 0) return { ok: false, why: 'at least one tag required' };
  if (!legalChildren(ctx.parentKind).includes(d.kind)) {
    return { ok: false, why: `${d.kind} not allowed under ${ctx.parentKind ?? 'root'}` };
  }
  if (d.kind === 'Group' || d.kind === 'Epic') {
    return {
      ok: true,
      request: {
        command: 'create_container',
        args: {
          kind: d.kind, name, parentId: d.parentId, projectId: ctx.projectId, tags,
          // Topology is Epic-only: a Group always passes none (the TUI's defensive gate).
          topologyId: d.kind === 'Epic' ? d.topologyId : null,
        },
      },
    };
  }
  if (d.prompt.trim() === '') return { ok: false, why: 'A leaf session needs a prompt to start with' };
  if (!ctx.workingDir) return { ok: false, why: 'No working directory: pick a parent or a project with a folder' };
  const effort = d.effort.trim();
  if (effort && !EFFORT_LEVELS.includes(effort)) return { ok: false, why: `unknown effort ${effort}` };
  return {
    ok: true,
    request: {
      command: 'launch_entity',
      args: {
        request: {
          kind: d.kind, title: name, query: d.prompt, provider: d.provider,
          model: d.model.trim() || null, effort: effort || null,
          parent_id: d.parentId, project_id: ctx.projectId, working_dir: ctx.workingDir,
          sandbox: d.sandbox, tags,
        },
      },
    },
  };
}

/** The working directory a leaf is created in (creation_working_dir): the project's folder, else the parent's, else the open session's. */
export function creationWorkingDir(projectPath: string | null | undefined, parent: HierSession | null, open: HierSession | null): string | null {
  const p = projectPath?.trim();
  if (p) return p;
  if (parent?.working_dir?.trim()) return parent.working_dir;
  if (open?.working_dir?.trim()) return open.working_dir;
  return null;
}

// ---------- gL ----------

/** Why `gL` cannot make `target` its Epic's lead, else null (SetEpicLead's checks). */
export function leadBlock(target: HierSession | null, parent: HierSession | undefined): string | null {
  if (!target) return 'No session selected';
  if (isContainer(target.session_kind)) return 'A Group or Epic cannot lead an Epic; pick a leaf session';
  if (!target.parent_id) return 'Session has no parent Epic';
  if (!parent || parent.session_kind !== 'Epic') return 'Parent is not an Epic';
  return null;
}
