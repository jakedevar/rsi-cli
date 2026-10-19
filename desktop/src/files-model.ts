// Project files (slice 10B and #113, hub #3012): the pure side of the file
// explorer, the fuzzy file picker and the file viewer/editor. No DOM; tested
// in tests/files.test.mjs. The DOM side is files.ts (RUNTIME in
// actions/files-run.ts); it reaches files only through rsid's
// project-root-scoped methods (files-api.ts, src-tauri/src/files.rs). The
// editor's buffer rules are files-edit-model.ts, the explorer's add / rename /
// move / copy / delete / undo prompts files-ops-model.ts.
//
// Keys follow docs/keybindings.md "File Explorer Overlay", "Telescope File
// Picker" and "File Viewer" (crates/rsi/src/overlay/file_explorer, telescope.rs,
// file_viewer.rs, git_gutter.rs).

import type { ColMotion } from './viewer-model.js';

// ---------- wire shapes (crates/rsi-common/src/project_files.rs) ----------

/** The root a request names: a project's `path` or a session's `working_dir`. */
export type FileRoot = { project_id: string } | { session_id: string };

export type EntryKind = 'file' | 'dir' | 'symlink';

export interface DirEntry {
  name: string;
  /** Root-relative, `/`-separated. */
  path: string;
  kind: EntryKind;
  size: number;
  /** RFC3339 UTC; absent when the platform cannot say. */
  mtime?: string;
  /** The name starts with `.`. */
  hidden: boolean;
  /** Git ignores it. */
  ignored: boolean;
}

export interface DirListing { root: string; path: string; entries: DirEntry[]; truncated: boolean }
export interface FileList { root: string; paths: string[]; truncated: boolean; source: 'git' | 'walk' }
export interface FileRead {
  root: string;
  path: string;
  size: number;
  binary: boolean;
  content?: string;
  truncated: boolean;
  start_line?: number;
  end_line?: number;
  /**
   * The opaque token `WriteProjectFile.expected_version` fences a save with
   * (`v1:<size>:<mtime>:<sha256>`); absent for a file over 64 MiB or one that
   * changed while it was read.
   */
  version?: string;
}

export type OperationKind = 'created' | 'moved' | 'copied' | 'trashed';
/** `WriteProjectFile` answer. */
export interface WriteResult { root: string; path: string; size: number; version: string; forced: boolean; created: boolean }
/** `CreateProjectEntry`, `MoveProjectEntry`, `CopyProjectEntry` and `TrashProjectEntry` answer. */
export interface OperationResult {
  root: string;
  op: OperationKind;
  /** Created: the requested path; moved/copied: the source; trashed: the original path. */
  path: string;
  /** Moved/copied: the destination. */
  to?: string;
  /** Created: the top-most entry that did not exist before. */
  created_top?: string;
  undo_depth: number;
}
/** `UndoProjectFileOperation` answer. */
export interface UndoResult {
  root: string;
  undone: OperationKind;
  effect: 'restored' | 'moved_back' | 'removed' | 'trashed';
  path: string;
  /** Moved: the path the entry moved back from. */
  from?: string;
  undo_depth: number;
}
export type GutterStatus = 'tracked' | 'untracked' | 'ignored' | 'binary' | 'too_large' | 'not_a_repo';
export type MarkerKind = 'added' | 'modified' | 'removed';
export interface GutterMarker { line: number; kind: MarkerKind }
export interface Gutter { root: string; path: string; status: GutterStatus; markers: GutterMarker[] }

/** Caps the desktop asks for (rsid's defaults; the ceilings are in files.rs). */
export const DIRECTORY_ENTRIES = 2_000;
export const PICKER_PATHS = 20_000;
export const READ_BYTES = 1 << 20;
export const MAX_READ_BYTES = 4 << 20;
/** Results the picker lists, as the TUI's telescope (`rank_finder(.., 100)`). */
export const PICKER_LIMIT = 100;

// ---------- which root ----------

export interface RootSession { id: string; project_id?: string | null; working_dir: string }
export interface RootProject { id: string; name: string; path?: string | null }

export type RootChoice =
  | { ok: true; root: FileRoot; key: string; label: string }
  | { ok: false; why: string };

export const NO_ROOT = 'Open a session or filter the list to a project with a folder first';

const base = (p: string): string => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || p;

/**
 * The tree's root, as the TUI's `current_project().path`: the open session's
 * project when it has a folder, else the open session's working directory,
 * else the project the list is filtered to.
 */
export function chooseRoot(
  session: RootSession | undefined, projects: readonly RootProject[], filterProjectId: string | null,
): RootChoice {
  const project = (id: string | null | undefined) => (id ? projects.find((p) => p.id === id && !!p.path?.trim()) : undefined);
  const own = project(session?.project_id);
  if (own) return { ok: true, root: { project_id: own.id }, key: `p:${own.id}`, label: own.name || base(own.path!) };
  if (session && session.working_dir.trim()) {
    return { ok: true, root: { session_id: session.id }, key: `s:${session.id}`, label: base(session.working_dir) };
  }
  const filtered = project(filterProjectId);
  if (filtered) return { ok: true, root: { project_id: filtered.id }, key: `p:${filtered.id}`, label: filtered.name || base(filtered.path!) };
  return { ok: false, why: NO_ROOT };
}

// ---------- calm refusals ----------

