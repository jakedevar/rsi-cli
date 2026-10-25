// Dialog and pane key hints, tooltips and toasts follow the Vim keys switch (hub #3101 follow-up, local #129, #132).
// With the switch off the single-letter keys do nothing (modal-gate.ts, dialog-gate.ts), so
// the text that advertises them must not: a button label loses its "(x)" suffix and a hint
// line drops the letter-key entries and keeps Enter, Esc, arrows and Tab. One pure
// formatter below and one DOM pass (installKeyHints) that rewrites the matching text nodes
// in place, remembers each original, and restores it when the switch comes back on. No
// dialog builds its labels differently: they keep writing the TUI wording. title="..."
// tooltips and the toast / empty-state prose go through formatKeyTip (same pass). Local #133:
// the to-do footer, Settings value cells and admin prose key lists (formatKeyHint), the
// notification log's messages and ':' command prose (formatKeyTip: ':name' becomes the
// command palette's title for it) reach the same two formatters.
import type { Platform } from './keyengine.js';
import { proseLabel } from './keylabel.js';
import { COMMANDS } from './keymap.generated.js';

/** Plain-text keys the gates block with the switch off (everything but modal-gate STANDARD_KEYS). */
function isVimKey(token: string): boolean {
  return /^[A-Za-z]$/.test(token) || /^\d(-\d)?$/.test(token) || /^[[\]+;/?]$/.test(token)
    || token === 'Space' || token === 'dd' || token === 'gg' || token === 'Backspace';
}

/** Keys that work with the switch off: modifier chords (Ctrl-K), F-keys and the standard navigation keys. */
const PLAIN_HINT_KEYS: ReadonlySet<string> = new Set(['Enter', 'Esc', 'Escape', 'Tab', 'Up', 'Down', 'Left', 'Right', 'PageUp', 'PageDown', 'Home', 'End']);

/**
 * Whether a registry key hint (`j`, `gg`, `<Space>a`, `:archive`, `DD`) is a Vim key or `:` command
 * that does nothing with the Vim keys switch off (local #227). `Ctrl-N`, `Shift-Down`, `F2`,
 * `Enter`, `Esc`, `Tab` and the arrows still work, so the palette and Help keep showing them.
 */
export function isVimOnlyHint(hint: string): boolean {
  if (/^(?:Ctrl|Alt|Cmd|Option|Shift)-/.test(hint)) return false;
  return !(/^F\d{1,2}$/.test(hint) || PLAIN_HINT_KEYS.has(hint));
}

const KEY_TOKEN = String.raw`(?:Space|dd|gg|Backspace|[A-Za-z]|\d(?:-\d)?|[[\]+;?]|[A-Za-z][A-Za-z-]*)`;
const LEADING_SPEC = new RegExp(String.raw`^(${KEY_TOKEN}(?:(?:/| or | / )${KEY_TOKEN})*)(?=\s|$)`);

/**
 * A button label without its "(x)" key suffix: "Refresh (r)" -> "Refresh", "Answers: on (n)" -> "Answers: on".
 * The manager bars' symbol and chord keys go too (local #223): "Raise one step (+)", "Filter… (/)",
 * "Next view (])", "Copy session ID (yy)", "Up a level (Backspace)", "Manager workspace (gm)" (local #230)
 * and "Conversation (Tab)": Tab is the browser's focus order with the switch off (local #243).
 */
export function stripKeyLabel(label: string): string {
  return label.replace(/\s*\((?:[A-Za-z]|Space|dd|yy|gm|Tab|Backspace|[+/[\]])\)(\s*)$/, '$1');
}

