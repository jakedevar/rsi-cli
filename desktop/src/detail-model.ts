// Session-detail logic (pure, no DOM): which transcript events show, event and
// user-message navigation, tool call/result groups, fold targets, what `yy`
// copies, the input-bar priority rules and the rename / rate / model-switch /
// prompt-preview models. actions/detail.ts is the DOM adapter; the rules here
// follow the TUI (crates/rsi/src/action_handler/navigation.rs, overlay/*.rs).
// Unit-tested under tests/detail.test.mjs.
import type { ConversationEvent } from './api.js';
import type { KeyEventLike } from './keyengine.js';
import { splitFold } from './mdmodel.js';
import { FOLD_CHARS, elideBlobs, eventKind } from './model.js';

// ---------- visibility ----------

export interface Visibility {
  hideTools: boolean;
  hideSystem: boolean;
  hideThinking: boolean;
}

export const SHOW_ALL: Visibility = { hideTools: false, hideSystem: false, hideThinking: false };

/** Whether the transcript shows the event now (the TUI skips zero-height events). */
export function isVisible(e: ConversationEvent, v: Visibility): boolean {
  switch (eventKind(e)) {
    case 'tool': case 'result': return !v.hideTools;
    case 'system': return !v.hideSystem;
    case 'thinking': return !v.hideThinking;
    default: return true;
  }
}

/** Sequences of the shown events, in transcript order. */
export function visibleSeqs(events: readonly ConversationEvent[], v: Visibility): number[] {
  return events.filter((e) => isVisible(e, v)).map((e) => e.sequence);
}

// ---------- navigation ----------

/**
 * Shift-Down / Shift-Up: the next / previous shown event. With no cursor, next
 * is the first event and previous the last. Past either end the cursor stays
 * (the TUI does not wrap here).
 */
export function stepEvent(seqs: readonly number[], cur: number | null, dir: 1 | -1): number | null {
  if (seqs.length === 0) return null;
  const first = seqs[0]!;
  const last = seqs[seqs.length - 1]!;
  if (cur === null) return dir > 0 ? first : last;
  if (dir > 0) return seqs.find((s) => s > cur) ?? (seqs.includes(cur) ? cur : last);
  for (let i = seqs.length - 1; i >= 0; i--) if (seqs[i]! < cur) return seqs[i]!;
  return first;
}

/**
 * `]u` / `[u`: the next newer / previous older shown user message, wrapping at
 * the ends. A first backward jump (no cursor) starts at the newest.
 */
export function stepUserMessage(
  events: readonly ConversationEvent[], v: Visibility, cur: number | null, dir: 1 | -1,
): number | null {
  const users = events.filter((e) => eventKind(e) === 'user' && isVisible(e, v)).map((e) => e.sequence);
  if (users.length === 0) return null;
  if (dir > 0) {
    const next = cur === null ? users[0] : users.find((s) => s > cur);
    return next ?? users[0]!;
  }
  if (cur === null) return users[users.length - 1]!;
  for (let i = users.length - 1; i >= 0; i--) if (users[i]! < cur) return users[i]!;
  return users[users.length - 1]!;
}

/** The cursor after the event list changed: kept while its event exists. */
export function keepCursor(events: readonly ConversationEvent[], cur: number | null): number | null {
  if (cur === null) return null;
  return events.some((e) => e.sequence === cur) ? cur : null;
}

// ---------- tool groups and folds ----------

export interface ToolGroup {
  /** First member in transcript order (carries the group summary in the TUI). */
  leader: number;
  /** Member sequences, ascending: the call and its result(s). */
  members: number[];
}

const isToolEvent = (e: ConversationEvent) => e.event_type === 'ToolUse' || e.event_type === 'ToolResult';

/**
 * Pair each tool call with its result by the provider's `tool_use_id`, wherever
 * the result sits in the stream. An event without an id is never paired (the
 * TUI does not guess by adjacency either). Keyed by member sequence.
 */
export function toolGroups(events: readonly ConversationEvent[]): Map<number, ToolGroup> {
  const byId = new Map<string, number[]>();
  for (const e of events) {
    if (!isToolEvent(e) || !e.tool_use_id) continue;
    const list = byId.get(e.tool_use_id);
    if (list) list.push(e.sequence); else byId.set(e.tool_use_id, [e.sequence]);
  }
  const out = new Map<number, ToolGroup>();
  for (const members of byId.values()) {
    members.sort((a, b) => a - b);
    const group: ToolGroup = { leader: members[0]!, members };
    for (const m of members) out.set(m, group);
  }
  return out;
}

