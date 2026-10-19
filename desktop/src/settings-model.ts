// The settings pane's pure model (no DOM), mirroring the TUI's settings page
// (crates/rsi/src/settings_registry.rs, settings_staging.rs and
// action_handler/daemon_config.rs). Tested in tests/settings.test.mjs.
//
// - Rows come from the generated registry (settings.generated.ts): the same
//   groups, sections, labels, summaries and apply classes as the TUI.
// - Daemon rows read `GetDaemonConfig` and stage edits locally; a keypress only
//   changes the staged copy. Apply sends ONE `UpdateDaemonConfig {updates}`
//   with every changed field's final value (all or none take effect), then
//   re-reads the config. Discard drops the staged copy.
// - A staged value is pending only while it differs from the live one, so
//   stepping back to where a row started clears its marker.
// - Rows the desktop cannot edit yet (other RPC families, files, the TUI's own
//   state.json) are `tui-only` with the reason (no rsid state exists for them);
//   rows another desktop view serves open it (`overlay`); credential values
//   are never shown. No rsid-backed row is a placeholder (local #125).
import { MODEL_CONTROL_MODES, modeMessage } from './settings-state-model.js';
import {
  APPLY_LABELS, DAEMON_WIDGETS, SETTINGS_GROUPS, SETTINGS_ROWS, SETTINGS_SECTIONS, TEXT_EDITORS,
  type ApplyKind, type GroupRow, type SectionRow, type SettingRow, type TextEditorRow,
} from './settings.generated.js';

export type Json = boolean | number | string | null;
export type Config = Readonly<Record<string, unknown>>;

// ---------- credentials ----------

/**
 * True for a config field whose value may be a credential. The daemon's
 * runtime config holds none today; this is the guard that keeps one from
 * ever being shown if it appears (the Rust side redacts the same names).
 */
export function isSecretField(field: string): boolean {
  const f = field.toLowerCase();
  if (/(^|[._])(api_?key|secret|password|passphrase|credential|private_key|bearer|auth_token|access_token|refresh_token|session_token)s?($|[._])/.test(f)) return true;
  return /(^|[._])token$/.test(f);
}

export const REDACTED = '[redacted]';

/** A URL with its user-info (`user:pass@`) masked. */
export function maskUrlCredentials(text: string): string {
  return text.replace(/^([a-z][a-z0-9+.-]*:\/\/)[^/@\s]+@/i, `$1${REDACTED}@`);
}

/** The config as the pane may show it: secret fields and URL credentials masked. */
export function redactConfig(config: Config): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(config)) {
    if (isSecretField(k)) out[k] = v === null || v === '' ? v : REDACTED;
    else out[k] = typeof v === 'string' ? maskUrlCredentials(v) : v;
  }
  return out;
}

// ---------- values ----------

/** A config value as a choice option's text (the TUI's `update_from_json`). */
export function optionText(v: unknown): string {
  if (v === null || v === undefined) return 'Default';
  if (typeof v === 'string') return v;
  if (typeof v === 'number' || typeof v === 'boolean') return String(v);
  return '';
}

/** The JSON a choice option sends (the TUI's `daemon_cycle_value_json`). */
export function cycleValueJson(option: string): Json {
  if (option === 'Default') return null;
  if (/^[+-]?\d+$/.test(option)) {
    const n = Number(option);
    if (Number.isSafeInteger(n)) return n;
  }
  if (option.trim() !== '' && !Number.isNaN(Number(option)) && /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(option)) {
    return Number(option);
  }
  return option;
}

/** Display text for any config value. */
export function displayValue(v: unknown): string {
  if (v === null || v === undefined) return '';
  if (typeof v === 'string') return v;
  if (typeof v === 'number' || typeof v === 'boolean') return String(v);
  if (typeof v === 'object' && Object.keys(v as object).length === 0) return '';
  return JSON.stringify(v);
}

export function sameJson(a: unknown, b: unknown): boolean {
  return JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
}

// ---------- stepping (settings_staging.rs) ----------

export type Edge = 'clamp' | 'wrap';
export type Step = 'activate' | 'next' | 'previous';

