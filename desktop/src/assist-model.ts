// Assist overlays, pure rules (hub #3015 slice 13B): memory search, the AI
// command and AI chat, the dialectic and the entity-card editor. Ports of the
// TUI's crates/rsi/src/overlay/{memory_search,ai_command,ai_chat,dialectic,
// card_editor}.rs and the `:card` / `:ask` parsing in commands.rs.
// Tested in tests/assist.test.mjs; the DOM is src/assist.ts.

// ---------- memory search ----------

/** `rsi_common::rpc::MemorySearchResult`. */
export interface MemoryResult {
  path: string;
  start_line: number;
  end_line: number;
  score: number;
  snippet: string;
  source: string;
}

export function parseMemoryResults(v: unknown): MemoryResult[] {
  if (!Array.isArray(v)) return [];
  const out: MemoryResult[] = [];
  for (const r of v) {
    if (!r || typeof r !== 'object') continue;
    const o = r as Record<string, unknown>;
    if (typeof o.path !== 'string') continue;
    out.push({
      path: o.path,
      start_line: typeof o.start_line === 'number' ? o.start_line : 0,
      end_line: typeof o.end_line === 'number' ? o.end_line : 0,
      score: typeof o.score === 'number' ? o.score : 0,
      snippet: typeof o.snippet === 'string' ? o.snippet : '',
      source: typeof o.source === 'string' ? o.source : '',
    });
  }
  return out;
}

/** The TUI's row header: `path [0.87]`. */
export function memoryHeader(r: MemoryResult): string {
  return `${r.path} [${r.score.toFixed(2)}]`;
}

/** `lines 12-30 · memory`. */
export function memoryDetail(r: MemoryResult): string {
  const lines = r.start_line === r.end_line ? `line ${r.start_line}` : `lines ${r.start_line}-${r.end_line}`;
  return r.source ? `${lines} · ${r.source}` : lines;
}

/** The TUI searches on every edit and clears the results on an empty query. */
export function memoryQueryToSend(query: string): string | null {
  return query === '' ? null : query;
}

/** Move a list selection (clamped, no wrap, like the TUI). */
export function moveSelection(count: number, cur: number, delta: number): number {
  if (count <= 0) return 0;
  return Math.min(count - 1, Math.max(0, cur + delta));
}

// ---------- conversations (AI chat, dialectic) ----------

export interface Turn { role: 'user' | 'assistant'; content: string }

/**
 * `Esc` in the chat and dialectic: while a request runs it closes; otherwise
 * it clears a non-empty input first and closes on the next press.
 */
export function escapeAction(input: string, inFlight: boolean): 'clear' | 'close' {
  return !inFlight && input !== '' ? 'clear' : 'close';
}

/** `q` closes only an empty input (it types otherwise). */
export function qCloses(input: string, inFlight: boolean): boolean {
  return inFlight || input === '';
}

/** The question to send (trimmed), or null for a blank input or a request in flight. */
export function questionToSend(input: string, inFlight: boolean): string | null {
  if (inFlight) return null;
  const t = input.trim();
  return t === '' ? null : t;
}

/** The conversation after asking `question` (sent to the daemon as the history). */
export function ask(turns: readonly Turn[], question: string): Turn[] {
  return [...turns, { role: 'user', content: question }];
}

/** A reply appended as the TUI does: the answer, or `Error: …` (kept in the conversation). */
export function reply(turns: readonly Turn[], outcome: { ok: true; text: string } | { ok: false; error: string }): Turn[] {
  return [...turns, { role: 'assistant', content: outcome.ok ? outcome.text : `Error: ${outcome.error}` }];
}

/** `GenerateTextResponse.text`, trimmed; empty is the TUI's "Model returned empty response". */
export function generatedText(v: unknown): { ok: true; text: string } | { ok: false; error: string } {
  const text = v && typeof v === 'object' && typeof (v as { text?: unknown }).text === 'string'
    ? (v as { text: string }).text.trim() : '';
  return text ? { ok: true, text } : { ok: false, error: 'Model returned empty response' };
}

/** `rsi_common::rpc::DialecticSource`. */
export interface DialecticSource { kind: string; label: string; detail: string | null }