/** One hint entry ("j/k move", "Enter or c cancels it") without its Vim-only keys; null when nothing is left. */
function stripSegment(seg: string): string | null {
  if (/^<Space>\S/.test(seg)) return null; // a leader sequence ("<Space>dj/dk move") is Vim-only
  if (/^\/\s/.test(seg)) return null; // "/ filter": the slash key itself, not a joiner (local #223)
  const m = LEADING_SPEC.exec(seg);
  if (!m) return seg;
  const spec = m[1]!;
  const joiner = spec.includes(' or ') && !spec.includes('/') ? ' or ' : '/';
  const tokens = spec.split(/ \/ |\/| or /);
  const kept = tokens.filter((t) => !isVimKey(t));
  if (kept.length === tokens.length) return seg;
  if (kept.length === 0) return null;
  // "Enter/Backspace next or previous question": without Backspace the clause would say Enter does both (local #246).
  if (/^\s+next or previous\b/.test(seg.slice(spec.length))) return null;
  return kept.join(joiner) + seg.slice(spec.length);
}

/**
 * A hint line for Vim keys off: entries (split on " · ", else ", ") that start with a
 * Vim-only key spec lose those keys, or vanish when none is left; "Press n to ..."
 * sentences vanish. Prose with a key list inside it (admin notes) is split further at
 * "; " and sentence ends: a piece that starts with a key list ("a / Enter confirms the
 * selected one", "Esc / n / q cancels") is stripped the same way. Text with no such key
 * comes back unchanged.
 */
export function formatKeyHint(text: string): string {
  return paneCycleKey(formatKeyHintKeys(text));
}

/**
 * With Vim keys off Tab is the browser's focus order in the panes, Help and the model picker, and F6 is
 * the pane-cycle key (modal-gate paneKeyName, local #243). Their hint phrases name F6 instead, as do the
 * satellite-peers and Devices/Projects dialogs' ("F6 cycles peers", local #248). A text field's Tab
 * (path completion, budgets) is a typing context and keeps its wording.
 */
const PANE_TAB_PHRASE = /\bTab(\/Shift-Tab (?:sections|provider)|(?: or Esc returns| conversation| agents\/usage| views| cycles peers| switches Devices))/g;
function paneCycleKey(text: string): string {
  return text.replace(PANE_TAB_PHRASE, (_m, rest: string) => `F6${rest.replace('Shift-Tab', 'Shift-F6')}`);
}

function formatKeyHintKeys(text: string): string {
  const t = stripPressAlt(text).replace(/\bPress [A-Za-z] to [^.]*\.\s*/g, '').trim();
  if (t.startsWith('<Space>')) return ''; // a leader-key line ("<Space>dj/dk move · dv open", "<Space>du undoes “title”") has no keyless form
  if (t.includes(' · ')) return joinEntries(t, ' · ');
  return stripProse(t);
}

/** Entries split on one separator, each stripped of its Vim-only keys (the legacy one-level pass). */
function joinEntries(text: string, sep: string): string {
  const dot = /\.\s*$/.test(text);
  const body = dot ? text.replace(/\.\s*$/, '') : text;
  const out: string[] = [];
  for (const seg of body.split(sep)) {
    const s = stripSegment(seg.trim());
    if (s !== null && s !== '') out.push(s);
  }
  const t = out.join(sep);
  return t === '' ? t : dot && !t.endsWith('.') ? `${t}.` : t;
}

/** The verbs a key mention in hint prose uses (KEY_VERB below plus the admin notes' own). */
const HINT_VERB = String.raw`(?:begins?|audits?|probes?|repairs?|pages?|selects?|refreshes|refresh|cycles?|add|edit|install|rotate|check|set|close)`;

/** A piece that begins a new sentence or clause: a lone key letter counts only before a key verb, a list of keys always. */
function startsWithKeyList(piece: string): boolean {
  const m = LEADING_SPEC.exec(piece);
  if (!m) return false;
  const spec = m[1]!;
  const tokens = spec.split(/ \/ |\/| or /);
  if (!tokens.some(isVimKey)) return false;
  if (tokens.length > 1 || tokens[0] === 'Space') return true;
  return new RegExp(String.raw`^\S+\s+(?:${KEY_VERB}|${HINT_VERB})\b`).test(piece);
}