const REFUSALS: Readonly<Record<string, string>> = {
  project_file_outside_root: 'That path is outside the project folder, so rsid will not open it.',
  project_file_not_found: 'That file or folder is no longer there. Refresh the tree (R).',
  project_file_not_a_directory: 'That path is not a folder.',
  project_file_not_a_file: 'That is not a regular file (a folder, a device or a link to one).',
  project_file_unknown_root: 'rsid has no folder for this project or session (it is unknown, removed, or has no path).',
  project_file_invalid_request: 'rsid refused the request.',
};

/** The refusal code of a daemon error, or null. */
export function refusalCode(err: unknown): string | null {
  const raw = err instanceof Error ? err.message : String(err);
  return raw.match(/project_file_[a-z_]+/)?.[0] ?? null;
}

/** What follows the refusal code in the daemon's message ("" when nothing does). */
export function refusalDetail(err: unknown): string {
  const raw = err instanceof Error ? err.message : String(err);
  const code = raw.match(/project_file_[a-z_]+/)?.[0];
  if (!code) return '';
  return raw.slice(raw.indexOf(code) + code.length).replace(/^[:\s]+/, '').replace(/\s*\[[a-z_]+\]$/, '').trim();
}

/**
 * A refused or failed file change as one calm sentence, in the TUI's words
 * where it has them ("<name> already exists", "Path must stay inside <root>",
 * "Nothing to undo"). `name` is the entry the user named; `rootName` the
 * project folder's name.
 */
export function writeErrorText(err: unknown, ctx: { name?: string; rootName?: string } = {}): string {
  const code = refusalCode(err);
  const detail = refusalDetail(err);
  const name = ctx.name ?? 'That entry';
  switch (code) {
    case 'project_file_stale': return 'The file changed on disk since it was opened.';
    case 'project_file_conflict':
      return `Not completed: something changed while rsid worked, so it stopped. Nothing was lost.${detail ? ` ${detail}` : ''}`;
    case 'project_file_exists': return `${name} already exists`;
    case 'project_file_outside_root': return `Path must stay inside ${ctx.rootName ?? 'the project folder'}`;
    case 'project_file_too_large': return 'The text is larger than the 8 MiB rsid accepts for a save.';
    case 'project_file_nothing_to_undo': return 'Nothing to undo';
    case 'project_file_undo_conflict': return `Undo failed: ${detail || 'the entry changed since, so it was left as it is'}`;
    case 'project_file_io_failed': return `The change failed: ${detail || 'the operating system refused it'}`;
    case 'project_file_unsupported_platform': return 'Changing files is not supported on this platform.';
    case 'project_file_invalid_request': return `rsid refused the request.${detail ? ` ${detail}` : ''}`;
    case 'project_file_not_found': return 'That file or folder is no longer there. Refresh the tree (R).';
    case 'project_file_not_a_file': return 'That is not a regular file (a folder, a device or a link to one).';
    case 'project_file_not_a_directory': return 'That path is not a folder.';
    case 'project_file_unknown_root': return 'rsid has no folder for this project or session (it is unknown, removed, or has no path).';
    default: break;
  }
  const raw = (err instanceof Error ? err.message : String(err)).replace(/^daemon error -?\d+:\s*/, '');
  if (/-32601|method not found|unknown method/i.test(raw)) return 'This rsid does not change project files yet. Update and restart the daemon.';
  if (/cannot reach rsid/i.test(raw)) return 'rsid is not running, so files cannot be changed.';
  return `The change failed: ${raw}`;
}

/** A file error as a calm sentence (the refusal code picks it; the detail follows). */
export function fileErrorText(err: unknown): string {
  const raw = err instanceof Error ? err.message : String(err);
  const code = raw.match(/project_file_[a-z_]+/)?.[0];
  if (code && REFUSALS[code]) {
    const detail = raw.slice(raw.indexOf(code) + code.length).replace(/^[:\s]+/, '').replace(/\s*\[[a-z_]+\]$/, '');
    return detail && code === 'project_file_invalid_request' ? `${REFUSALS[code]} ${detail}` : REFUSALS[code]!;
  }
  if (/-32601|method not found|unknown method/i.test(raw)) {
    return 'This rsid does not serve project files yet. Update and restart the daemon.';
  }
  if (/cannot reach rsid/i.test(raw)) return 'rsid is not running, so files cannot be read.';
  return `Could not read files: ${raw.replace(/^daemon error -?\d+:\s*/, '')}`;
}

// ---------- the explorer tree ----------

export type DirStatus = 'loading' | 'loaded' | 'error';
interface DirNode { status: DirStatus; entries: DirEntry[]; truncated: boolean; error?: string }

export type TreeRow =
  | { kind: 'entry'; entry: DirEntry; depth: number; expanded: boolean }
  | { kind: 'note'; depth: number; tone: 'loading' | 'error' | 'truncated' | 'empty'; text: string };

/** What a tree key asks the DOM side to do next. */
export type TreeEffect =
  | { kind: 'none' }
  | { kind: 'load'; path: string }
  | { kind: 'open'; path: string };

const NONE: TreeEffect = { kind: 'none' };
const parentOf = (path: string): string => (path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '');

/** Dirs first, then name, case-insensitive (rsid sends this order; kept for merges). */
export function sortEntries(entries: readonly DirEntry[]): DirEntry[] {
  return [...entries].sort((a, b) => {
    const da = a.kind === 'dir' ? 0 : 1;
    const db = b.kind === 'dir' ? 0 : 1;
    return da - db || a.name.toLowerCase().localeCompare(b.name.toLowerCase()) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
  });
}

