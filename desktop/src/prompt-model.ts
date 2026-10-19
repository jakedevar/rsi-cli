// The launch / continue prompts and the input modal (gap sweep G4, hub #3000),
// pure (no DOM): the prompt-only chords of docs/keybindings.md "New Session
// Prompt", "New Session Settings", "Continue Prompt" and "Input Modal", and
// the New session prompt's settings side (crates/rsi/src/overlay/prompt.rs,
// launch_settings.rs). The DOM side is prompts.ts; tests/prompts.test.mjs.
import { effortLadder } from './detail-model.js';

export type PromptKind = 'launch' | 'continue' | 'modal';

export type PromptCmd =
  | 'close' | 'settings' | 'model' | 'effort' | 'sandbox' | 'launch-tab' | 'launch-split'
  | 'compile' | 'grammar' | 'help' | 'stack' | 'stack-focus' | 'send' | 'arm' | 'pass';

export interface PromptCtx {
  kind: PromptKind;
  /** The text field is in vim normal mode (Vim keys on). */
  normal: boolean;
  /** A vim command is half typed (`f`, `d`, a count): `?` completes it. */
  pending: boolean;
  /** `Space` was pressed in normal mode (the `Space N` leader). */
  leader: boolean;
  /** Enter sends (the input bar's Submit on Enter setting; the input modal follows it). */
  submitOnEnter: boolean;
}

export interface KeyLike { key: string; ctrlKey?: boolean; shiftKey?: boolean; altKey?: boolean; metaKey?: boolean }

/**
 * A prompt chord, or `pass` for the field (vim, typing, G1's global keys). The
 * Ctrl chords are physical Ctrl, as the TUI's (Cmd stays the platform's).
 */
export function promptKey(ctx: PromptCtx, k: KeyLike): PromptCmd {
  const ctrl = !!k.ctrlKey && !k.metaKey && !k.altKey;
  const key = k.key.length === 1 ? k.key.toLowerCase() : k.key;
  if (ctx.leader) return !k.ctrlKey && !k.metaKey && !k.altKey && (k.key === 'N' || k.key === 'n') ? 'stack' : 'pass';
  if (ctrl && !k.shiftKey) {
    switch (key) {
      case 'q': return 'close';
      case 'y': return 'compile';
      case 'o': return ctx.kind === 'launch' ? 'settings' : 'pass';
      case 'm': return ctx.kind === 'launch' ? 'model' : 'pass';
      case 'e': return ctx.kind === 'launch' ? 'effort' : 'pass';
      case 'b': return ctx.kind === 'launch' ? 'sandbox' : 'pass';
      case 't': return ctx.kind === 'launch' ? 'launch-tab' : 'pass';
      case 's': return ctx.kind === 'launch' ? 'launch-split' : 'pass';
      case 'j': case 'k': return ctx.kind === 'launch' ? 'stack-focus' : 'pass';
      default: return 'pass';
    }
  }
  if (ctrl && k.shiftKey && key === 'g') return 'grammar';
  if (k.ctrlKey || k.metaKey || k.altKey) return 'pass';
  if (ctx.kind === 'modal' && k.key === 'Enter' && !k.shiftKey && ctx.submitOnEnter && !ctx.normal) return 'send';
  if (!ctx.normal || ctx.pending) return 'pass';
  if (k.key === '?') return 'help';
  if (k.key === 'Tab' && !k.shiftKey && ctx.kind === 'launch') return 'settings';
  if (k.key === ' ' && ctx.kind === 'launch') return 'arm';
  return 'pass';
}

// ---------- the settings side ----------

export type PolicyPreset = 'observe' | 'execute' | 'full';
export const POLICY_PRESETS: readonly PolicyPreset[] = ['observe', 'execute', 'full'];
export const PRESET_LABELS: Readonly<Record<PolicyPreset, string>> = { observe: 'Observe', execute: 'Execute', full: 'Full project control' };

