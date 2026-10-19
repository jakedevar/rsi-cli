// The file editor's buffer rules (#113, hub #3012), pure (no DOM): line endings,
// drafts, the save request and the TUI's changed-on-disk decisions. The DOM side
// is files.ts. Saving goes through rsid's WriteProjectFile only: the desktop
// sends the version ReadProjectFile answered, rsid refuses a stale one
// (`project_file_stale`) unless `force`, and the new version comes back in the
// answer. Mirrors crates/rsi/src/file_viewer.rs `save_file`.
import type { FileRoot } from './files-model.js';

/** Largest text rsid accepts in one save (`MAX_WRITE_BYTES`). */
export const MAX_WRITE_BYTES = 8 << 20;

/**
 * A file that mixes line endings (or has a lone CR): the break after each line
 * is remembered, so a save writes every unchanged line's own ending back.
 */
export interface MixedEol {
  readonly kind: 'mixed';
  /** The break after line i (`\n`, `\r\n` or `\r`), for i = 0..lines-2. */
  readonly seps: readonly string[];
  /** The buffer (LF breaks) the breaks belong to: new text is matched against it. */
  readonly base: string;
}

export type Eol = 'lf' | 'crlf' | MixedEol;

const BREAK = /\r\n|\n|\r/g;

/**
 * The line-ending style of a file: `lf`, `crlf`, or a MixedEol that keeps each
 * break. A text box normalizes every break to LF, so the editor holds LF text
 * and writes the breaks back on save (a mixed file keeps each line's own).
 */
export function eolOf(text: string): Eol {
  const seps = text.match(BREAK) ?? [];
  if (seps.every((s) => s === '\n')) return 'lf';
  if (seps.every((s) => s === '\r\n')) return 'crlf';
  return { kind: 'mixed', seps, base: text.replace(BREAK, '\n') };
}

/** The disk text as the editor's buffer holds it (LF breaks). */
export const toBuffer = (disk: string, eol: Eol): string => (eol === 'lf' ? disk : disk.replace(BREAK, '\n'));

/** Lines at the start and end that `a` and `b` share (never overlapping). */
export function commonEnds(a: readonly string[], b: readonly string[]): { prefix: number; suffix: number } {
  const max = Math.min(a.length, b.length);
  let prefix = 0;
  while (prefix < max && a[prefix] === b[prefix]) prefix++;
  let suffix = 0;
  while (suffix < max - prefix && a[a.length - 1 - suffix] === b[b.length - 1 - suffix]) suffix++;
  return { prefix, suffix };
}

/** The breaks of a mixed file for a new buffer: kept line by line where the text still matches the old, else the line above's. */
function mixedBreaks(eol: MixedEol, lines: readonly string[]): string[] {
  const old = eol.base.split('\n');
  const { prefix, suffix } = commonEnds(old, lines);
  const middleSame = old.length - prefix - suffix === lines.length - prefix - suffix;
  const dominant = ['\n', '\r\n', '\r'].map((s) => [s, eol.seps.filter((x) => x === s).length] as const).sort((x, y) => y[1] - x[1])[0]![0];
  const out: string[] = [];
  for (let j = 0; j < lines.length - 1; j++) {
    let sep: string | undefined;
    if (j < prefix) sep = eol.seps[j];
    else if (j >= lines.length - suffix) sep = eol.seps[old.length - (lines.length - j)];
    else if (middleSame) sep = eol.seps[j];
    out.push(sep ?? out[j - 1] ?? dominant);
  }
  return out;
}

/** The buffer as it is written (its original line endings). */
export function toDisk(buffer: string, eol: Eol): string {
  if (eol === 'lf') return buffer;
  if (eol === 'crlf') return buffer.replace(/\n/g, '\r\n');
  const lines = buffer.split('\n');
  const seps = mixedBreaks(eol, lines);
  let out = '';
  for (let i = 0; i < lines.length; i++) out += i < lines.length - 1 ? lines[i]! + seps[i]! : lines[i]!;
  return out;
}

/** Why a file cannot be edited here, or null when it can. */
export function editRefusal(read: { binary: boolean; truncated: boolean; content?: string; version?: string }, canReadMore: boolean): string | null {
  if (read.binary) return 'A binary file is not edited here';
  if (read.truncated) {
    return canReadMore
      ? 'Only the start of this file is loaded: press "Read up to 4 MiB" first, then edit'
      : 'This file is too large to edit here (over 4 MiB); edit it in your editor';
  }
  if (read.content === undefined) return 'The file text was not loaded';
  if (!read.version) return 'rsid could not fingerprint the file (it is over 64 MiB or changed while it was read), so a save could not be checked; reload it with :e!';
  return null;
}

/** One file's unsaved buffer, kept while another file is open (the TUI's file_viewer_cache). */
export interface Draft {
  path: string;
  /** The buffer text (LF breaks). */
  text: string;
  /** The disk text (LF breaks) and version the buffer started from. */
  base: string;
  version: string;
  eol: Eol;
  /** The disk changed under the buffer (a stale save, or a newer read): `:w!` overwrites, `:e!` reloads. */
  conflict: boolean;
  /** The caret (UTF-16 offset in the buffer) when the draft was set aside. */
  cursor: number;
}

export const isDirty = (d: Pick<Draft, 'text' | 'base'>): boolean => d.text !== d.base;