/** Numeric option lists (a leading "Default" allowed) clamp; everything else wraps. */
export function edgeForOptions(options: readonly string[]): Edge {
  let numeric = 0;
  for (const o of options) {
    if (o === 'Default') continue;
    if (o.trim() === '' || Number.isNaN(Number(o))) return 'wrap';
    numeric++;
  }
  return numeric > 0 ? 'clamp' : 'wrap';
}

/** Move `index` one place through `len` options; an unknown index enters at the matching end. */
export function stepIndex(index: number, len: number, forward: boolean, edge: Edge): number {
  if (len === 0) return 0;
  if (index < 0 || index >= len) return forward ? 0 : len - 1;
  if (edge === 'wrap') return forward ? (index + 1) % len : (index + len - 1) % len;
  return forward ? Math.min(index + 1, len - 1) : Math.max(index - 1, 0);
}

/**
 * A choice row's options with the live value: a numeric list gains a live
 * number it does not offer, in order (the TUI's `update_from_json`).
 */
export function choiceOptions(options: readonly string[], live: unknown): string[] {
  const out = [...options];
  const cur = optionText(live);
  const allNumeric = out.every((o) => /^\d+$/.test(o));
  if (allNumeric && /^\d+$/.test(cur) && !out.includes(cur)) {
    out.push(cur);
    out.sort((a, b) => Number(a) - Number(b));
  }
  return out;
}

// ---------- rows ----------

export type Widget =
  | { type: 'switch' }
  | { type: 'choice'; options: readonly string[] }
  /** A daemon field typed as text and staged (a model id, a choice the TUI builds at run time). */
  | { type: 'text'; valueType: 'string' | 'number' }
  /** The TUI's Daemon Text Setting editor: writes on submit, "" for the default. */
  | { type: 'text-editor'; editor: TextEditorRow }
  | { type: 'display' }
  /** Desktop-local rows the DOM side owns. */
  | { type: 'local'; local: 'theme' | 'roles' | 'reset-theme' | 'vim-keys' | 'text-bg' | 'text-bg-color' | 'budgets' | 'provider-keys' }
  /** A row an rsid-backed desktop view serves: Enter opens it or runs its action (settings-state.ts, the slice overlays). */
  | { type: 'overlay' }
  /** A TUI-local presentation preference (the TUI's own state.json): no rsid state, so no desktop control. */
  | { type: 'tui-only'; why: string }
  /** A field the pane cannot show (a credential value); no registry row uses it today. */
  | { type: 'partial'; why: string };

export interface PaneRow {
  /** Registry id, or `desktop_*` for a desktop-only row. */
  id: string;
  label: string;
  summary: string;
  detail?: string;
  keywords: readonly string[];
  section: string;
  /** The daemon field it reads and writes, if any. */
  field: string | null;
  widget: Widget;
  apply: string;
  destructive: boolean;
}

const TEXT_BY_FIELD: ReadonlyMap<string, TextEditorRow> = new Map(TEXT_EDITORS.map((t) => [t.field, t]));

/**
 * Rows another desktop view serves with a real control over the same rsid
 * operation the TUI row uses (registered in settings-state.ts, actions/
 * operatorconfig.ts and actions/promptcreator-run.ts; tests/settings-state
 * checks each id is registered). Enter opens the view or runs the action.
 */
export const OVERLAY_ROW_IDS: ReadonlySet<string> = new Set([
  // operator config files (actions/operatorconfig.ts)
  'claude_hooks', 'api_providers', 'signal_bridge', 'imessage_bridge',
  // skills list, preview and e/d over rsid (actions/promptcreator-run.ts, #97)
  'claude_skills',
  // daemon-state rows (settings-state.ts)
  'emergency_stop', 'usage_stats', 'usage_actions', 'source_worktree_settlement', 'legacy_scratch_adoption',
  'cold_archive_status', 'cold_archive_last_run', 'cold_archive_run_now',
  'sandbox_storage_status', 'preview_reclaim', 'reclaim_now', 'target_pool_status',
  'satellite_registry', 'remote_access', 'mcp_server_configurations',
]);

/** The daemon field the model-control mode row stages; it is written by its own RPC, not UpdateDaemonConfig. */
export const MODE_FIELD = 'model_control_mode';