export type ScopeChoice = { kind: 'project' } | { kind: 'group' | 'epic'; id: string; label: string };

export interface LaunchSettings {
  provider: string;
  model: string;
  /** "" is the model default. */
  effort: string;
  sandbox: boolean;
  /** A planned appointment (blank prompts with a project). */
  manager: { scope: ScopeChoice; preset: PolicyPreset } | null;
}

export type SettingRow = 'model' | 'effort' | 'sandbox' | 'manager' | 'scope' | 'policy' | 'grant';

export const ROW_LABELS: Readonly<Record<SettingRow, string>> = {
  model: 'Model', effort: 'Effort', sandbox: 'Sandbox', manager: 'Manager', scope: 'Scope', policy: 'Policy', grant: 'Grant',
};

/** One-line help under the rows (`LaunchSettingRow::description`). */
export const ROW_HELP: Readonly<Record<SettingRow, string>> = {
  model: 'Provider and model for this launch only.',
  effort: 'Reasoning effort for this launch (the model’s ladder).',
  sandbox: 'Run in its own git worktree on an rsi/<session> branch.',
  manager: 'Appoint this session as the project manager at launch.',
  scope: 'What the manager supervises: project, Group or Epic.',
  policy: 'The grant preset: Observe, Execute or Full project control.',
  grant: 'The preset’s policy is granted at launch; p opens the full policy editor once the manager is appointed.',
};

/** The rows shown, top to bottom (`launch_setting_rows`). */
export function settingRows(s: LaunchSettings): SettingRow[] {
  const rows: SettingRow[] = ['model', 'effort', 'sandbox', 'manager'];
  if (s.manager) rows.push('scope', 'policy', 'grant');
  return rows;
}

export type SettingsCmd =
  | { kind: 'move'; by: 1 | -1 }
  | { kind: 'model' }
  | { kind: 'effort'; by: 1 | -1 }
  | { kind: 'sandbox' }
  | { kind: 'manager' }
  | { kind: 'scope'; by: 1 | -1 }
  | { kind: 'policy'; by: 1 | -1 }
  | { kind: 'grant' }
  | { kind: 'back' }
  | { kind: 'none' };

/** A key on the settings side (after promptKey took the chords both sides share). */
export function settingsKey(row: SettingRow, k: KeyLike): SettingsCmd {
  if (k.ctrlKey || k.metaKey || k.altKey) return { kind: 'none' };
  switch (k.key) {
    case 'j': case 'ArrowDown': return { kind: 'move', by: 1 };
    case 'k': case 'ArrowUp': return { kind: 'move', by: -1 };
    case 'Escape': case 'Tab': return { kind: 'back' };
    case 'p': return { kind: 'grant' };
    default: break;
  }
  const left = k.key === 'h' || k.key === 'ArrowLeft';
  const right = k.key === 'l' || k.key === 'ArrowRight';
  const act = k.key === ' ' || k.key === 'Enter';
  if (!left && !right && !act) return { kind: 'none' };
  const by: 1 | -1 = left ? -1 : 1;
  switch (row) {
    case 'model': return { kind: 'model' };
    case 'effort': return { kind: 'effort', by };
    case 'sandbox': return { kind: 'sandbox' };
    case 'manager': return { kind: 'manager' };
    case 'scope': return { kind: 'scope', by };
    case 'policy': return { kind: 'policy', by };
    case 'grant': return act ? { kind: 'grant' } : { kind: 'none' };
  }
}

/** Efforts the launch accepts (the desktop's LaunchSession check). */
export const LAUNCH_EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'];

/** Ctrl-E / the Effort row: the model default first, then its ladder, wrapping. */
export function stepEffort(model: string, current: string, by: 1 | -1): string {
  const ladder = effortLadder(model).filter((e) => LAUNCH_EFFORTS.includes(e));
  const steps = ['', ...(ladder.length ? ladder : LAUNCH_EFFORTS)];
  const i = steps.indexOf(current);
  return steps[((i < 0 ? 0 : i) + by + steps.length) % steps.length]!;
}