/** `QueryMemoryResponse` → the answer and its sources, or null. */
export function parseQueryResponse(v: unknown): { answer: string; sources: DialecticSource[]; toolCalls: number } | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  if (typeof o.answer !== 'string') return null;
  const sources: DialecticSource[] = [];
  for (const s of Array.isArray(o.sources) ? o.sources : []) {
    if (!s || typeof s !== 'object') continue;
    const r = s as Record<string, unknown>;
    if (typeof r.label !== 'string') continue;
    sources.push({ kind: typeof r.kind === 'string' ? r.kind : '', label: r.label, detail: typeof r.detail === 'string' ? r.detail : null });
  }
  return { answer: o.answer, sources, toolCalls: typeof o.tool_calls === 'number' ? o.tool_calls : 0 };
}

/** `[memory] notes.md (0.91)`. */
export function sourceLine(s: DialecticSource): string {
  return `[${s.kind || 'source'}] ${s.label}${s.detail ? ` (${s.detail})` : ''}`;
}

/** `:ask <question>` fills the dialectic input; bare `:ask` opens it empty. */
export function askPrefill(arg: string | undefined): string {
  return arg?.trim() ?? '';
}

// ---------- AI command ----------

/** The AI command's result is a preview: `a` accepts it, `d` / `Esc` discard it. */
export type PreviewKey = 'accept' | 'discard' | 'none';

export function previewKey(key: string): PreviewKey {
  if (key === 'a') return 'accept';
  if (key === 'd' || key === 'Escape') return 'discard';
  return 'none';
}

/** Why the AI command or chat cannot open on `text` (the TUI's notices), or null. */
export function assistRefusal(kind: 'command' | 'chat', text: string): string | null {
  if (text.trim() !== '') return null;
  return kind === 'command' ? 'No text to transform' : 'No text to chat about';
}

// ---------- entity cards ----------

/** `EntityCard::MAX_FACTS`. */
export const MAX_FACTS = 40;

export type CardWho = 'project' | 'user';

export type CardCommand =
  | { kind: 'open'; who: CardWho }
  | { kind: 'add'; who: CardWho; fact: string }
  | { kind: 'invalid'; text: string };

/** `:card [user] [add <fact>]`, exactly as the TUI parses it (commands.rs). */
export function parseCardCommand(arg: string | undefined): CardCommand {
  const a = (arg ?? '').trim();
  if (a === '') return { kind: 'open', who: 'project' };
  if (a === 'user') return { kind: 'open', who: 'user' };
  const fact = (s: string): string => s.trim().replace(/^"+|"+$/g, '');
  if (a.startsWith('add ')) {
    const f = fact(a.slice(4));
    return f ? { kind: 'add', who: 'project', fact: f } : { kind: 'invalid', text: `card ${a}` };
  }
  if (a.startsWith('user ')) {
    const rest = a.slice(5);
    if (rest.startsWith('add ')) {
      const f = fact(rest.slice(4));
      return f ? { kind: 'add', who: 'user', fact: f } : { kind: 'invalid', text: `card ${a}` };
    }
    return { kind: 'open', who: 'user' };
  }
  return { kind: 'open', who: 'project' };
}

/** The facts after a `:card add` (null with the TUI's "card is full" refusal). */
export function addFact(facts: readonly string[], fact: string): string[] | null {
  return facts.length >= MAX_FACTS ? null : [...facts, fact];
}

export interface CardState {
  facts: string[];
  selected: number;
  /** The text of the fact being edited, or null in navigation mode. */
  editing: string | null;
  /** `d` was pressed: a second `d` deletes. */
  pendingDelete: boolean;
}

export function openCard(facts: readonly string[]): CardState {
  return { facts: [...facts], selected: 0, editing: null, pendingDelete: false };
}

/**
 * - `save`: send the whole card (SetEntityCard),
 * - `close`: save then close,
 * - `full`: refused, the card holds MAX_FACTS,
 * - `saved-notice`: Ctrl-S, save and say so.
 */
export type CardEffect = 'none' | 'save' | 'close' | 'full' | 'saved-notice';

export interface CardKey { key: string; ctrl?: boolean; alt?: boolean; meta?: boolean }

function clampSel(facts: readonly string[], sel: number): number {
  return sel >= facts.length && facts.length > 0 ? facts.length - 1 : sel;
}