/** Why a TUI-owned preference has no desktop control (shown on the row and mirrored in parity-audit.json). */
export const TUI_ONLY_WHY = "A TUI presentation preference kept in the TUI's own state.json; rsid has no state for it. The desktop has its own appearance settings.";

/** The reason a row the pane cannot drive stays a read-only notice; no registry row reaches it today (tests/settings-state.test.mjs). */
function partialWhy(spec: SettingRow): string {
  return `${spec.label} has no desktop control.`;
}

/** Which widget the desktop gives a registry row. */
export function widgetFor(spec: SettingRow, live: unknown): Widget {
  if (spec.owner.kind === 'tui') {
    if (spec.id === 'built_in_theme') return { type: 'local', local: 'theme' };
    if (spec.id === 'theme_roles') return { type: 'local', local: 'roles' };
    if (spec.id === 'reset_theme') return { type: 'local', local: 'reset-theme' };
    // Gap sweep G4: the text-area background (textbg.ts) is a desktop preference.
    if (spec.id === 'text_area_background') return { type: 'local', local: 'text-bg' };
    if (spec.id === 'background_color') return { type: 'local', local: 'text-bg-color' };
    if (OVERLAY_ROW_IDS.has(spec.id)) return { type: 'overlay' };
    return { type: 'tui-only', why: TUI_ONLY_WHY };
  }
  if (OVERLAY_ROW_IDS.has(spec.id)) return { type: 'overlay' };
  // The mode is written by UpdateModelControlPolicy (the TUI stages it and Apply sends it).
  if (spec.id === 'model_control_mode') return { type: 'choice', options: MODEL_CONTROL_MODES };
  // Gap sweep G4: the Budgets list (budgets.ts) and the Provider Keys slots (the admin overlay).
  if (spec.id === 'budget_policies') return { type: 'local', local: 'budgets' };
  if (spec.id === 'provider_credential_slots') return { type: 'local', local: 'provider-keys' };
  if (spec.owner.kind === 'daemon_state' && spec.kind === 'read_only') {
    // A read-only summary the config carries (e.g. `cloud_spend_status`).
    return spec.id === 'cloud_spend_status' ? { type: 'display' } : { type: 'partial', why: partialWhy(spec) };
  }
  if (spec.owner.kind === 'daemon_fields') return { type: 'display' };
  if (spec.owner.kind !== 'daemon') return { type: 'partial', why: partialWhy(spec) };
  const field = spec.owner.args?.[0] ?? '';
  if (isSecretField(field)) return { type: 'partial', why: 'A credential field: its value is never shown.' };
  const text = TEXT_BY_FIELD.get(field);
  if (text) return { type: 'text-editor', editor: text };
  const w = DAEMON_WIDGETS[field];
  if (spec.kind === 'read_only') return { type: 'display' };
  if (spec.kind === 'action' || !w) return { type: 'partial', why: partialWhy(spec) };
  if (w.widget === 'switch') return { type: 'switch' };
  if (w.widget === 'choice') return { type: 'choice', options: w.options };
  if (typeof live === 'object' && live !== null) return { type: 'partial', why: 'A structured value; edit it in the TUI.' };
  return { type: 'text', valueType: typeof live === 'number' ? 'number' : 'string' };
}

export function applyLabel(spec: { kind: string; apply: ApplyKind }): string {
  return spec.kind === 'action' ? 'when activated' : APPLY_LABELS[spec.apply] ?? spec.apply;
}

/** The desktop-only row the Vim keys toggle lives in (Workspace › Editing Mode). */
export const VIM_KEYS_ROW: Omit<PaneRow, 'widget'> & { widget: Widget } = {
  id: 'desktop_vim_keys',
  label: 'Vim keys (this desktop)',
  summary: 'Drives the desktop with the TUI\'s modal keys while focus is not in a text field. Kept on this computer; the daemon\'s Editing mode row above is the TUI\'s choice.',
  keywords: ['vim', 'keys', 'modal', 'desktop'],
  section: 'editing_mode',
  field: null,
  widget: { type: 'local', local: 'vim-keys' },
  apply: APPLY_LABELS.immediate,
  destructive: false,
};