/** Effort bars for the header: one per ladder step, filled to the level. */
export function effortBars(model: string, effort: string): string {
  const ladder = effortLadder(model).filter((e) => LAUNCH_EFFORTS.includes(e));
  const steps = ladder.length ? ladder : LAUNCH_EFFORTS;
  const at = steps.indexOf(effort);
  return steps.map((_, i) => (i <= at ? '▮' : '▯')).join('');
}

interface ScopeSession { id: string; session_kind?: string | null; parent_id?: string | null; project_id?: string | null; status: string; title?: string | null; name?: string | null }

const live = (s: ScopeSession, projectId: string): boolean => s.project_id === projectId && s.status !== 'Archived' && s.status !== 'Deleted';
const nameOf = (s: ScopeSession): string => (s.name ?? s.title ?? s.id).toString();

/** Whole project, then each live Group (by name) followed by its live Epics (`manager_scope_choices`). */
export function scopeChoices(sessions: readonly ScopeSession[], projectId: string): ScopeChoice[] {
  const sorted = (rows: ScopeSession[]) => rows.sort((a, b) => nameOf(a).toLowerCase().localeCompare(nameOf(b).toLowerCase()) || (a.id < b.id ? -1 : 1));
  const out: ScopeChoice[] = [{ kind: 'project' }];
  for (const g of sorted(sessions.filter((s) => s.session_kind === 'Group' && live(s, projectId)))) {
    out.push({ kind: 'group', id: g.id, label: nameOf(g) });
    for (const e of sorted(sessions.filter((s) => s.session_kind === 'Epic' && s.parent_id === g.id && live(s, projectId)))) {
      out.push({ kind: 'epic', id: e.id, label: nameOf(e) });
    }
  }
  return out;
}

export const scopeKey = (c: ScopeChoice): string => (c.kind === 'project' ? 'project' : `${c.kind}:${c.id}`);

export function scopeLabel(c: ScopeChoice): string {
  return c.kind === 'project' ? 'Whole project' : `${c.kind === 'group' ? 'Group' : 'Epic'} ${c.label}`;
}

export function stepScope(choices: readonly ScopeChoice[], current: ScopeChoice, by: 1 | -1): ScopeChoice {
  if (choices.length === 0) return current;
  const i = choices.findIndex((c) => scopeKey(c) === scopeKey(current));
  return choices[((i < 0 ? 0 : i) + by + choices.length) % choices.length]!;
}

export function stepPreset(p: PolicyPreset, by: 1 | -1): PolicyPreset {
  const i = POLICY_PRESETS.indexOf(p);
  return POLICY_PRESETS[(i + by + POLICY_PRESETS.length) % POLICY_PRESETS.length]!;
}

/** The header row of a New session prompt: working dir, model, effort, sandbox, manager. */
export function launchSummary(s: LaunchSettings, workingDir: string): string {
  const parts = [workingDir || '(no working directory)', s.model ? `${s.provider} · ${s.model}` : `${s.provider} · default model`];
  parts.push(`${effortBars(s.model, s.effort)} ${s.effort || 'default effort'}`);
  parts.push(s.sandbox ? '⊡ sandbox' : '⊡ no sandbox');
  if (s.manager) parts.push(`★ manager · ${PRESET_LABELS[s.manager.preset]}`);
  return parts.join('  ');
}

/** The session id in a LaunchSession answer (a Session, or `{session_id}`), else null. */
export function launchedId(result: unknown): string | null {
  if (!result || typeof result !== 'object') return null;
  const r = result as { id?: unknown; session_id?: unknown; session?: { id?: unknown } };
  const id = r.id ?? r.session_id ?? r.session?.id;
  return typeof id === 'string' && /^[0-9a-f-]{36}$/i.test(id) ? id.toLowerCase() : null;
}
