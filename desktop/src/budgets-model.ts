// Settings › Safety & Spend › Budgets and the budget policy form (gap sweep
// G4, hub #3000), pure (no DOM): crates/rsi/src/overlay/budget_policy_form.rs
// keys and fields, and the list's row text. The DOM side is budgets.ts; the
// policy is validated and sent by src-tauri/src/budgets.rs.

/** The form's scope kinds and model tiers in their cycle order (model_control_budgets.rs). */
export const SCOPE_KINDS = [
  'global', 'provider', 'project', 'session', 'tree', 'workflow', 'subsystem', 'retry', 'scheduled_job', 'issue_tracker',
  'recursive_graph', 'operator',
] as const;
export const MODEL_TIERS = ['any', 'local', 'standard', 'premium'] as const;

export type BudgetField =
  | 'scope_kind' | 'scope_id' | 'purpose' | 'model_tier' | 'max_total_tokens' | 'max_concurrency'
  | 'max_calls_per_window' | 'rate_window_seconds' | 'alert_threshold_ratio';

/** Tab order (FIELD_SCOPE_KIND .. FIELD_ALERT_THRESHOLD_RATIO). */
export const FIELDS: readonly BudgetField[] = [
  'scope_kind', 'scope_id', 'purpose', 'model_tier', 'max_total_tokens', 'max_concurrency', 'max_calls_per_window',
  'rate_window_seconds', 'alert_threshold_ratio',
];

export const FIELD_LABELS: Readonly<Record<BudgetField, string>> = {
  scope_kind: 'Scope kind', scope_id: 'Scope id', purpose: 'Purpose', model_tier: 'Model tier',
  max_total_tokens: 'Max total tokens', max_concurrency: 'Max concurrency', max_calls_per_window: 'Max calls per window',
  rate_window_seconds: 'Rate window (s)', alert_threshold_ratio: 'Alert threshold ratio',
};

const NUMERIC: ReadonlySet<BudgetField> = new Set(['max_total_tokens', 'max_concurrency', 'max_calls_per_window', 'rate_window_seconds']);

export type BudgetForm = Record<BudgetField, string>;

/** A policy as rsid sends it (the fields the form and the list show). */
export interface BudgetPolicy {
  scope_kind: string;
  scope_id?: string | null;
  purpose?: string | null;
  model_tier?: string | null;
  max_total_tokens?: number | null;
  max_concurrency?: number | null;
  max_calls_per_window?: number | null;
  rate_window_seconds?: number | null;
  alert_threshold_ratio?: number | null;
  max_calls?: number | null;
  max_retries?: number | null;
  max_wall_time_ms?: number | null;
  [k: string]: unknown;
}

const str = (v: unknown): string => (v === null || v === undefined ? '' : String(v));

/** The form for a new policy (`None`) or one being edited. */
export function formFor(p: BudgetPolicy | null): BudgetForm {
  if (!p) {
    return {
      scope_kind: 'global', scope_id: '', purpose: '', model_tier: 'any', max_total_tokens: '', max_concurrency: '',
      max_calls_per_window: '', rate_window_seconds: '', alert_threshold_ratio: '',
    };
  }
  return {
    scope_kind: (SCOPE_KINDS as readonly string[]).includes(p.scope_kind) ? p.scope_kind : 'global',
    scope_id: p.scope_kind === 'global' ? '' : str(p.scope_id),
    purpose: str(p.purpose),
    model_tier: p.model_tier && (MODEL_TIERS as readonly string[]).includes(p.model_tier) ? p.model_tier : 'any',
    max_total_tokens: str(p.max_total_tokens), max_concurrency: str(p.max_concurrency),
    max_calls_per_window: str(p.max_calls_per_window), rate_window_seconds: str(p.rate_window_seconds),
    alert_threshold_ratio: str(p.alert_threshold_ratio),
  };
}

export interface FormState { form: BudgetForm; field: number }

export type FormEffect = { kind: 'none' } | { kind: 'save' } | { kind: 'close' };

function cycle<T extends string>(list: readonly T[], cur: string, by: 1 | -1): T {
  const i = Math.max(0, list.indexOf(cur as T));
  return list[(i + by + list.length) % list.length]!;
}

/**
 * One key of the form: Tab / Shift-Tab cycle fields, Up / Down cycle the
 * scope kind or model tier, typing appends (digits only in the numeric
 * fields; the ratio also takes one `.`), Backspace deletes (not on the cycle
 * fields), Enter saves, Esc closes.
 */