/**
 * The explorer's tree: directories load lazily (one `ListProjectDirectory`
 * each), hidden entries are left out unless shown (`.` / `H`, off by default
 * as in the TUI), ignored ones stay and are dimmed.
 */
export class FileTree {
  readonly dirs = new Map<string, DirNode>();
  readonly expanded = new Set<string>();
  showHidden = false;
  /** Path of the selected entry. */
  selected: string | null = null;

  /** The root (`""`) and every expanded directory: what a refresh re-reads. */
  loadedDirs(): string[] {
    return ['', ...[...this.expanded].sort()].filter((p) => this.dirs.has(p));
  }

  setLoading(path: string): void {
    const had = this.dirs.get(path);
    this.dirs.set(path, { status: 'loading', entries: had?.entries ?? [], truncated: had?.truncated ?? false });
  }

  setListing(path: string, listing: Pick<DirListing, 'entries' | 'truncated'>): void {
    this.dirs.set(path, { status: 'loaded', entries: sortEntries(listing.entries), truncated: listing.truncated });
    this.fixSelection();
  }

  setError(path: string, error: string): void {
    this.dirs.set(path, { status: 'error', entries: [], truncated: false, error });
    if (path !== '') this.fixSelection();
  }

  private visible(e: DirEntry): boolean {
    return this.showHidden || !e.hidden;
  }

  rows(): TreeRow[] {
    const out: TreeRow[] = [];
    const walk = (path: string, depth: number) => {
      const node = this.dirs.get(path);
      if (!node) return;
      if (node.status === 'loading' && node.entries.length === 0) {
        out.push({ kind: 'note', depth, tone: 'loading', text: 'Loading…' });
        return;
      }
      if (node.status === 'error') {
        out.push({ kind: 'note', depth, tone: 'error', text: node.error ?? 'Could not list this folder' });
        return;
      }
      const shown = node.entries.filter((e) => this.visible(e));
      for (const e of shown) {
        const open = e.kind === 'dir' && this.expanded.has(e.path);
        out.push({ kind: 'entry', entry: e, depth, expanded: open });
        if (open) walk(e.path, depth + 1);
      }
      if (node.truncated) out.push({ kind: 'note', depth, tone: 'truncated', text: 'More entries not shown (listing capped)' });
      else if (shown.length === 0) {
        const hidden = node.entries.length - shown.length;
        out.push({ kind: 'note', depth, tone: 'empty', text: hidden > 0 ? `${hidden} hidden (. shows them)` : 'Empty' });
      }
    };
    walk('', 0);
    return out;
  }

  /** The selectable rows (entries), in display order. */
  entries(): Array<Extract<TreeRow, { kind: 'entry' }>> {
    return this.rows().filter((r): r is Extract<TreeRow, { kind: 'entry' }> => r.kind === 'entry');
  }

  selectedEntry(): DirEntry | null {
    return this.entries().find((r) => r.entry.path === this.selected)?.entry ?? null;
  }

  private index(): number {
    return this.entries().findIndex((r) => r.entry.path === this.selected);
  }

  /** Keep the selection on a visible entry: the nearest visible ancestor, else the first. */
  fixSelection(): void {
    const list = this.entries();
    if (list.length === 0) { this.selected = null; return; }
    let p = this.selected;
    while (p !== null && p !== '') {
      if (list.some((r) => r.entry.path === p)) { this.selected = p; return; }
      p = parentOf(p);
    }
    this.selected = list[0]!.entry.path;
  }

  /** `j`/`k` (a count of rows), clamped; `g`/`G` with ±Infinity. */
  move(delta: number): void {
    const list = this.entries();
    if (list.length === 0) return;
    const i = this.index();
    const from = i < 0 ? 0 : i;
    const to = delta === Infinity ? list.length - 1 : delta === -Infinity ? 0 : Math.max(0, Math.min(list.length - 1, from + delta));
    this.selected = list[to]!.entry.path;
  }

  private expand(path: string): TreeEffect {
    this.expanded.add(path);
    const node = this.dirs.get(path);
    if (!node || node.status === 'error') { this.setLoading(path); return { kind: 'load', path }; }
    return NONE;
  }

  /** `Enter` / `o`: open a file, or expand / collapse a directory. */
  activate(): TreeEffect {
    const e = this.selectedEntry();
    if (!e) return NONE;
    if (e.kind !== 'dir') return { kind: 'open', path: e.path };
    if (this.expanded.has(e.path)) { this.expanded.delete(e.path); return NONE; }
    return this.expand(e.path);
  }

  /** `l` / `Right`: expand a directory, step into it when already expanded, or open a file. */
  openNode(): TreeEffect {
    const e = this.selectedEntry();
    if (!e) return NONE;
    if (e.kind !== 'dir') return { kind: 'open', path: e.path };
    if (!this.expanded.has(e.path)) return this.expand(e.path);
    const list = this.entries();
    const i = this.index();
    const next = list[i + 1];
    if (next && next.depth > list[i]!.depth) this.selected = next.entry.path;
    return NONE;
  }

  /** `h` / `Left`: collapse a directory, or collapse the parent and select it. */
  closeNode(): void {
    const e = this.selectedEntry();
    if (!e) return;
    if (e.kind === 'dir' && this.expanded.has(e.path)) { this.expanded.delete(e.path); return; }
    const parent = parentOf(e.path);
    if (parent === '') return;
    this.expanded.delete(parent);
    this.selected = parent;
  }