/** Every row of a section in page order, with its widget for the live config. */
export function sectionRows(section: string, config: Config): PaneRow[] {
  const rows: PaneRow[] = SETTINGS_ROWS.filter((r) => r.section === section).map((spec) => {
    const field = spec.owner.kind === 'daemon' ? spec.owner.args?.[0] ?? null : spec.id === MODE_FIELD ? MODE_FIELD : null;
    return {
      id: spec.id, label: spec.label, summary: spec.summary, ...(spec.detail ? { detail: spec.detail } : {}),
      keywords: spec.keywords, section: spec.section, field,
      widget: widgetFor(spec, field ? config[field] : undefined),
      apply: applyLabel(spec), destructive: spec.destructive,
    };
  });
  if (section === VIM_KEYS_ROW.section) rows.push({ ...VIM_KEYS_ROW });
  return rows;
}

export function sectionsOf(group: string): SectionRow[] {
  return SETTINGS_SECTIONS.filter((s) => s.group === group);
}

export function groupOf(section: string): GroupRow | undefined {
  const s = SETTINGS_SECTIONS.find((x) => x.id === section);
  return s ? SETTINGS_GROUPS.find((g) => g.id === s.group) : undefined;
}

/** The value a row shows: for a daemon row, the staged value when one is pending. */
export function rowValue(row: PaneRow, config: Config, staging: Staging): unknown {
  if (row.id === 'cloud_spend_status') return config.cloud_spend_status;
  if (row.widget.type === 'display' && !row.field) {
    const spec = SETTINGS_ROWS.find((r) => r.id === row.id);
    const fields = spec?.owner.kind === 'daemon_fields' ? spec.owner.args ?? [] : [];
    return fields.map((f) => `${f}=${displayValue(config[f]) || 'default'}`).join(' · ');
  }
  if (!row.field) return undefined;
  return viewValue(staging, row.field, config[row.field]);
}

/** Can a row's value be stepped with Enter / Space / + / -? */
export function isSteppable(row: PaneRow): boolean {
  return row.widget.type === 'switch' || row.widget.type === 'choice';
}

/** The next value of a switch or choice row from `current` (undefined when it does not step). */
export function stepValue(row: PaneRow, current: unknown, step: Step): Json | undefined {
  const w = row.widget;
  if (w.type === 'switch') {
    if (step === 'activate') return !current;
    return step === 'next';
  }
  if (w.type === 'choice') {
    const options = choiceOptions(w.options, current);
    if (options.length === 0) return undefined;
    const i = options.indexOf(optionText(current));
    const j = stepIndex(i, options.length, step !== 'previous', edgeForOptions(options));
    return cycleValueJson(options[j] ?? '');
  }
  return undefined;
}

/** A typed text value for a `text` row, or the reason it is refused. */
export function parseTextValue(row: PaneRow, text: string): { ok: true; value: Json } | { ok: false; error: string } {
  if (row.widget.type !== 'text' && row.widget.type !== 'text-editor') return { ok: false, error: 'This row is not edited as text' };
  if (text.length > 64 * 1024) return { ok: false, error: 'The value is longer than 64 KiB' };
  if (row.widget.type === 'text' && row.widget.valueType === 'number') {
    const t = text.trim();
    if (t === '' || Number.isNaN(Number(t))) return { ok: false, error: 'Type a number' };
    return { ok: true, value: Number(t) };
  }
  return { ok: true, value: row.widget.type === 'text-editor' ? text.trim() : text };
}

// ---------- staging (PendingSettings) ----------

export interface Staging { readonly staged: ReadonlyMap<string, Json> }

export const EMPTY_STAGING: Staging = { staged: new Map() };

/** Stage `value` for `field`; a value equal to the live one unstages the row. */
export function stage(st: Staging, field: string, live: unknown, value: Json): Staging {
  const staged = new Map(st.staged);
  staged.delete(field);
  if (!sameJson(value, live)) staged.set(field, value);
  return { staged };
}

export function viewValue(st: Staging, field: string, live: unknown): unknown {
  return st.staged.has(field) ? st.staged.get(field) : live;
}

export function isDirty(st: Staging, field: string, live: unknown): boolean {
  return st.staged.has(field) && !sameJson(st.staged.get(field), live);
}