export function budgetFormKey(st: FormState, k: { key: string; shiftKey?: boolean; ctrlKey?: boolean; metaKey?: boolean; altKey?: boolean }): { st: FormState; effect: FormEffect } {
  const none = (s: FormState) => ({ st: s, effect: { kind: 'none' } as FormEffect });
  const field = FIELDS[st.field]!;
  if (k.key === 'Enter') return { st, effect: { kind: 'save' } };
  if (k.key === 'Escape') return { st, effect: { kind: 'close' } };
  if (k.key === 'Tab') return none({ ...st, field: (st.field + (k.shiftKey ? FIELDS.length - 1 : 1)) % FIELDS.length });
  if (k.ctrlKey || k.metaKey || k.altKey) return none(st);
  if (k.key === 'ArrowUp' || k.key === 'ArrowDown') {
    const by = k.key === 'ArrowDown' ? 1 : -1;
    if (field === 'scope_kind') return none({ ...st, form: { ...st.form, scope_kind: cycle(SCOPE_KINDS, st.form.scope_kind, by) } });
    if (field === 'model_tier') return none({ ...st, form: { ...st.form, model_tier: cycle(MODEL_TIERS, st.form.model_tier, by) } });
    return none(st);
  }
  if (field === 'scope_kind' || field === 'model_tier') return none(st);
  if (k.key === 'Backspace') return none({ ...st, form: { ...st.form, [field]: st.form[field].slice(0, -1) } });
  if ([...k.key].length !== 1) return none(st);
  if (NUMERIC.has(field) && !/^[0-9]$/.test(k.key)) return none(st);
  if (field === 'alert_threshold_ratio' && !(/^[0-9]$/.test(k.key) || (k.key === '.' && !st.form[field].includes('.')))) return none(st);
  return none({ ...st, form: { ...st.form, [field]: st.form[field] + k.key } });
}

/** One list row: `scope/id · tier · purpose · limits`. */
export function policyLine(p: BudgetPolicy): string {
  const parts = [`${p.scope_kind}/${p.scope_id ?? '*'}`];
  if (p.model_tier) parts.push(p.model_tier);
  if (p.purpose) parts.push(p.purpose);
  const lim: string[] = [];
  if (p.max_total_tokens != null) lim.push(`${p.max_total_tokens} tokens`);
  if (p.max_concurrency != null) lim.push(`${p.max_concurrency} concurrent`);
  if (p.max_calls != null) lim.push(`${p.max_calls} calls`);
  if (p.max_calls_per_window != null) lim.push(`${p.max_calls_per_window} calls / ${p.rate_window_seconds ?? '?'} s`);
  if (p.max_retries != null) lim.push(`${p.max_retries} retries`);
  if (p.max_wall_time_ms != null) lim.push(`${p.max_wall_time_ms} ms`);
  if (p.alert_threshold_ratio != null) lim.push(`alert at ${Math.round(p.alert_threshold_ratio * 100)}%`);
  if (lim.length) parts.push(lim.join(', '));
  return parts.join(' · ');
}

/** What the Budgets list can do; each button and each key runs one of these (local #208). */
export type BudgetAction = 'add' | 'edit' | 'delete' | 'refresh' | 'close' | 'down' | 'up';

/** The Budgets list's buttons, in order: label, the key shown in the label, and the action both run. */
export const BUDGET_BUTTONS: readonly { readonly action: BudgetAction; readonly label: string; readonly key: string }[] = [
  { action: 'add', label: 'Add policy', key: 'a' },
  { action: 'edit', label: 'Edit', key: 'Enter' },
  { action: 'delete', label: 'Delete', key: 'd' },
  { action: 'refresh', label: 'Refresh', key: 'R' },
  { action: 'close', label: 'Close', key: 'q' },
];

/** The Budgets list's keys: j/k and arrows move, Enter / Space edit, a add, d delete, R refresh, q close. */
export function budgetListKey(key: string): BudgetAction | null {
  switch (key) {
    case 'j': case 'ArrowDown': return 'down';
    case 'k': case 'ArrowUp': return 'up';
    case 'Enter': case ' ': return 'edit';
    case 'a': return 'add';
    case 'd': return 'delete';
    case 'R': return 'refresh';
    case 'q': return 'close';
    default: return null;
  }
}

/** A button label with its key in secondary text ("Add (a)"); the key-hints pass drops a Vim-only "(a)" with Vim keys off. */
export function budgetButtonLabel(b: { readonly label: string; readonly key: string }): string {
  return b.key === 'Enter' ? b.label : `${b.label} (${b.key})`;
}