  /** `z`: collapse every directory; the selection moves to its top-level ancestor. */
  collapseAll(): void {
    this.expanded.clear();
    if (this.selected) this.selected = this.selected.split('/')[0]!;
    this.fixSelection();
  }

  /** `.` / `H`: show or hide dotfiles. */
  toggleHidden(): void {
    this.showHidden = !this.showHidden;
    this.fixSelection();
  }

  /**
   * Expand the ancestors of `path` and select it (opening the explorer on the
   * viewer's file, or the finder's Enter). Returns the ancestor directories
   * that still need a listing, outermost first.
   */
  reveal(path: string): string[] {
    const parts = path.split('/').filter(Boolean);
    const need: string[] = [];
    for (let i = 1; i < parts.length; i++) {
      const dir = parts.slice(0, i).join('/');
      this.expanded.add(dir);
      const node = this.dirs.get(dir);
      if (!node || node.status === 'error') { this.setLoading(dir); need.push(dir); }
    }
    // A hidden ancestor or file is shown once revealed, as the TUI does.
    if (parts.some((p) => p.startsWith('.'))) this.showHidden = true;
    this.selected = parts.join('/');
    return need;
  }
}

// ---------- the picker's fuzzy scorer ----------

// A port of fzy's scorer (github.com/jhawthorn/fzy, MIT): the letters of the
// query in order, preferring consecutive runs and matches after `/`, `-`, `_`,
// space, `.` or a lower-to-upper case change, and fewer gaps. The TUI uses
// skim's V2 matcher (fuzzy_matcher); both rank the same kinds of match first,
// and the exact ordering is pinned in tests/files.test.mjs.
const SCORE_MIN = -Infinity;
const SCORE_MAX = Infinity;
const GAP_LEADING = -0.005;
const GAP_TRAILING = -0.005;
const GAP_INNER = -0.01;
const MATCH_CONSECUTIVE = 1.0;
const MATCH_SLASH = 0.9;
const MATCH_WORD = 0.8;
const MATCH_CAPITAL = 0.7;
const MATCH_DOT = 0.6;
/** Longer candidates match but are not scored (fzy's MATCH_MAX_LEN). */
const MAX_SCORED = 1024;

export interface FuzzyMatch { score: number; hits: number[] }

function bonus(prev: string, cur: string): number {
  if (prev === '/' || prev === '\\') return MATCH_SLASH;
  if (prev === '-' || prev === '_' || prev === ' ') return MATCH_WORD;
  if (prev === '.') return MATCH_DOT;
  if (prev >= 'a' && prev <= 'z' && cur >= 'A' && cur <= 'Z') return MATCH_CAPITAL;
  return 0;
}

/** Smart case: a query with a capital letter matches case-sensitively. */
export function fuzzyMatch(candidate: string, query: string): FuzzyMatch | null {
  const needle = query.replace(/\s+/g, '');
  if (needle === '') return { score: 0, hits: [] };
  const exact = needle !== needle.toLowerCase();
  const fold = (s: string) => (exact ? s : s.toLowerCase());
  const n = fold(needle);
  const h = fold(candidate);
  // Subsequence check first: most candidates fail here.
  let at = 0;
  for (const c of n) {
    at = h.indexOf(c, at);
    if (at < 0) return null;
    at++;
  }
  const N = n.length;
  const M = h.length;
  if (M > MAX_SCORED) return { score: SCORE_MIN, hits: [] };
  if (N === M) return { score: SCORE_MAX, hits: Array.from({ length: N }, (_, i) => i) };
  const bonuses: number[] = [];
  let prev = '/';
  for (let j = 0; j < M; j++) {
    const ch = candidate[j]!;
    bonuses.push(bonus(prev, ch));
    prev = ch;
  }
  const D: Float64Array[] = [];
  const Mx: Float64Array[] = [];
  for (let i = 0; i < N; i++) {
    const d = new Float64Array(M);
    const m = new Float64Array(M);
    let prevScore = SCORE_MIN;
    const gap = i === N - 1 ? GAP_TRAILING : GAP_INNER;
    for (let j = 0; j < M; j++) {
      if (n[i] === h[j]) {
        let score = SCORE_MIN;
        if (i === 0) score = j * GAP_LEADING + bonuses[j]!;
        else if (j > 0) score = Math.max(Mx[i - 1]![j - 1]! + bonuses[j]!, D[i - 1]![j - 1]! + MATCH_CONSECUTIVE);
        d[j] = score;
        prevScore = Math.max(score, prevScore + gap);
        m[j] = prevScore;
      } else {
        d[j] = SCORE_MIN;
        prevScore += gap;
        m[j] = prevScore;
      }
    }
    D.push(d);
    Mx.push(m);
  }
  const hits = new Array<number>(N).fill(0);
  let required = false;
  let j = M - 1;
  for (let i = N - 1; i >= 0; i--) {
    for (; j >= 0; j--) {
      const d = D[i]![j]!;
      if (d !== SCORE_MIN && (required || d === Mx[i]![j]!)) {
        required = i > 0 && j > 0 && Mx[i]![j]! === D[i - 1]![j - 1]! + MATCH_CONSECUTIVE;
        hits[i] = j--;
        break;
      }
    }
  }
  return { score: Mx[N - 1]![M - 1]!, hits };
}

export interface Ranked { index: number; path: string; hits: number[] }