/** Whether the event has something to fold: tool and thinking blocks, long messages. */
export function isFoldable(e: ConversationEvent): boolean {
  switch (eventKind(e)) {
    case 'tool': case 'result': case 'thinking': return true;
    default: return splitFold(elideBlobs(e.content), FOLD_CHARS)[1] !== '';
  }
}

/** What `zo` / `zc` / `za` act on: the cursor event, or its whole call/result group. */
export function foldTargets(events: readonly ConversationEvent[], cur: number | null): number[] {
  if (cur === null) return [];
  const e = events.find((x) => x.sequence === cur);
  if (!e) return [];
  const g = toolGroups(events).get(cur);
  return g ? g.members : [cur];
}

export type FoldOp = 'open' | 'close' | 'toggle';

/** Open (true) or close (false) the targets: toggle opens unless every one is open. */
export function foldWanted(op: FoldOp, isOpen: readonly boolean[]): boolean {
  if (op === 'open') return true;
  if (op === 'close') return false;
  return isOpen.some((o) => !o);
}

// ---------- yank ----------

export type Focus = 'list' | 'transcript';

/**
 * The actions that move the transcript cursor (they put the keys "in" the transcript).
 * The fold keys are not here: they act on the pane that already has the keys
 * (actions/folds.ts), so they keep the focus where it is.
 */
export const TRANSCRIPT_ACTIONS: ReadonlySet<string> = new Set([
  'event-next', 'event-prev', 'user-next', 'user-prev',
  'scroll-up', 'scroll-down',
]);
/** The actions that move the session-list cursor (the keys go back to the list). */
export const LIST_ACTIONS: ReadonlySet<string> = new Set([
  'cursor-down', 'cursor-up', 'cursor-first', 'cursor-last', 'open', 'next-attention', 'prev-attention',
  'focus-search', 'view-active', 'view-archived',
]);

/** The focus after an action ran, or null when it does not change it. */
export function focusAfter(id: string, cur: Focus): Focus {
  if (TRANSCRIPT_ACTIONS.has(id)) return 'transcript';
  if (LIST_ACTIONS.has(id)) return 'list';
  return cur;
}

/** `yy`: the event under the cursor when the keys are in the transcript, else the session ID. */
export function yankTarget(focus: Focus, eventCursor: number | null): 'event' | 'session-id' {
  return focus === 'transcript' && eventCursor !== null ? 'event' : 'session-id';
}

// ---------- input bar priority ----------

export interface DetailKeyEnv {
  /** The message box has focus. */
  composerFocused: boolean;
  /** The text typed into the message box. */
  draft: string;
  /** A modal dialog is open. */
  modalOpen: boolean;
  /** A text field other than the message box has focus (search, dialog fields). */
  typingElsewhere: boolean;
  /** A session is open in the detail pane. */
  sessionOpen: boolean;
}

export type DetailKeyRoute =
  | { kind: 'pass' }
  | { kind: 'action'; action: 'event-next' | 'event-prev' | 'scroll-up' | 'scroll-down' }
  | { kind: 'blur-composer' };

const hasOtherMods = (ev: KeyEventLike) => !!(ev.ctrlKey || ev.altKey || ev.metaKey);

/**
 * The TUI's "input bar gets first chance" rule, for the keys that reach the
 * transcript from anywhere in the detail pane:
 * - The message box owns every key while it has focus, with three exceptions:
 *   `Esc` hands the keyboard back to the app keys (the draft stays; the desktop
 *   box has no separate normal mode), and `Up` / `Down` scroll the transcript
 *   when the draft is empty or one line, where moving the caret means nothing.
 * - Outside the box, `Shift-Down` / `Shift-Up` select the next / previous event
 *   and `Up` / `Down` scroll the transcript, unless a dialog or another field has the keys.
 */