/** Number of staged rows that differ from the live config. */
export function pendingCount(st: Staging, config: Config): number {
  let n = 0;
  for (const [field, v] of st.staged) if (!sameJson(v, config[field])) n++;
  return n;
}

export interface FieldUpdate { field: string; value: Json }

/** The updates an apply sends: every staged field that differs from live, in page order. */
export function pendingUpdates(st: Staging, config: Config): FieldUpdate[] {
  const order = new Map(SETTINGS_ROWS.map((r, i) => [r.owner.args?.[0] ?? '', i]));
  return [...st.staged]
    .filter(([field, v]) => !sameJson(v, config[field]))
    .sort(([a], [b]) => (order.get(a) ?? 1e9) - (order.get(b) ?? 1e9) || a.localeCompare(b))
    .map(([field, value]) => ({ field, value }));
}

/** Drop staged values that now equal the live config (a refresh that landed on them). */
export function prune(st: Staging, config: Config): Staging {
  const staged = new Map([...st.staged].filter(([field, v]) => !sameJson(v, config[field])));
  return { staged };
}

/** The daemon calls the pane makes (the Tauri commands in src-tauri/src/settings.rs). */
export interface SettingsClient {
  getConfig(): Promise<Config>;
  update(updates: readonly FieldUpdate[]): Promise<unknown>;
  /** `UpdateModelControlPolicy {mode}` for the staged mode row (the TUI sends it apart from the batch). */
  setModelControlMode?(slug: string): Promise<unknown>;
}

export interface ApplyOutcome {
  config: Config;
  staging: Staging;
  /** Fields the daemon accepted. */
  applied: string[];
  /** Why nothing was applied (every staged row stays pending), else null. */
  error: string | null;
  message: string;
}

function fieldLabel(field: string): string {
  if (field === MODE_FIELD) return 'Model control mode';
  return SETTINGS_ROWS.find((r) => r.owner.args?.[0] === field)?.label ?? DAEMON_WIDGETS[field]?.label ?? field;
}

/** The suffix a saved-value toast carries for a non-live apply class (`restart_toast_suffix`). */
export function restartSuffix(field: string): string {
  const apply = SETTINGS_ROWS.find((r) => r.owner.args?.[0] === field)?.apply;
  switch (apply) {
    case 'daemon_restart': return ' · applies after daemon restart';
    case 'live_off_restart_on': return ' · off now, on after daemon restart';
    case 'partial_live': return ' · new launches now, resumed sessions after restart';
    case 'next_spawn': return ' · applies at next spawn';
    case 'not_applied': return " · stored, not applied (see the row's summary)";
    default: return '';
  }
}

/**
 * Apply every staged edit as ONE `UpdateDaemonConfig`, then re-read the
 * config. A refusal leaves every staged row pending and names them. The staged
 * model-control mode is the exception the TUI makes too: it goes to
 * `UpdateModelControlPolicy` after the batch, and its failure leaves only that
 * row pending.
 */
export async function applyStaged(client: SettingsClient, st: Staging, config: Config): Promise<ApplyOutcome> {
  const updates = pendingUpdates(st, config);
  const mode = updates.find((u) => u.field === MODE_FIELD);
  if (!mode) return applyBatch(client, st, config, updates);
  const rest = updates.filter((u) => u !== mode);
  const batch = rest.length > 0 ? await applyBatch(client, st, config, rest) : null;
  let current = batch?.config ?? config;
  let staging = batch?.staging ?? st;
  const messages = batch && batch.error === null && batch.applied.length > 0 ? [batch.message] : [];
  const errors = batch?.error ? [batch.message] : [];
  const applied = [...(batch?.applied ?? [])];
  if (!client.setModelControlMode) {
    errors.push('Model control mode: this client cannot set the mode');
  } else {
    try {
      const report = await client.setModelControlMode(optionText(mode.value));
      applied.push(MODE_FIELD);
      messages.push(modeMessage(report, optionText(mode.value)));
      const settled = new Map(staging.staged);
      settled.delete(MODE_FIELD);
      staging = { staged: settled };
    } catch (e) {
      errors.push(`Model control mode not applied and still pending: ${String(e)}`);
    }
  }
  try {
    current = await client.getConfig();
  } catch (e) {
    current = { ...current, ...(applied.includes(MODE_FIELD) ? { [MODE_FIELD]: mode.value } : {}) };
    messages.push(`re-reading the config failed: ${String(e)}`);
  }
  staging = prune(staging, current);
  const message = [...messages, ...errors].join('; ');
  return { config: current, staging, applied, error: errors.length > 0 ? errors.join('; ') : null, message };
}

