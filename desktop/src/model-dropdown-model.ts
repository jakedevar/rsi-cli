// The model dropdown (gap sweep G4, hub #3000), pure (no DOM): the TUI's
// reusable model picker (crates/rsi/src/widget/model_dropdown.rs,
// docs/keybindings.md "Model Dropdown (Widget)" and "Model Picker"). One
// widget serves Ctrl-M / :model, the New session dialog, the create-entity
// form and the switch-model dialog. Browsing is tentative: nothing changes
// until Enter or 1-9 picks; Esc / q leave everything as it was.
// The DOM side is model-dropdown.ts; tests/model-dropdown.test.mjs pins it.

export interface ModelEntry {
  id: string;
  /** Display name (the id when the catalog has none). */
  label: string;
}

/** The TUI's provider cycle order; custom providers follow in their own order. */
export const PROVIDER_ORDER = ['Claude', 'Codex', 'Pioneer', 'OpenRouter', 'Bedrock', 'Local', 'Antigravity', 'CodexAppServer', 'Harness'];

/** The daemon's providers in the TUI's cycle order. */
export function orderProviders(providers: readonly string[]): string[] {
  const rank = (p: string) => { const i = PROVIDER_ORDER.indexOf(p); return i < 0 ? PROVIDER_ORDER.length : i; };
  return [...new Set(providers)].map((p, i) => ({ p, i })).sort((a, b) => rank(a.p) - rank(b.p) || a.i - b.i).map((x) => x.p);
}

/** The next (+1) or previous (-1) provider, wrapping. */
export function cycleProvider(providers: readonly string[], current: string, by: 1 | -1): string {
  if (providers.length === 0) return current;
  const i = providers.indexOf(current);
  if (i < 0) return providers[0]!;
  return providers[(i + by + providers.length) % providers.length]!;
}

const fold = (s: string): string => s.toLowerCase();
const squash = (s: string): string => s.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, '');

/** A filtered row: the entry, its catalog index and the matched ranges of its label. */
export interface Hit { entry: ModelEntry; index: number; ranges: Array<[number, number]> }

/**
 * The TUI filter: case-insensitive; every space-separated term must appear in
 * the name or the id; if not, punctuation and spacing are ignored as a
 * fallback (`opus45` finds `claude-opus-4-5`). Results keep catalog order.
 */
export function filterModels(entries: readonly ModelEntry[], query: string): Hit[] {
  const terms = query.split(/\s+/).filter(Boolean).map(fold);
  if (terms.length === 0) return entries.map((entry, index) => ({ entry, index, ranges: [] }));
  const out: Hit[] = [];
  entries.forEach((entry, index) => {
    const name = fold(entry.label);
    const id = fold(entry.id);
    const direct = terms.every((t) => name.includes(t) || id.includes(t));
    const loose = !direct && terms.every((t) => { const q = squash(t); return q !== '' && (squash(entry.label).includes(q) || squash(entry.id).includes(q)); });
    if (!direct && !loose) return;
    const ranges: Array<[number, number]> = [];
    if (direct) {
      for (const t of terms) {
        const at = name.indexOf(t);
        if (at >= 0) ranges.push([at, at + t.length]);
      }
    }
    out.push({ entry, index, ranges: mergeRanges(ranges) });
  });
  return out;
}

function mergeRanges(r: Array<[number, number]>): Array<[number, number]> {
  const s = [...r].sort((a, b) => a[0] - b[0]);
  const out: Array<[number, number]> = [];
  for (const x of s) {
    const last = out[out.length - 1];
    if (last && x[0] <= last[1]) last[1] = Math.max(last[1], x[1]);
    else out.push([x[0], x[1]]);
  }
  return out;
}

/** The picker's transient state. */
export interface DropdownState {
  query: string;
  /** Typing a filter: every printable key goes into the query. */
  typing: boolean;
  /** Highlighted row of the filtered list. */
  sel: number;
}

export const NEW_DROPDOWN: DropdownState = { query: '', typing: false, sel: 0 };