/** Drafts by root and path. */
export class DraftStore {
  private readonly m = new Map<string, Draft>();
  private static key(root: string, path: string): string { return `${root}\u0000${path}`; }
  put(root: string, d: Draft): void { this.m.set(DraftStore.key(root, d.path), d); }
  get(root: string, path: string): Draft | undefined { return this.m.get(DraftStore.key(root, path)); }
  drop(root: string, path: string): void { this.m.delete(DraftStore.key(root, path)); }
  /** Paths with unsaved changes under `root`. */
  dirty(root: string): Set<string> {
    const out = new Set<string>();
    for (const [k, d] of this.m) if (k.startsWith(`${root}\u0000`) && isDirty(d)) out.add(d.path);
    return out;
  }
  anyDirty(): boolean {
    for (const d of this.m.values()) if (isDirty(d)) return true;
    return false;
  }
  /** `from` moved to `to` (a file, or a folder holding drafts): point the drafts at the new paths. */
  retarget(root: string, from: string, to: string): void {
    const moved: Draft[] = [];
    for (const [k, d] of [...this.m]) {
      if (!k.startsWith(`${root}\u0000`)) continue;
      if (d.path === from || d.path.startsWith(`${from}/`)) {
        this.m.delete(k);
        moved.push({ ...d, path: d.path === from ? to : `${to}${d.path.slice(from.length)}` });
      }
    }
    for (const d of moved) this.put(root, d);
  }
}

/** `WriteProjectFile` params: the buffer in its own line endings, fenced by the version it started from. */
export function saveRequest(root: FileRoot, path: string, buffer: string, eol: Eol, version: string | null, force: boolean): Record<string, unknown> {
  return {
    ...root, path, content: toDisk(buffer, eol),
    ...(version ? { expected_version: version } : {}),
    ...(force ? { force: true } : {}),
  };
}

/** The save text is over rsid's limit (UTF-8 bytes). */
export function tooLargeToSave(buffer: string, eol: Eol): boolean {
  return new TextEncoder().encode(toDisk(buffer, eol)).length > MAX_WRITE_BYTES;
}

// ---------- the TUI's save messages ----------

export const savedText = (path: string, forced: boolean, created: boolean): string =>
  `Saved: ${path}${created ? ' (created)' : forced ? ' (overwrote the disk copy)' : ''}`;
export const upToDateText = (path: string): string => `Already up to date: ${path}`;
export const refusedChangedText = (path: string): string =>
  `Save refused — file changed on disk: ${path} (use :w! to overwrite or :e! to reload)`;
export const refusedUnverifiedText = (path: string): string =>
  `Save refused — cannot verify file state: ${path} (use :w! to overwrite or :e! to reload)`;
export const failedText = (path: string, why: string): string => `Save failed: ${path}: ${why}`;
export const UNSAVED = 'No write since last change (use :q! to override)';
export const UNSAVED_RELOAD = 'No write since last change (use :e! to discard it)';

/** What a stale save does after the file is read again (the TUI compares the disk text with the buffer). */
export type StaleVerdict =
  | { kind: 'up-to-date'; version: string }
  | { kind: 'recreate' }
  | { kind: 'refused' }
  | { kind: 'unverified' };

/**
 * A save was refused `project_file_stale`: decide from a fresh read of the file.
 * `read` is what ReadProjectFile answered (null when it failed, with the refusal
 * `code`). The buffer matching the disk loses nothing, so the version is taken
 * and no write happens; a deleted file is written again (a user-initiated save
 * creates it, as the TUI does); anything else is a real conflict, or cannot be
 * checked.
 */
export function staleVerdict(
  buffer: string, eol: Eol,
  read: { binary: boolean; truncated: boolean; content?: string; version?: string } | null, code: string | null,
): StaleVerdict {
  if (read === null) return code === 'project_file_not_found' ? { kind: 'recreate' } : { kind: 'unverified' };
  if (read.binary || read.truncated || read.content === undefined) return { kind: 'unverified' };
  if (read.content === toDisk(buffer, eol) && read.version) return { kind: 'up-to-date', version: read.version };
  return { kind: 'refused' };
}

/**
 * The offset to hand the vim editor so that leaving insert mode (which steps
 * one column left) lands on `offset` itself: a normal-mode edit key started
 * from the viewer acts on the character under its cursor, as in the TUI.
 */
export function normalEntryOffset(text: string, offset: number): number {
  const lineStart = text.lastIndexOf('\n', offset - 1) + 1;
  const col = offset - lineStart;
  const next = text[offset];
  return col > 0 && next !== undefined && next !== '\n' ? offset + 1 : offset;
}

/** The offset of the start of 1-based `line` (clamped to the text), for `:<n>` in the editor. */
export function lineOffset(text: string, line: number): number {
  let at = 0;
  for (let i = 1; i < line; i++) {
    const nl = text.indexOf('\n', at);
    if (nl < 0) return at;
    at = nl + 1;
  }
  return at;
}

/** 0-based line and column (UTF-16 units) of `offset`, for the status text. */
export function lineCol(text: string, offset: number): { line: number; col: number } {
  const head = text.slice(0, offset);
  const line = (head.match(/\n/g) ?? []).length;
  return { line, col: offset - (head.lastIndexOf('\n') + 1) };
}