/**
 * The picker's results: the best `limit` matches, best first; ties keep the
 * feed's order (rsid sends git's order). An empty query lists the first
 * `limit` paths as they come, like the TUI.
 */
export function rankPaths(paths: readonly string[], query: string, limit = PICKER_LIMIT): Ranked[] {
  if (query.replace(/\s+/g, '') === '') return paths.slice(0, limit).map((path, index) => ({ index, path, hits: [] }));
  const out: Array<Ranked & { score: number }> = [];
  paths.forEach((path, index) => {
    const m = fuzzyMatch(path, query);
    if (m) out.push({ index, path, hits: m.hits, score: m.score });
  });
  out.sort((a, b) => (b.score === a.score ? a.index - b.index : b.score > a.score ? 1 : -1));
  return out.slice(0, limit).map(({ score: _s, ...r }) => r);
}

/** Move a picker selection, clamped (the TUI's `move_finder_selection`). */
export function movePickerSelection(selected: number, count: number, delta: number): number {
  if (count <= 0) return 0;
  return Math.max(0, Math.min(count - 1, selected + delta));
}

// ---------- the git gutter ----------

export type GutterSign = MarkerKind;
export const GUTTER_CHARS: Readonly<Record<GutterSign, string>> = { added: '+', modified: '~', removed: '-' };

/**
 * One sign per worktree line (index 0 is line 1), as the TUI draws them: `+`
 * added, `~` modified, `-` where lines were deleted. rsid's `removed` marker at
 * line L means "deleted after L" (0: before the first line); the TUI puts the
 * `-` on line L (line 1 for 0). An added or modified sign wins over `-` on the
 * same line. Markers past `lineCount` (a capped read) are dropped.
 */
export function gutterSigns(markers: readonly GutterMarker[], lineCount: number): Array<GutterSign | null> {
  const signs = new Array<GutterSign | null>(Math.max(0, lineCount)).fill(null);
  for (const m of markers) {
    const line = m.kind === 'removed' ? Math.max(1, m.line) : m.line;
    if (!Number.isInteger(line) || line < 1 || line > lineCount) continue;
    if (m.kind === 'removed' && signs[line - 1] !== null) continue;
    signs[line - 1] = m.kind;
  }
  return signs;
}

/** A line for the viewer header when the gutter has nothing to show, else null. */
export function gutterNote(status: GutterStatus): string | null {
  switch (status) {
    case 'tracked': return null;
    case 'untracked': return 'new file (not in HEAD)';
    case 'ignored': return 'ignored by git';
    case 'binary': return 'git calls this change binary';
    case 'too_large': return 'too large for the git gutter';
    case 'not_a_repo': return 'not in a git repository';
  }
}

// ---------- the viewer ----------