export function routeDetailKey(ev: KeyEventLike, env: DetailKeyEnv): DetailKeyRoute {
  if (ev.defaultPrevented || ev.isComposing || env.modalOpen || env.typingElsewhere) return { kind: 'pass' };
  if (env.composerFocused) {
    if (ev.key === 'Escape' && !hasOtherMods(ev) && !ev.shiftKey) return { kind: 'blur-composer' };
    if (!env.sessionOpen || hasOtherMods(ev) || ev.shiftKey || env.draft.includes('\n')) return { kind: 'pass' };
    if (ev.key === 'ArrowUp') return { kind: 'action', action: 'scroll-up' };
    if (ev.key === 'ArrowDown') return { kind: 'action', action: 'scroll-down' };
    return { kind: 'pass' };
  }
  if (!env.sessionOpen || hasOtherMods(ev)) return { kind: 'pass' };
  if (ev.key === 'ArrowDown') return { kind: 'action', action: ev.shiftKey ? 'event-next' : 'scroll-down' };
  if (ev.key === 'ArrowUp') return { kind: 'action', action: ev.shiftKey ? 'event-prev' : 'scroll-up' };
  return { kind: 'pass' };
}

/** Lines the transcript scrolls per `Up` / `Down` (the TUI scrolls 3). */
export const SCROLL_LINES = 3;

// ---------- rename ----------

export const MAX_TITLE_CHARS = 200;

export type Checked<T> = { ok: true; value: T } | { ok: false; why: string };

/** The title the rename dialog may send (src-tauri/src/detail.rs checks the same). */
export function checkTitle(raw: string): Checked<string> {
  const t = raw.trim();
  if (t === '') return { ok: false, why: 'The title is empty' };
  if ([...t].length > MAX_TITLE_CHARS) return { ok: false, why: `The title is over ${MAX_TITLE_CHARS} characters` };
  if (/[\p{Cc}]/u.test(t)) return { ok: false, why: 'The title must be one line' };
  return { ok: true, value: t };
}

// ---------- rating ----------

export const MIN_RATING = 1;
export const MAX_RATING = 10;

/** `:rate <n>`: a whole number 1-10; an empty argument means "open the overlay" (null). */
export function parseRating(arg: string | undefined): Checked<number | null> {
  const t = (arg ?? '').trim();
  if (t === '') return { ok: true, value: null };
  if (!/^\d+$/.test(t)) return { ok: false, why: 'Invalid rating. Use :rate <1-10>' };
  const n = Number(t);
  if (n < MIN_RATING || n > MAX_RATING) return { ok: false, why: 'Invalid rating. Use :rate <1-10>' };
  return { ok: true, value: n };
}

/** The rating overlay's digit keys: 1-9 pick that rating, 0 picks 10. */
export function ratingFromKey(key: string): number | null {
  if (/^[1-9]$/.test(key)) return Number(key);
  return key === '0' ? MAX_RATING : null;
}

/** `h` / `l` / arrows: step the rating, clamped; the first step from none lands on 1. */
export function stepRating(cur: number | null, delta: 1 | -1): number {
  if (cur === null) return MIN_RATING;
  return Math.max(MIN_RATING, Math.min(MAX_RATING, cur + delta));
}

/** The rating shown for a session (the daemon stores 1-10; anything else is unrated). */
export function sessionRating(rating: number | null | undefined): number | null {
  return typeof rating === 'number' && rating >= MIN_RATING && rating <= MAX_RATING ? rating : null;
}

// ---------- model / effort switch ----------

export interface PendingSwitch { model: string; effort?: string | null }

/** `GetSessionModelSwitchOptions`, as the daemon returns it. */
export interface SwitchOptions {
  session_id: string;
  provider: string;
  model?: string | null;
  effort?: string | null;
  model_invocation_id?: string | null;
  switchable: boolean;
  unavailable_reason?: string | null;
  keeps_context: boolean;
  context_note: string;
  model_allowlist?: string[];
  pending?: PendingSwitch | null;
}

/** `DiscoverModels` with `include_capabilities`: model rows plus the effort ladders. */
export interface DiscoveredModels {
  models: Array<[string, string]>;
  effort_capabilities?: Array<{ model: string; supported_efforts: string[]; default_effort?: string | null }>;
  degraded?: boolean;
}

/** The effort levels a session may switch to, per provider (rsi_common::model_utils). */
const SWITCH_EFFORTS: Readonly<Record<string, readonly string[]>> = {
  Claude: ['low', 'medium', 'high', 'xhigh', 'max'],
  Codex: ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'],
  Pioneer: ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'],
  Antigravity: ['low', 'medium', 'high'],
};

/** Providers that cannot switch in place get no effort rows (the daemon refuses them first). */
export function switchEfforts(provider: string): readonly string[] {
  return SWITCH_EFFORTS[provider] ?? [];
}

const canonicalModel = (provider: string, id: string): string => {
  const t = id.trim().toLowerCase();
  return provider === 'Claude' && t.startsWith('claude-') && t.endsWith('[1m]') ? t.slice(0, -4).trimEnd() : t;
};