/** Hint prose: "; " and sentence ends are piece boundaries, commas split a piece into entries (first piece: legacy rule). */
function stripProse(text: string): string {
  const parts = text.split(/((?<!\s);\s+|(?<=\.)\s+)/);
  let out = '';
  parts.forEach((part, idx) => {
    if (idx % 2 === 1) return;
    const sep = idx > 0 ? parts[idx - 1]! : '';
    const dotted = /\.$/.test(part);
    let core = dotted ? part.slice(0, -1) : part;
    // "nothing is sent until y": the confirming key becomes the action.
    core = core.replace(/\buntil y$/, 'until you confirm');
    const entries = core.split(', ');
    const kept: string[] = [];
    // A later piece is a key list only when it starts like one ("a / Enter confirms ..."); plain prose stays.
    if (idx > 0 && !startsWithKeyList(entries[0]!.trim())) {
      out += (out === '' ? '' : sep) + core + (dotted ? '.' : '');
      return;
    }
    entries.forEach((entry, ei) => {
      const e = entry.trim();
      const s = stripSegment(e);
      if (s !== null && s !== '') kept.push(s);
    });
    const piece = kept.join(', ');
    if (piece === '') {
      if (dotted && out !== '' && !/\.$/.test(out)) out += '.';
      return;
    }
    out += (out === '' ? '' : sep) + piece + (dotted ? '.' : '');
  });
  return out;
}

// ---------- tooltips (title="...") and toast / empty-state prose (local #132) ----------

/** A key as a tooltip or a toast names it: only keys the Vim keys switch blocks (not Ctrl-K, Enter, Esc, arrows). */
function isVimTipKey(token: string): boolean {
  return token === '/' || /^(?:[A-Za-z]|\d(?:-\d)?|[?;:[\]+<>!-])$/.test(token) || /^<Space>\S+$/.test(token)
    || /^(?:g[A-Za-z]|gf\d|[zZ][A-Za-z]|m[a-z]|dd|DD|yy|F\d{1,2}|[[\]][a-z]|Space|Backspace)$/.test(token) || /^:[A-Za-z][\w -]*!?$/.test(token);
}

/** The verbs a prose key mention uses ("a adds one", "P unpins", "r retries"). */
const KEY_VERB = String.raw`(?:adds?|creates?|opens?|launches|starts?|runs?|retries|reloads?|refreshes|clears?|enables?|disables?|cancels?|shows?|hides?|deletes?|sends?|saves?|edits?|rotates?|toggles?|confirms?|stops?|closes?|unpins?|pins?|moves?|picks?|imports?|checks?|dismiss(?:es)?|copies|undoes|links?|appoints?|sets?|previews?|applies)`;

/** A "(...)" item that is a Vim-only key, or a Vim-only key then its description: "r", "j/k", "<Space>dt shows the panel", "a adds one". */
function isVimTipItem(item: string): boolean {
  const m = /^(\S+)(?:\s+(.+))?$/.exec(item.trim().replace(/^then\s+/, ''));
  if (!m) return false;
  const head = m[1]!;
  const keys = head === '/' ? [head] : head.split('/');
  if (!keys.every(isVimTipKey)) return false;
  const rest = m[2];
  if (rest === undefined) return true;
  if (keys.length === 1 && /^[A-Za-z]$/.test(head)) return new RegExp(String.raw`^${KEY_VERB}\b`).test(rest);
  return true;
}