async function applyBatch(client: SettingsClient, st: Staging, config: Config, updates: readonly FieldUpdate[]): Promise<ApplyOutcome> {
  if (updates.length === 0) {
    return { config, staging: EMPTY_STAGING, applied: [], error: null, message: 'No pending settings changes' };
  }
  try {
    await client.update(updates);
  } catch (e) {
    const labels = updates.map((u) => fieldLabel(u.field)).join(', ');
    const error = `none of ${labels} were applied: ${String(e)}`;
    return { config, staging: st, applied: [], error, message: `${updates.length} daemon setting${updates.length === 1 ? '' : 's'} not applied and still pending: ${error}` };
  }
  const applied = updates.map((u) => u.field);
  let fresh: Config = config;
  let reread = '';
  try {
    fresh = await client.getConfig();
  } catch (e) {
    // Accepted but not re-read: show the values the daemon accepted.
    fresh = { ...config, ...Object.fromEntries(updates.map((u) => [u.field, u.value])) };
    reread = ` (re-reading the config failed: ${String(e)})`;
  }
  const settled = new Map(st.staged);
  for (const f of applied) settled.delete(f);
  const parts = updates.map((u) => `${fieldLabel(u.field)} → ${optionText(u.value)}${restartSuffix(u.field)}`);
  const message = (updates.length === 1 ? parts[0]! : `Applied ${updates.length} settings: ${parts.join('; ')}`) + reread;
  return { config: fresh, staging: prune({ staged: settled }, fresh), applied, error: null, message };
}

// ---------- navigation ----------

export type Focus = 'rail' | 'items';

export interface Nav {
  group: number;
  /** Last section per group, and last row per section (the TUI remembers both). */
  sectionOf: Readonly<Record<string, string>>;
  rowOf: Readonly<Record<string, number>>;
  focus: Focus;
}

export const INITIAL_NAV: Nav = { group: 0, sectionOf: {}, rowOf: {}, focus: 'rail' };

export function currentGroup(nav: Nav): GroupRow {
  return SETTINGS_GROUPS[Math.max(0, Math.min(SETTINGS_GROUPS.length - 1, nav.group))]!;
}

export function currentSection(nav: Nav): SectionRow {
  const g = currentGroup(nav);
  const sections = sectionsOf(g.id);
  return sections.find((s) => s.id === nav.sectionOf[g.id]) ?? sections[0]!;
}

export function currentRow(nav: Nav, rowCount: number): number {
  const r = nav.rowOf[currentSection(nav).id] ?? 0;
  return rowCount === 0 ? 0 : Math.max(0, Math.min(rowCount - 1, r));
}

export function moveGroup(nav: Nav, delta: number): Nav {
  const n = SETTINGS_GROUPS.length;
  return { ...nav, group: Math.max(0, Math.min(n - 1, nav.group + delta)) };
}

/** Tab / ]: the next section tab of the group, wrapping inside it; focus moves into the items. */
export function moveSection(nav: Nav, delta: number): Nav {
  const g = currentGroup(nav);
  const sections = sectionsOf(g.id);
  const i = sections.findIndex((s) => s.id === currentSection(nav).id);
  const j = (((i + delta) % sections.length) + sections.length) % sections.length;
  return { ...nav, sectionOf: { ...nav.sectionOf, [g.id]: sections[j]!.id }, focus: 'items' };
}

export function moveRow(nav: Nav, delta: number, rowCount: number): Nav {
  const s = currentSection(nav).id;
  const r = currentRow(nav, rowCount);
  const next = rowCount === 0 ? 0 : Math.max(0, Math.min(rowCount - 1, r + delta));
  return { ...nav, rowOf: { ...nav.rowOf, [s]: next } };
}