/** Split file text into lines; a final newline does not start an empty line. */
export function splitLines(text: string): string[] {
  if (text === '') return [];
  const lines = text.split(/\r?\n/);
  if (lines[lines.length - 1] === '') lines.pop();
  return lines;
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KiB', 'MiB', 'GiB'];
  let v = bytes / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[u]}`;
}

/** The viewer's header states: binary, truncated, or the line count. */
export function readSummary(r: Pick<FileRead, 'size' | 'binary' | 'truncated'>, lines: number, cap: number): string {
  if (r.binary) return `Binary file, ${formatSize(r.size)}: not shown`;
  const head = `${lines} line${lines === 1 ? '' : 's'}, ${formatSize(r.size)}`;
  return r.truncated ? `${head}; truncated at ${formatSize(cap)}` : head;
}

/** Whether a truncated read can ask rsid for more (up to its 4 MiB ceiling). */
export function canReadMore(r: Pick<FileRead, 'truncated' | 'binary'>, cap: number): boolean {
  return r.truncated && !r.binary && cap < MAX_READ_BYTES;
}

export interface ViewPos {
  /** 0-based cursor line. */
  cursor: number;
  /** 0-based first visible line. */
  top: number;
}

export type ViewMove =
  | { by: 'line'; n: number }
  | { by: 'half'; n: number }
  | { by: 'page'; n: number }
  | { by: 'top' }
  | { by: 'bottom' }
  | { by: 'goto'; line: number };

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

/** Keep the cursor on screen and the top inside the file. */
export function settle(pos: ViewPos, lines: number, rows: number): ViewPos {
  const last = Math.max(0, lines - 1);
  const view = Math.max(1, rows);
  const cursor = clamp(pos.cursor, 0, last);
  let top = clamp(pos.top, 0, Math.max(0, lines - view));
  if (cursor < top) top = cursor;
  if (cursor >= top + view) top = cursor - view + 1;
  return { cursor, top };
}

/**
 * Viewer motion. `j`/`k` move the cursor; `Ctrl-d`/`Ctrl-u` move cursor and
 * view half a page, `PageDown`/`Ctrl-f` and `PageUp`/`Ctrl-b` a full page;
 * `gg`/`G` the ends (with a count, that line); `:N` a line.
 */
export function moveView(pos: ViewPos, lines: number, rows: number, m: ViewMove): ViewPos {
  const view = Math.max(1, rows);
  switch (m.by) {
    case 'line': return settle({ cursor: pos.cursor + m.n, top: pos.top }, lines, view);
    case 'half': {
      const step = Math.max(1, Math.floor(view / 2)) * m.n;
      return settle({ cursor: pos.cursor + step, top: pos.top + step }, lines, view);
    }
    case 'page': {
      const step = view * m.n;
      return settle({ cursor: pos.cursor + step, top: pos.top + step }, lines, view);
    }
    case 'top': return settle({ cursor: 0, top: 0 }, lines, view);
    case 'bottom': return settle({ cursor: lines - 1, top: lines }, lines, view);
    case 'goto': return settle({ cursor: m.line - 1, top: m.line - 1 - Math.floor(view / 2) }, lines, view);
  }
}

// ---------- keys ----------

export interface KeyLike { key: string; ctrlKey?: boolean; shiftKey?: boolean; altKey?: boolean; metaKey?: boolean }

export type ViewerCmd =
  | { kind: 'move'; move: ViewMove }
  | { kind: 'col'; motion: ColMotion; n: number }
  | { kind: 'search'; forward: boolean }
  | { kind: 'next-match'; forward: boolean; n: number }
  | { kind: 'star'; forward: boolean }
  | { kind: 'fold'; op: FoldOp }
  | { kind: 'preview' }
  | { kind: 'close' }
  | { kind: 'explorer' }
  | { kind: 'picker' }
  | { kind: 'focus-tree' }
  | { kind: 'back' }
  | { kind: 'cmdline' }
  /** Start editing the file; `how` is the insert key, or `normal` for a normal-mode edit key (`replay` runs that key in the editor). */
  | { kind: 'edit'; how: EditEntry; replay: boolean }
  | { kind: 'save' }
  | { kind: 'say'; text: string }
  | { kind: 'swallow' }
  | { kind: 'pass' };

/** How a key starts the editor: the insert keys, or normal mode. */
export type EditEntry = 'i' | 'a' | 'A' | 'I' | 'o' | 'O' | 'normal';

/** `za` toggle, `zo` open, `zc` close, `zM` close all, `zR` open all. */
export type FoldOp = 'a' | 'o' | 'c' | 'M' | 'R';

/** Ctrl-V is the TUI's visual-block key; the desktop editor has no block selection. */
export const NO_BLOCK_VISUAL = 'Visual-block selection (Ctrl-V) is not in the desktop editor; v and V select';

/** Pending keys of a chord: a count, `g`, `z`, or the `<Space>` leader. */
export interface ViewerKeys { count: string; prefix: '' | 'g' | 'z' | ' ' }
export const NO_VIEWER_KEYS: ViewerKeys = { count: '', prefix: '' };

const COL_KEYS: Readonly<Record<string, ColMotion>> = {
  h: 'h', ArrowLeft: 'h', l: 'l', ArrowRight: 'l', w: 'w', b: 'b', e: 'e', W: 'W', B: 'B', E: 'E',
  '0': '0', Home: '0', '^': '^', $: '$', End: '$', '{': '{', '}': '}', '%': '%',
};

/** Resolve one viewer key (TUI "File Viewer": navigation, search, folds, preview; edit keys start the editor). */
export function viewerKey(st: ViewerKeys, k: KeyLike): { cmd: ViewerCmd; next: ViewerKeys } {
  const done = (cmd: ViewerCmd) => ({ cmd, next: NO_VIEWER_KEYS });
  if (k.metaKey || k.altKey) return done({ kind: 'pass' });
  const count = st.count === '' ? null : Number(st.count);
  const n = count ?? 1;
  if (k.ctrlKey) {
    switch (k.key.toLowerCase()) {
      case 'd': return done({ kind: 'move', move: { by: 'half', n } });
      case 'u': return done({ kind: 'move', move: { by: 'half', n: -n } });
      case 'f': return done({ kind: 'move', move: { by: 'page', n } });
      case 'b': return done({ kind: 'move', move: { by: 'page', n: -n } });
      case 'e': return done({ kind: 'move', move: { by: 'line', n } });
      case 'y': return done({ kind: 'move', move: { by: 'line', n: -n } });
      case 'h': return done({ kind: 'focus-tree' });
      case 's': return done({ kind: 'save' });
      case 'v': return done({ kind: 'say', text: NO_BLOCK_VISUAL });
      default: return done({ kind: 'pass' });
    }
  }
  if (st.prefix === ' ') {
    switch (k.key) {
      case 'q': return done({ kind: 'close' });
      case 'e': return done({ kind: 'explorer' });
      case ' ': return done({ kind: 'picker' });
      case 'm': return done({ kind: 'preview' });
      default: return done({ kind: 'swallow' });
    }
  }
  if (st.prefix === 'g') {
    if (k.key === 'g') return done({ kind: 'move', move: count === null ? { by: 'top' } : { by: 'goto', line: count } });
    return done({ kind: 'swallow' });
  }
  if (st.prefix === 'z') {
    if (k.key === 'a' || k.key === 'o' || k.key === 'c' || k.key === 'M' || k.key === 'R') return done({ kind: 'fold', op: k.key });
    return done({ kind: 'swallow' });
  }
  if (/^[0-9]$/.test(k.key) && !(k.key === '0' && st.count === '')) {
    return { cmd: { kind: 'swallow' }, next: { count: (st.count + k.key).slice(0, 7), prefix: '' } };
  }
  const col = COL_KEYS[k.key];
  if (col) return done({ kind: 'col', motion: col, n });
  switch (k.key) {
    case 'j': case 'ArrowDown': return done({ kind: 'move', move: { by: 'line', n } });
    case 'k': case 'ArrowUp': return done({ kind: 'move', move: { by: 'line', n: -n } });
    case 'PageDown': return done({ kind: 'move', move: { by: 'page', n } });
    case 'PageUp': return done({ kind: 'move', move: { by: 'page', n: -n } });
    case 'G': return done({ kind: 'move', move: count === null ? { by: 'bottom' } : { by: 'goto', line: count } });
    case 'g': return { cmd: { kind: 'swallow' }, next: { count: st.count, prefix: 'g' } };
    case 'z': return { cmd: { kind: 'swallow' }, next: { count: '', prefix: 'z' } };
    case ' ': return { cmd: { kind: 'swallow' }, next: { count: '', prefix: ' ' } };
    case '/': return done({ kind: 'search', forward: true });
    case '?': return done({ kind: 'search', forward: false });
    case 'n': return done({ kind: 'next-match', forward: true, n });
    case 'N': return done({ kind: 'next-match', forward: false, n });
    case '*': return done({ kind: 'star', forward: true });
    case '#': return done({ kind: 'star', forward: false });
    case 'q': return done({ kind: 'close' });
    case 'Backspace': return done({ kind: 'back' });
    case ':': return done({ kind: 'cmdline' });
    case 'i': case 'a': case 'o': case 'O': case 'A': case 'I':
      return done({ kind: 'edit', how: k.key, replay: false });
    // Every other editing key starts the editor in normal mode and runs there.
    case 'x': case 'X': case 'p': case 'P': case 'u': case 'd': case 'c': case 's': case 'S': case 'r': case 'R':
    case 'C': case 'D': case 'J':
      return done({ kind: 'edit', how: 'normal', replay: true });
    case 'Escape': return done({ kind: 'swallow' });
    case 'Tab': case 'Enter': return done({ kind: 'swallow' });
    default: return done(k.key.length === 1 ? { kind: 'swallow' } : { kind: 'pass' });
  }
}

export type ViewerLineCmd =
  /** `:q` (`force` for `:q!`). */
  | { kind: 'quit'; force: boolean }
  | { kind: 'goto'; line: number }
  /** `:w` (`force` for `:w!`); `close` for `:wq` and `:x`. */
  | { kind: 'save'; force: boolean; close: boolean }
  /** `:e!` reloads the file from disk, discarding the buffer; `:e` does so only for a clean buffer. */
  | { kind: 'reload'; force: boolean }
  /** `:set autopair` / `:set noautopair`. */
  | { kind: 'autopair'; on: boolean }
  | { kind: 'say'; text: string }
  | { kind: 'none' };

/** The viewer's own `:` line (`:q` closes the viewer, not the app); the TUI's forms in file_viewer_commands.rs. */
export function viewerCommand(line: string): ViewerLineCmd {
  const t = line.trim().replace(/^:/, '').trim();
  if (t === '') return { kind: 'none' };
  if (/^\d+$/.test(t)) return { kind: 'goto', line: Number(t) };
  if (t === 'q' || t === 'quit') return { kind: 'quit', force: false };
  if (t === 'q!' || t === 'quit!') return { kind: 'quit', force: true };
  if (t === 'e!') return { kind: 'reload', force: true };
  if (t === 'e') return { kind: 'reload', force: false };
  if (t === 'w') return { kind: 'save', force: false, close: false };
  if (t === 'w!') return { kind: 'save', force: true, close: false };
  if (t === 'wq' || t === 'x') return { kind: 'save', force: false, close: true };
  if (t === 'set autopair' || t === 'set noautopair') return { kind: 'autopair', on: t === 'set autopair' };
  return { kind: 'say', text: `:${t} is not in the desktop viewer` };
}

export type TreeOp = 'add' | 'mkdir' | 'rename' | 'move' | 'copy' | 'delete' | 'undo';

export type TreeCmd =
  | { kind: 'move'; n: number }
  | { kind: 'half'; n: number }
  | { kind: 'activate' }
  | { kind: 'open-node' }
  | { kind: 'close-node' }
  | { kind: 'preview' }
  | { kind: 'follow' }
  | { kind: 'collapse-all' }
  | { kind: 'refresh' }
  | { kind: 'finder' }
  | { kind: 'hidden' }
  | { kind: 'yank'; what: 'absolute' | 'name' | 'relative' }
  | { kind: 'info' }
  | { kind: 'close' }
  | { kind: 'focus-viewer' }
  | { kind: 'resize'; by: number }
  | { kind: 'reset-width' }
  | { kind: 'help' }
  /** a / A / r / m / c / d open the add, mkdir, rename, move, copy or delete prompt; u undoes the last change. */
  | { kind: 'op'; op: TreeOp }
  | { kind: 'say'; text: string }
  | { kind: 'swallow' }
  | { kind: 'pass' };


/** Resolve one explorer-tree key (TUI "File Explorer Overlay"). `pending` is `y` mid-chord. */
export function treeKey(pending: string, k: KeyLike): { cmd: TreeCmd; pending: string } {
  const done = (cmd: TreeCmd) => ({ cmd, pending: '' });
  if (k.metaKey || k.altKey) return done({ kind: 'pass' });
  if (k.ctrlKey) {
    if (k.shiftKey && k.key === 'ArrowLeft') return done({ kind: 'resize', by: -1 });
    if (k.shiftKey && k.key === 'ArrowRight') return done({ kind: 'resize', by: 1 });
    switch (k.key.toLowerCase()) {
      case 'l': return done({ kind: 'focus-viewer' });
      case '0': return done({ kind: 'reset-width' });
      case 'd': return done({ kind: 'half', n: 1 });
      case 'u': return done({ kind: 'half', n: -1 });
      case 'n': return done({ kind: 'move', n: 1 });
      case 'p': return done({ kind: 'move', n: -1 });
      default: return done({ kind: 'pass' });
    }
  }
  if (pending === 'y') {
    if (k.key === 'y') return done({ kind: 'yank', what: 'absolute' });
    if (k.key === 'n') return done({ kind: 'yank', what: 'name' });
    if (k.key === 'r') return done({ kind: 'yank', what: 'relative' });
    // Any other key cancels the chord and then acts normally.
  }
  switch (k.key) {
    case 'Escape': case 'q': case ' ': return done({ kind: 'close' });
    case 'j': case 'ArrowDown': return done({ kind: 'move', n: 1 });
    case 'k': case 'ArrowUp': return done({ kind: 'move', n: -1 });
    case 'g': case 'Home': return done({ kind: 'move', n: -Infinity });
    case 'G': case 'End': return done({ kind: 'move', n: Infinity });
    case 'PageDown': return done({ kind: 'half', n: 2 });
    case 'PageUp': return done({ kind: 'half', n: -2 });
    case 'Enter': case 'o': return done({ kind: 'activate' });
    case 'l': case 'ArrowRight': return done({ kind: 'open-node' });
    case 'h': case 'ArrowLeft': return done({ kind: 'close-node' });
    case 'Tab': return done({ kind: 'preview' });
    case 'P': return done({ kind: 'follow' });
    case 'z': return done({ kind: 'collapse-all' });
    case 'R': return done({ kind: 'refresh' });
    case '/': return done({ kind: 'finder' });
    case '.': case 'H': return done({ kind: 'hidden' });
    case 'y': return { cmd: { kind: 'swallow' }, pending: 'y' };
    case 'i': return done({ kind: 'info' });
    case '?': return done({ kind: 'help' });
    case '<': return done({ kind: 'resize', by: -1 });
    case '>': return done({ kind: 'resize', by: 1 });
    case '=': return done({ kind: 'reset-width' });
    case 'a': return done({ kind: 'op', op: 'add' });
    case 'A': return done({ kind: 'op', op: 'mkdir' });
    case 'r': return done({ kind: 'op', op: 'rename' });
    case 'm': return done({ kind: 'op', op: 'move' });
    case 'c': return done({ kind: 'op', op: 'copy' });
    case 'd': return done({ kind: 'op', op: 'delete' });
    case 'u': return done({ kind: 'op', op: 'undo' });
    default: return done(k.key.length === 1 ? { kind: 'swallow' } : { kind: 'pass' });
  }
}

export type PickerCmd =
  | { kind: 'move'; n: number } | { kind: 'clear' } | { kind: 'clear-segment' } | { kind: 'open' } | { kind: 'close' } | { kind: 'type' };

/**
 * A key in the picker's query box (TUI "Telescope File Picker" and the
 * explorer's finder, which adds `Ctrl-W`); `type` leaves it to the text field.
 */
export function pickerKey(k: KeyLike): PickerCmd {
  if (k.key === 'Escape') return { kind: 'close' };
  if (k.key === 'Enter') return { kind: 'open' };
  if (k.key === 'ArrowDown') return { kind: 'move', n: 1 };
  if (k.key === 'ArrowUp') return { kind: 'move', n: -1 };
  if (k.ctrlKey && !k.metaKey && !k.altKey) {
    switch (k.key.toLowerCase()) {
      case 'j': case 'n': return { kind: 'move', n: 1 };
      case 'k': case 'p': return { kind: 'move', n: -1 };
      case 'u': return { kind: 'clear' };
      case 'w': return { kind: 'clear-segment' };
      default: break;
    }
  }
  return { kind: 'type' };
}

/** `Ctrl-W` in the finder: drop the last path segment of the query (and its `/`). */
export function dropSegment(query: string): string {
  const t = query.replace(/\/+$/, '');
  const at = t.lastIndexOf('/');
  return at < 0 ? '' : t.slice(0, at + 1);
}

// ---------- explorer width ----------

export const DRAWER_DEFAULT = 300;
export const DRAWER_MIN = 180;
export const DRAWER_STEP = 24;

/** The drawer width after a resize step, kept between the minimum and 60% of the space. */
export function drawerWidth(current: number, by: number, space: number): number {
  const max = Math.max(DRAWER_MIN, Math.floor(space * 0.6));
  return clamp(current + by * DRAWER_STEP, DRAWER_MIN, max);
}

export function entryInfo(e: DirEntry): string {
  const when = e.mtime ? `, modified ${e.mtime.replace('T', ' ').replace(/\.\d+/, '').replace(/Z$/, ' UTC')}` : '';
  const kind = e.kind === 'dir' ? 'folder' : e.kind === 'symlink' ? 'symlink' : formatSize(e.size);
  const flags = [e.hidden ? 'hidden' : '', e.ignored ? 'ignored by git' : ''].filter(Boolean).join(', ');
  return `${e.path}: ${kind}${when}${flags ? ` (${flags})` : ''}`;
}

/** Join the canonical root and a relative path for the absolute-path yank. */
export function absolutePath(root: string, rel: string): string {
  if (rel === '') return root;
  const sep = root.includes('\\') && !root.includes('/') ? '\\' : '/';
  return `${root.replace(/[\\/]+$/, '')}${sep}${sep === '\\' ? rel.replace(/\//g, '\\') : rel}`;
}