/** The operator allowlist (empty = unrestricted); the daemon checks it again. */
export function modelAllowed(allowlist: readonly string[] | undefined, provider: string, id: string): boolean {
  if (!allowlist || allowlist.length === 0) return true;
  const want = canonicalModel(provider, id);
  return allowlist.some((a) => canonicalModel(provider, a) === want);
}

export interface SwitchModelRow { id: string; label: string }

export interface SwitchState {
  options: SwitchOptions;
  models: SwitchModelRow[];
  modelIndex: number;
  /** Effort rows for the highlighted model; null is the model's own default. */
  efforts: Array<string | null>;
  effortIndex: number;
  capabilities: NonNullable<DiscoveredModels['effort_capabilities']>;
}

/** Effort rows: the model default first, then the levels both the model and the provider allow. */
export function effortChoices(
  provider: string, model: string, caps: NonNullable<DiscoveredModels['effort_capabilities']>,
): Array<string | null> {
  const allowed = switchEfforts(provider);
  // The ladder discovery reported for the model, else the compiled one (app/models.rs model_effort_ladder).
  const reported = caps.find((c) => c.model === model)?.supported_efforts;
  const ladder = reported ?? effortLadder(model);
  return [null, ...ladder.filter((l) => allowed.includes(l))];
}

const FIVE = ['low', 'medium', 'high', 'xhigh', 'max'];
const CODEX_ULTRA = [...FIVE, 'ultra'];
const CODEX_LEGACY = ['low', 'medium', 'high', 'xhigh'];

/** Claude family and (major, minor) from a model id (rsi_common::model_utils::parse_model_version). */
export function parseModelVersion(id: string): { family: 'fable' | 'opus' | 'sonnet' | 'haiku'; major: number; minor: number } | null {
  const lower = id.toLowerCase();
  const family = (['opus', 'sonnet', 'haiku', 'fable'] as const).find((f) => lower.includes(f));
  if (!family) return null;
  const m = lower.slice(lower.indexOf(family) + family.length).match(/^\D*(\d+)(?:\D+(\d+))?/);
  if (!m) return null;
  const minor = m[2] ?? '';
  // A longer "minor" is a date suffix (claude-sonnet-4-20250514): no parsable version.
  if (minor.length > 2) return null;
  return { family, major: Number(m[1]), minor: minor === '' ? 0 : Number(minor) };
}

/**
 * The compiled effort ladder of a model (rsi_common::model_utils::effort_ladder):
 * Codex GPT-6 Astra / Sol and GPT-5.6 Sol / Terra reach `ultra`, Luna `max`,
 * older Codex models `xhigh`; Claude Fable and Opus / Sonnet 4.7+ five levels,
 * 4.6 without `xhigh`, Opus 4.5 three; anything else none.
 */
export function effortLadder(model: string): string[] {
  const slug = model.toLowerCase().replace(/^openai\./, '').replace(/^.*\.openai\./, '');
  if (slug.startsWith('gpt-6') || slug.startsWith('gpt-5') || slug.startsWith('codex-auto-review')) {
    if (['gpt-6-astra', 'gpt-6-sol', 'gpt-5.6-sol', 'gpt-5.6-terra'].includes(slug)) return CODEX_ULTRA;
    if (['gpt-6-luna', 'gpt-5.6-luna'].includes(slug)) return FIVE;
    return CODEX_LEGACY;
  }
  const v = parseModelVersion(model);
  if (!v) return [];
  if (v.family === 'fable') return FIVE;
  if (v.family === 'opus' || v.family === 'sonnet') {
    if (v.major > 4 || (v.major === 4 && v.minor >= 7)) return FIVE;
    if (v.major === 4 && v.minor === 6) return ['low', 'medium', 'high', 'max'];
    if (v.family === 'opus' && v.major === 4 && v.minor === 5) return ['low', 'medium', 'high'];
  }
  return [];
}

/**
 * Build the picker for what the daemon reported. Null when the provider offers
 * no model the operator may switch to. The current model always has a row.
 */