/** "(r, :refresh)" -> "", "(Ctrl-K, gt)" -> "(Ctrl-K)", "(j/k, then Enter)" -> "(Enter)", "(staged; w applies)" -> "(staged)"; other parentheses stay. */
function stripTipParens(text: string): string {
  return text.replace(/(\s*)\(([^()]*)\)/g, (all, space: string, inner: string) => {
    const parts = inner.split(/(,\s+|;\s+)/);
    const items = parts.filter((_, i) => i % 2 === 0);
    const seps = parts.filter((_, i) => i % 2 === 1);
    const keep = items.map((it) => !isVimTipItem(it));
    if (keep.every(Boolean)) return all;
    let out = '';
    let first = true;
    items.forEach((it, i) => {
      if (!keep[i]) return;
      out += first ? (i > 0 ? it.replace(/^then\s+/, '') : it) : seps[i - 1]! + it;
      first = false;
    });
    return out === '' ? '' : `${space}(${out})`;
  });
}

/** One prose clause that only names a Vim key: "n launches a manager", "Here n launches one", "press r". */
const KEY_CLAUSE = new RegExp(String.raw`^(?:(?:Here|There) )?(?:(?:[Pp]ress (?:[A-Za-z]|<Space>\S+))|(?:[A-Za-z](?: or [A-Za-z])* ${KEY_VERB}\b.*))\.?$`);

/** One list entry that only names a Vim key and its action: "a adds", "and i imports from environment". */
const KEY_ENTRY = new RegExp(String.raw`^(?:and )?[A-Za-z](?: or [A-Za-z])* ${KEY_VERB}\b[^,]*$`);

/** "a adds, Enter edits, d deletes" -> "Enter edits"; "" when every entry names a key; a clause with no such entry comes back unchanged. */
function dropKeyEntries(clause: string): string {
  const dot = /\.$/.test(clause);
  const body = dot ? clause.slice(0, -1) : clause;
  if (!body.includes(', ')) return clause;
  const parts = body.split(/(, (?:and )?)/);
  const entries = parts.filter((_, i) => i % 2 === 0);
  const seps = parts.filter((_, i) => i % 2 === 1);
  const isKey = entries.map((e) => KEY_ENTRY.test(e.trim()));
  if (!isKey.some(Boolean)) return clause;
  let out = '';
  entries.forEach((e, i) => {
    if (isKey[i]) return;
    out += out === '' ? e.replace(/^and /, '') : seps[i - 1]! + e;
  });
  return out === '' ? '' : dot ? `${out}.` : out;
}

/** Drop the key-only clauses of a sentence ("...read-only; n launches a manager" -> "...read-only"), and the key-only entries of a list ("a adds, Enter edits" -> "Enter edits"). */
function stripKeyClauses(text: string): string {
  const parts = text.split(/(; |: |, or )/);
  const clauses = parts.filter((_, i) => i % 2 === 0).map(dropKeyEntries);
  const seps = parts.filter((_, i) => i % 2 === 1);
  const dot = /\.$/.test(text);
  const keep = clauses.map((c) => c !== '' && !KEY_CLAUSE.test(c.replace(/\.$/, '').trim()));
  if (keep.every(Boolean) && clauses.join('') === parts.filter((_, i) => i % 2 === 0).join('')) return text;
  let out = '';
  let prev = -1;
  clauses.forEach((c, i) => {
    if (!keep[i]) return;
    out += prev < 0 ? c : seps[prev]! + c;
    prev = i;
  });
  if (!keep[0]) out = out.charAt(0).toUpperCase() + out.slice(1);
  return dot && out !== '' && !out.endsWith('.') ? `${out}.` : out;
}

/** ':name' (any registered command or alias) -> the palette entry that does the same. */
const PALETTE_TITLES: ReadonlyMap<string, string> = new Map(COMMANDS.flatMap((c) => [c.name, ...c.aliases].map((n) => [n, c.title] as const)));
const COMMAND_PROSE = new RegExp(
  String.raw`(^|[\s("'\u201c]):(${[...PALETTE_TITLES.keys()].sort((a, b) => b.length - a.length).map((n) => n.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})(?![\w-])(?:\s+(?:\[[^\]]*\]|<[^>]*>))*`, 'g');