/** Navigation-mode keys (overlay/card_editor.rs handle_nav_mode). */
export function cardNavKey(st: CardState, ev: CardKey): { st: CardState; effect: CardEffect } {
  const plain = !ev.ctrl && !ev.alt && !ev.meta;
  const k = ev.key;
  if (st.pendingDelete) {
    st = { ...st, pendingDelete: false };
    if (k === 'd' && plain) {
      if (st.facts.length === 0) return { st, effect: 'none' };
      const facts = st.facts.filter((_, i) => i !== st.selected);
      return { st: { ...st, facts, selected: clampSel(facts, st.selected) }, effect: 'save' };
    }
  }
  const n = st.facts.length;
  if (ev.ctrl && (k === 's' || k === 'S')) return { st, effect: 'saved-notice' };
  if (k === 'Escape') return { st, effect: 'close' };
  if (!plain) return { st, effect: 'none' };
  switch (k) {
    case 'j': case 'ArrowDown':
      return { st: { ...st, selected: n > 0 && st.selected < n - 1 ? st.selected + 1 : st.selected }, effect: 'none' };
    case 'k': case 'ArrowUp':
      return { st: { ...st, selected: Math.max(0, st.selected - 1) }, effect: 'none' };
    case 'g': return { st: { ...st, selected: 0 }, effect: 'none' };
    case 'G': return { st: { ...st, selected: n > 0 ? n - 1 : 0 }, effect: 'none' };
    case 'a':
      if (n >= MAX_FACTS) return { st, effect: 'full' };
      return { st: { ...st, facts: [...st.facts, ''], selected: n, editing: '' }, effect: 'none' };
    case 'e': case 'i': case 'Enter':
      return n > 0 ? { st: { ...st, editing: st.facts[st.selected] ?? '' }, effect: 'none' } : { st, effect: 'none' };
    case 'd': return { st: { ...st, pendingDelete: true }, effect: 'none' };
    case 'J': {
      if (n === 0 || st.selected >= n - 1) return { st, effect: 'none' };
      const facts = [...st.facts];
      [facts[st.selected], facts[st.selected + 1]] = [facts[st.selected + 1]!, facts[st.selected]!];
      return { st: { ...st, facts, selected: st.selected + 1 }, effect: 'save' };
    }
    case 'K': {
      if (st.selected === 0 || n === 0) return { st, effect: 'none' };
      const facts = [...st.facts];
      [facts[st.selected], facts[st.selected - 1]] = [facts[st.selected - 1]!, facts[st.selected]!];
      return { st: { ...st, facts, selected: st.selected - 1 }, effect: 'save' };
    }
    case 'q': return { st, effect: 'close' };
    default: return { st, effect: 'none' };
  }
}

/** Edit mode: `Enter` commits (an empty fact is deleted) and saves. */
export function commitEdit(st: CardState, text: string): { st: CardState; effect: CardEffect } {
  if (st.editing === null) return { st, effect: 'none' };
  const t = text.trim();
  let facts: string[];
  let selected = st.selected;
  if (t === '') {
    facts = st.facts.filter((_, i) => i !== st.selected);
    selected = clampSel(facts, selected);
  } else {
    facts = st.facts.map((f, i) => (i === st.selected ? t : f));
  }
  return { st: { ...st, facts, selected, editing: null }, effect: 'save' };
}

/** Edit mode: `Esc` cancels; a new, still empty fact is removed. Nothing is saved. */
export function cancelEdit(st: CardState): CardState {
  if (st.editing === null) return st;
  if ((st.facts[st.selected] ?? '') === '') {
    const facts = st.facts.filter((_, i) => i !== st.selected);
    return { ...st, facts, selected: clampSel(facts, st.selected), editing: null };
  }
  return { ...st, editing: null };
}

/** The facts to send: never the empty placeholder of a fact still being added. */
export function factsToSave(st: CardState): string[] {
  return st.facts.filter((f) => f.trim() !== '');
}

/** The card's title: the project's name or `User`. */
export function cardTitle(who: CardWho, projectName: string | undefined): string {
  return who === 'user' ? 'User' : projectName ?? 'Project';
}

/**
 * The project the assist overlays act on, like the TUI's current project: the
 * open session's, else the project the list is filtered to.
 */
export function activeProject(openSessionProject: string | null | undefined, projectFilter: string | null | undefined): string | null {
  return openSessionProject || projectFilter || null;
}