/** Jump to a section (and row) by id, e.g. `:hooks`. */
export function goToSection(nav: Nav, section: string, row = 0): Nav {
  const s = SETTINGS_SECTIONS.find((x) => x.id === section);
  if (!s) return nav;
  const group = SETTINGS_GROUPS.findIndex((g) => g.id === s.group);
  return { group, sectionOf: { ...nav.sectionOf, [s.group]: s.id }, rowOf: { ...nav.rowOf, [s.id]: row }, focus: 'items' };
}

// ---------- search ----------

export interface Match { section: string; row: number }

/** Every row whose label, summary, keywords or section matches all of the query's terms. */
export function searchRows(query: string, config: Config = {}): Match[] {
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return [];
  const out: Match[] = [];
  for (const s of SETTINGS_SECTIONS) {
    sectionRows(s.id, config).forEach((r, i) => {
      const hay = [r.label, r.summary, r.field ?? '', s.label, ...r.keywords].join(' ').toLowerCase();
      if (terms.every((t) => hay.includes(t))) out.push({ section: s.id, row: i });
    });
  }
  return out;
}

/** The next (or previous) match after the cursor, wrapping; `wrapped` says it went round. */
export function nextMatch(matches: readonly Match[], at: Match, dir: 1 | -1): { match: Match; wrapped: boolean } | null {
  if (matches.length === 0) return null;
  const order = SETTINGS_SECTIONS.map((s) => s.id);
  const key = (m: Match) => order.indexOf(m.section) * 10000 + m.row;
  const cur = key(at);
  if (dir > 0) {
    const m = matches.find((x) => key(x) > cur);
    return m ? { match: m, wrapped: false } : { match: matches[0]!, wrapped: true };
  }
  const before = matches.filter((x) => key(x) < cur);
  return before.length > 0 ? { match: before[before.length - 1]!, wrapped: false } : { match: matches[matches.length - 1]!, wrapped: true };
}

// ---------- keys ----------

export type PaneEffect =
  | { kind: 'none' }
  | { kind: 'nav'; nav: Nav }
  | { kind: 'step'; step: Step }
  | { kind: 'activate' }
  | { kind: 'apply' }
  | { kind: 'refresh' }
  | { kind: 'search' }
  | { kind: 'match'; dir: 1 | -1 }
  | { kind: 'close' }
  | { kind: 'reset-role' }
  /** Gap sweep G4: `a` / `d` on a list row (Budgets). */
  | { kind: 'item'; op: 'add' | 'delete' }
  /** `s` / `r` / `c` / `i`: the provider-key actions (the admin overlay). */
  | { kind: 'provider-key'; op: 's' | 'r' | 'c' | 'i' }
  /** `>` / `<` / `=`: widen, narrow or reset the focused panel. */
  | { kind: 'resize'; op: '>' | '<' | '=' }
  | { kind: 'missing'; key: string; what: string };

/** TUI settings keys the desktop does not run yet (they act on partial rows). */
const NOT_YET: Readonly<Record<string, string>> = {
  e: 'Enable or disable skill (the Claude Skills row opens the skills list, where e runs it)',
  K: 'Move navigator column earlier (the desktop list has no optional navigator columns)',
  J: 'Move navigator column later (the desktop list has no optional navigator columns)',
};

/**
 * The key a pane effect hands to the selected row's overlay (registerRowOverlay,
 * local #88), or null when the pane runs it itself: a row overlay's own key
 * (a adds a hook or a provider) wins over the pane's list and provider keys.
 */
export function overlayKeyFor(fx: PaneEffect, overlayKeys: Readonly<Record<string, string>> | undefined): string | null {
  const key = fx.kind === 'item' ? (fx.op === 'add' ? 'a' : 'd')
    : fx.kind === 'provider-key' ? fx.op
      : fx.kind === 'missing' ? fx.key : null;
  return key !== null && overlayKeys?.[key] ? key : null;
}

/** Panel widths (px); null is the automatic layout. */
export interface PanelWidths { rail: number | null; info: number | null }

export const RAIL_DEFAULT = 230;
export const INFO_DEFAULT = 300;
const PANEL_STEP = 24;