/** Toast prose that tells the operator to run a ':' command points at the command palette instead (the command line needs Vim keys). */
function paletteCommands(text: string): string {
  return text.replace(COMMAND_PROSE, (_all, lead: string, name: string) => `${lead}\u201c${PALETTE_TITLES.get(name)!}\u201d (command palette)`);
}

/**
 * A tooltip or toast for Vim keys off: parenthesised Vim-only keys go ("Pinned (P unpins)" ->
 * "Pinned", "Open a session first (j/k, then Enter)" -> "(Enter)"), so do clauses that only
 * name a key ("...; n launches a manager", "; press r") and "Press n to ..." sentences.
 * Ctrl chords, Enter, Esc and arrows stay. Text with no such key comes back unchanged.
 */
export function formatKeyTip(text: string): string {
  const lines = text.split('\n').map((line) => {
    const sentences = stripPressAlt(line).replace(/\bPress [A-Za-z] (?:to|again) [^.]*(?:\.\s*|$)/g, '');
    return paletteCommands(stripKeyClauses(stripTipParens(sentences))).trim();
  });
  return lines.join('\n').trim();
}

// ---------- next steps, both ways (local #208) ----------

/** One next step: a button to click or a menu path to choose, and the Vim key that does the same. */
export interface NextStep {
  /** A visible button's label ("Add peer"). */
  readonly click?: string;
  /** A menu path ("Actions… › New session") when there is no button on screen. */
  readonly menu?: string;
  /** A plain-words action with no button to click ("Delete the node"): its keys follow in "(...)", and it is dropped when only Vim keys do it. */
  readonly do?: string;
  /** The key (or keys, "j / k") with Vim keys on. */
  readonly key?: string;
  /** What the step does, after it: "Click Run (or press r) to run the workflow". */
  readonly to?: string;
}

/** Every token of a "(or press ...)" key list is a key the Vim keys switch blocks. */
function isVimAlt(keys: string): boolean {
  return keys.split(/\s*\/\s*|\s+or\s+|\s+then\s+/).every((k) => isVimTipKey(k) || isVimKey(k));
}

/** "Click Add (or press a)" -> "Click Add" with Vim keys off; "(or press Enter)" stays (Enter works either way). */
function stripPressAlt(text: string): string {
  return text.replace(/\s*\(or press ([^()]+)\)/g, (all, keys: string) => (isVimAlt(keys) ? '' : all));
}

/**
 * The one hint formatter every empty state and hint uses (local #208): the visible action
 * first, then the key in secondary text when Vim keys are on.
 *   nextStep({ click: 'Add peer', key: 'a' }, true)  -> 'Click Add peer (or press a)'
 *   nextStep({ click: 'Add peer', key: 'a' }, false) -> 'Click Add peer'
 *   nextStep({ menu: 'Settings › Budgets' }, false)   -> 'Choose Settings › Budgets'
 * The on-form is what a page renders: the key-hints DOM pass (formatKeyTip / formatKeyHint)
 * turns it into the off-form when the switch is off, so a live switch flip needs no re-render.
 */
export function nextStep(step: NextStep, vimOn = true, platform?: Platform): string {
  const key = step.key && platform ? proseLabel(step.key, platform) : step.key;
  const to = step.to ? ` to ${step.to}` : '';
  if (step.do) {
    // A plain action: with Vim keys off it survives only when a non-Vim key (Enter, Esc, Tab) does it.
    if (!step.key) return `${step.do}${to}`;
    return vimOn || !isVimAlt(step.key) ? `${step.do} (${key})${to}` : '';
  }
  const head = step.click ? `Click ${step.click}` : step.menu ? `Choose ${step.menu}` : '';
  const alt = key ? `(or press ${key})` : '';
  const on = head === '' ? (key ? `Press ${key}` : '') : alt === '' ? head : `${head} ${alt}`;
  const shown = vimOn ? on : head === '' ? '' : stripPressAlt(on);
  return shown === '' ? '' : shown + to;
}

