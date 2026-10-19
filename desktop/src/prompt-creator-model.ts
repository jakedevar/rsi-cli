// Pure rules of the prompt creator (local #88; hub #3040): the flat .md name
// checks, the list keys of prompt_creator_keys.rs and the display helpers. No
// DOM; tested in tests/promptcreator.test.mjs. The DOM side is prompt-creator.ts,
// the Tauri commands src-tauri/src/prompt_files.rs.

/** `PromptFileEntryV1`. */
export interface PromptEntry { name: string; first_line: string; size: number; mtime?: string | null }

export const NEW_PROMPT_CONTENT = '# New Prompt\n\n';

/** `new_prompt_name`: `prompt-YYYYMMDD-HHMMSS.md` (UTC). */
export function newPromptName(nowMs: number): string {
  const d = new Date(nowMs);
  const p = (n: number, w = 2): string => String(n).padStart(w, '0');
  return `prompt-${p(d.getUTCFullYear(), 4)}${p(d.getUTCMonth() + 1)}${p(d.getUTCDate())}-${p(d.getUTCHours())}${p(d.getUTCMinutes())}${p(d.getUTCSeconds())}.md`;
}

/** The daemon's `validate_prompt_name`: the reason a name is refused, or null. */
export function promptNameError(name: string): string | null {
  if (!name) return 'the name is empty';
  if (new TextEncoder().encode(name).length > 200) return 'the name is longer than 200 bytes';
  if (/[/\\]/.test(name)) return 'the name contains a path separator; name a flat file';
  if (name.includes('..')) return "the name contains '..'";
  if (name.includes(':')) return "the name contains ':'";
  if (/[\u0000-\u001f\u007f-\u009f]/.test(name)) return 'the name contains a control character';
  if (name.startsWith('.')) return "the name starts with '.'";
  if (name.trim() !== name) return 'the name starts or ends with whitespace';
  if (!name.endsWith('.md')) return 'the name does not end in .md';
  if (name === '.md') return 'the name has no stem before .md';
  return null;
}

/** `relative_time`. */
export function relativeTime(mtime: string | null | undefined, nowMs: number): string {
  const t = mtime ? Date.parse(mtime) : NaN;
  if (!Number.isFinite(t)) return '';
  const secs = Math.max(0, Math.floor((nowMs - t) / 1000));
  if (secs < 60) return 'just now';
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

/** `format_size`. */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}

export type PromptListKey = 'down' | 'up' | 'open' | 'new' | 'delete' | 'model' | 'close';

/** The prompt list's keys (prompt_creator_keys.rs `handle_list_key`). */
export function promptListKey(key: string, ctrl: boolean): PromptListKey | null {
  if (ctrl) return key === 'n' || key === 'N' ? 'new' : null;
  switch (key) {
    case 'j': case 'ArrowDown': return 'down';
    case 'k': case 'ArrowUp': return 'up';
    case 'Enter': return 'open';
    // `n` is a desktop alias of Ctrl+N (which a desktop menu accelerator may take).
    case 'n': return 'new';
    case 'd': return 'delete';
    case 'm': return 'model';
    case 'q': case 'Escape': return 'close';
    default: return null;
  }
}

/** The list cursor after a move (clamped; the TUI does not wrap). */
export function movePromptCursor(at: number, len: number, dir: 1 | -1): number {
  if (len === 0) return 0;
  return Math.max(0, Math.min(len - 1, at + dir));
}

/** Where the cursor goes after the list reloads: the named prompt, else clamped. */
export function promptCursorAfter(files: readonly PromptEntry[], name: string | null, at: number): number {
  const i = name ? files.findIndex((f) => f.name === name) : -1;
  if (i >= 0) return i;
  return Math.max(0, Math.min(files.length - 1, at));
}