/**
 * `>` / `<` widen or narrow the focused panel: the rail when it has focus,
 * otherwise the list, which takes room from the info card beside it; `=`
 * restores the automatic widths (the TUI's settings layout keys).
 */
export function resizePanels(w: PanelWidths, focus: 'rail' | 'items', op: '>' | '<' | '='): PanelWidths {
  if (op === '=') return { rail: null, info: null };
  const by = op === '>' ? PANEL_STEP : -PANEL_STEP;
  if (focus === 'rail') return { ...w, rail: Math.max(140, Math.min(420, (w.rail ?? RAIL_DEFAULT) + by)) };
  return { ...w, info: Math.max(160, Math.min(520, (w.info ?? INFO_DEFAULT) - by)) };
}

export function panelColumns(w: PanelWidths): string {
  if (w.rail === null && w.info === null) return '';
  const rail = w.rail === null ? 'minmax(180px, 230px)' : `${w.rail}px`;
  const info = w.info === null ? 'minmax(220px, 300px)' : `${w.info}px`;
  return `${rail} minmax(0, 1fr) ${info}`;
}

export function parsePanels(raw: string | null): PanelWidths {
  try {
    const v = raw ? JSON.parse(raw) as { rail?: unknown; info?: unknown } : {};
    const n = (x: unknown) => (typeof x === 'number' && Number.isFinite(x) ? x : null);
    return { rail: n(v.rail), info: n(v.info) };
  } catch { return { rail: null, info: null }; }
}

/** One key in the settings pane (focus not in a text field). */
export function paneKey(nav: Nav, key: string, shift: boolean, rowCount: number): PaneEffect {
  const rail = nav.focus === 'rail';
  switch (key) {
    case 'j': case 'ArrowDown':
      return { kind: 'nav', nav: rail ? moveGroup(nav, 1) : moveRow(nav, 1, rowCount) };
    case 'k': case 'ArrowUp':
      return { kind: 'nav', nav: rail ? moveGroup(nav, -1) : moveRow(nav, -1, rowCount) };
    case 'g': case 'Home':
      return { kind: 'nav', nav: rail ? { ...nav, group: 0 } : moveRow(nav, -1e6, rowCount) };
    case 'G': case 'End':
      return { kind: 'nav', nav: rail ? { ...nav, group: 1e6 } : moveRow(nav, 1e6, rowCount) };
    case 'h': case 'ArrowLeft':
      return { kind: 'nav', nav: { ...nav, focus: 'rail' } };
    case 'l': case 'ArrowRight':
      return { kind: 'nav', nav: { ...nav, focus: 'items' } };
    case 'Tab':
      return { kind: 'nav', nav: moveSection(nav, shift ? -1 : 1) };
    case ']': return { kind: 'nav', nav: moveSection(nav, 1) };
    case '[': return { kind: 'nav', nav: moveSection(nav, -1) };
    case 'Enter': case ' ':
      return rail ? { kind: 'nav', nav: { ...nav, focus: 'items' } } : { kind: 'activate' };
    case '+': return rail ? { kind: 'none' } : { kind: 'step', step: 'next' };
    case '-': return rail ? { kind: 'none' } : { kind: 'step', step: 'previous' };
    case 'w': return { kind: 'apply' };
    case 'R': return { kind: 'refresh' };
    case '/': return { kind: 'search' };
    case 'n': return { kind: 'match', dir: 1 };
    case 'N': return { kind: 'match', dir: -1 };
    case 'q': case 'Escape': return { kind: 'close' };
    case 'Delete': return rail ? { kind: 'none' } : { kind: 'reset-role' };
    case 'a': return rail ? { kind: 'none' } : { kind: 'item', op: 'add' };
    case 'd': return rail ? { kind: 'none' } : { kind: 'item', op: 'delete' };
    case 's': case 'r': case 'c': case 'i': return rail ? { kind: 'none' } : { kind: 'provider-key', op: key };
    case '>': case '<': case '=': return { kind: 'resize', op: key };
    default: {
      const what = NOT_YET[key];
      return what ? { kind: 'missing', key, what } : { kind: 'none' };
    }
  }
}