/** A footer or side hint: several next steps, plain words first, keys secondary, joined with " · " (a step with nothing left is skipped). */
export function hintLine(steps: readonly NextStep[], vimOn = true, platform?: Platform): string {
  return steps.map((s) => nextStep(s, vimOn, platform)).filter((t) => t !== '').join(' · ');
}

/** The text of a node under the key-text rule `kind`, for Vim keys off. */
export function offText(kind: 'button' | 'hint' | 'tip', text: string): string {
  return kind === 'button' ? stripKeyLabel(text) : kind === 'tip' ? formatKeyTip(text) : formatKeyHint(text);
}

export const BUTTON_SELECTOR = 'button, [role="button"]';
export const HINT_SELECTOR = '.pick-hint, .mgr-hints, .dialog-note, [data-key-hint]';
/** Prose that is not a key list: the toast and empty-state lines. */
export const TIP_SELECTOR = '#toast, .pick-empty, [data-key-tip]';

interface TextRecord { readonly orig: string; readonly shown: string }

/** Which rule applies to a text node, by the element it sits in. */
function kindOf(node: Text): 'button' | 'hint' | 'tip' | null {
  const p = node.parentElement;
  if (!p) return null;
  if (p.closest('[data-key-plain]')) return null; // text already written for Vim keys off (local #246)
  if (p.closest(BUTTON_SELECTOR)) return 'button';
  if (p.closest(HINT_SELECTOR)) return 'hint';
  if (p.closest(TIP_SELECTOR)) return 'tip';
  return null;
}

/**
 * Keep every key hint and button label on the page in step with the switch. `isOn` is read
 * per pass; the returned function re-applies after the switch flips. A DOM-only module:
 * the logic is formatKeyHint / stripKeyLabel above (unit-tested).
 */
export function installKeyHints(root: HTMLElement, isOn: () => boolean): () => void {
  const records = new WeakMap<Text, TextRecord>();
  const titles = new WeakMap<Element, TextRecord>();
  const doc = root.ownerDocument;
  const settle = (node: Text): void => {
    const kind = kindOf(node);
    const rec = records.get(node);
    if (!kind && !rec) return;
    const orig = rec && node.data === rec.shown ? rec.orig : node.data;
    const shown = !kind || isOn() ? orig : offText(kind, orig);
    if (shown !== node.data) node.data = shown;
    records.set(node, { orig, shown });
  };
  // A title="..." tooltip follows the same switch: the original is kept, an empty tooltip
  // (nothing left of it) shows nothing, and a tooltip the page rewrites is re-read.
  const settleTitle = (el: Element): void => {
    const rec = titles.get(el);
    const now = el.getAttribute('title');
    if (now === null && !rec) return;
    const orig = rec && (now ?? '') === rec.shown ? rec.orig : (now ?? '');
    const shown = isOn() ? orig : formatKeyTip(orig);
    if (shown !== (now ?? '')) el.setAttribute('title', shown);
    titles.set(el, { orig, shown });
  };
  const walk = (from: Node): void => {
    if (from.nodeType === 3) { settle(from as Text); return; }
    if (from.nodeType !== 1) return;
    const tw = doc.createTreeWalker(from, 4 /* NodeFilter.SHOW_TEXT */);
    for (let n = tw.nextNode(); n; n = tw.nextNode()) settle(n as Text);
    const el = from as Element;
    settleTitle(el);
    el.querySelectorAll('[title]').forEach(settleTitle);
  };
  new MutationObserver((muts) => {
    for (const m of muts) {
      if (m.type === 'characterData') walk(m.target);
      else if (m.type === 'attributes') settleTitle(m.target as Element);
      else m.addedNodes.forEach(walk);
    }
  }).observe(root, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ['title'] });
  walk(root);
  // The switch is also exposed as an attribute on <html> for CSS (data-vim-keys="on" | "off").
  return () => {
    doc.documentElement.dataset.vimKeys = isOn() ? 'on' : 'off';
    walk(root);
  };
}