export function buildSwitch(options: SwitchOptions, discovered: DiscoveredModels | null): SwitchState | null {
  const provider = options.provider;
  const models: SwitchModelRow[] = (discovered?.models ?? []).map(([id, label]) => ({ id, label }));
  const current = options.model ?? null;
  if (current && !models.some((m) => m.id === current)) models.unshift({ id: current, label: current });
  const shown = models.filter((m) => modelAllowed(options.model_allowlist, provider, m.id));
  if (shown.length === 0) return null;
  const caps = discovered?.effort_capabilities ?? [];
  const modelIndex = Math.max(0, current ? shown.findIndex((m) => m.id === current) : 0);
  const efforts = effortChoices(provider, shown[modelIndex]!.id, caps);
  const at = options.effort ? efforts.indexOf(options.effort) : -1;
  return { options, models: shown, modelIndex, efforts, effortIndex: at >= 0 ? at : 0, capabilities: caps };
}

/** j / k / g / G on the model rows; the effort row is kept when the new model offers it. */
export function moveModel(s: SwitchState, to: number): SwitchState {
  const modelIndex = Math.max(0, Math.min(s.models.length - 1, to));
  if (modelIndex === s.modelIndex) return s;
  const kept = s.efforts[s.effortIndex] ?? null;
  const efforts = effortChoices(s.options.provider, s.models[modelIndex]!.id, s.capabilities);
  const at = kept === null ? 0 : efforts.indexOf(kept);
  return { ...s, modelIndex, efforts, effortIndex: at >= 0 ? at : 0 };
}

/** h / l: lower / raise the effort, clamped. */
export function stepEffort(s: SwitchState, delta: 1 | -1): SwitchState {
  const effortIndex = Math.max(0, Math.min(s.efforts.length - 1, s.effortIndex + delta));
  return effortIndex === s.effortIndex ? s : { ...s, effortIndex };
}

export function chosen(s: SwitchState): { model: string; effort: string | null } {
  return { model: s.models[s.modelIndex]!.id, effort: s.efforts[s.effortIndex] ?? null };
}

/** `opus-5 · xhigh`, or `opus-5 · default effort`. */
export function tupleLabel(model: string, effort: string | null): string {
  return `${model.replace(/^claude-/, '')} · ${effort ?? 'default effort'}`;
}

/** What Enter does: nothing when the highlighted tuple is what the session already runs. */
export function switchDecision(s: SwitchState): 'unchanged' | 'queue' | 'no-fence' {
  if (!s.options.model_invocation_id) return 'no-fence';
  const c = chosen(s);
  const same = (s.options.model ?? null) === c.model && (s.options.effort ?? null) === c.effort;
  return same && !s.options.pending ? 'unchanged' : 'queue';
}

/** The queued-switch line for the header: `→ model · effort`. */
export function pendingLabel(p: PendingSwitch | null | undefined): string | null {
  return p ? `→ ${tupleLabel(p.model, p.effort ?? null)}` : null;
}

// ---------- prompt preview ----------

/** The session whose prompt the overlay shows after `j` / `k` / `G`. */
export function previewMove(ids: readonly string[], cur: string | null, key: 'next' | 'prev' | 'last'): string | null {
  if (ids.length === 0) return null;
  if (key === 'last') return ids[ids.length - 1]!;
  const i = cur === null ? -1 : ids.indexOf(cur);
  const from = i >= 0 ? i : key === 'next' ? -1 : ids.length;
  return ids[Math.max(0, Math.min(ids.length - 1, from + (key === 'next' ? 1 : -1)))] ?? null;
}

/** Ctrl-D / Ctrl-U move ten rows; the offset never goes below the top. */
export function previewScroll(offset: number, dir: 1 | -1): number {
  return Math.max(0, offset + dir * 10);
}

// ---------- continue prompt ----------

/** What Ctrl-Enter sends from the continue dialog: the trimmed text, or why not. */
export function checkFollowUp(raw: string): Checked<string> {
  const t = raw.trim();
  return t === '' ? { ok: false, why: 'Nothing to send' } : { ok: true, value: raw.replace(/\s+$/, '') };
}

// ---------- overlay chords ----------

export type OverlayChord = 'launch' | 'palette';

/**
 * `Space N` and `Space ;` work inside every overlay: they close it and open a
 * blank session prompt / the command palette. `key` is a KeyboardEvent.key
 * value; any other key drops the pending Space.
 */
export function overlayChord(armed: boolean, key: string): { armed: boolean; run: OverlayChord | null } {
  if (key === ' ' || key === 'Space') return { armed: true, run: null };
  if (armed && key === 'N') return { armed: false, run: 'launch' };
  if (armed && key === ';') return { armed: false, run: 'palette' };
  return { armed: false, run: null };
}