export type DropdownEffect =
  | { kind: 'none' }
  | { kind: 'pick'; index: number }
  | { kind: 'provider'; by: 1 | -1 }
  | { kind: 'close' };

export interface KeyLike { key: string; ctrlKey?: boolean; shiftKey?: boolean; altKey?: boolean; metaKey?: boolean }

const clampSel = (sel: number, n: number): number => (n <= 0 ? 0 : Math.max(0, Math.min(n - 1, sel)));

/** Drop the last word of the query (Ctrl-W). */
export function dropWord(q: string): string {
  return q.replace(/\s*\S+\s*$/, '');
}

/**
 * One key. `count` is the filtered row count before the key; a query edit
 * re-highlights the first match (the caller re-filters, so `sel` 0 is it).
 * `fixedCatalog` disables Tab (the Prompt Creator's fixed list), where `m`
 * also closes outside typing.
 */
export function dropdownKey(st: DropdownState, k: KeyLike, count: number, fixedCatalog = false): { st: DropdownState; effect: DropdownEffect } {
  const none = (s: DropdownState): { st: DropdownState; effect: DropdownEffect } => ({ st: s, effect: { kind: 'none' } });
  const fx = (effect: DropdownEffect, s: DropdownState = st) => ({ st: s, effect });
  if (k.metaKey || k.altKey) return none(st);
  const ctrl = !!k.ctrlKey;
  const key = k.key;
  // Keys shared by both modes.
  if (key === 'Tab') return fixedCatalog ? none(st) : fx({ kind: 'provider', by: k.shiftKey ? -1 : 1 }, { ...st, sel: 0 });
  if (key === 'Enter') return count > 0 ? fx({ kind: 'pick', index: clampSel(st.sel, count) }) : none(st);
  if (key === 'ArrowDown' || (ctrl && key.toLowerCase() === 'n')) return none({ ...st, sel: clampSel(st.sel + 1, count) });
  if (key === 'ArrowUp' || (ctrl && key.toLowerCase() === 'p')) return none({ ...st, sel: clampSel(st.sel - 1, count) });
  if (key === 'Home') return none({ ...st, sel: 0 });
  if (key === 'End') return none({ ...st, sel: clampSel(count - 1, count) });
  if (st.typing) {
    if (key === 'Escape') return none({ ...st, typing: false });
    if (key === 'Backspace') {
      if (st.query === '') return none({ ...st, typing: false });
      return none({ ...st, query: [...st.query].slice(0, -1).join(''), sel: 0 });
    }
    if (ctrl && key.toLowerCase() === 'w') return none({ ...st, query: dropWord(st.query), sel: 0 });
    if (ctrl && key.toLowerCase() === 'u') return none({ ...st, query: '', sel: 0 });
    if (!ctrl && [...key].length === 1) return none({ ...st, query: st.query + key, sel: 0 });
    return none(st);
  }
  if (ctrl) return none(st);
  switch (key) {
    case 'Escape': case 'q': return fx({ kind: 'close' });
    case 'm': return fixedCatalog ? fx({ kind: 'close' }) : none(st);
    case 'j': return none({ ...st, sel: clampSel(st.sel + 1, count) });
    case 'k': return none({ ...st, sel: clampSel(st.sel - 1, count) });
    case 'g': return none({ ...st, sel: 0 });
    case 'G': return none({ ...st, sel: clampSel(count - 1, count) });
    case '/': return none({ ...st, typing: true });
    default: break;
  }
  if (/^[1-9]$/.test(key)) {
    const i = Number(key) - 1;
    return i < count ? fx({ kind: 'pick', index: i }) : none(st);
  }
  return none(st);
}

/** The query row: `/query` and `matches of total`. */
export function queryLine(st: DropdownState, matches: number, total: number): string {
  if (!st.typing && st.query === '') return `${total} model${total === 1 ? '' : 's'} · / filters`;
  return `/${st.query}${st.typing ? '▏' : ''}  ${matches} of ${total}`;
}
