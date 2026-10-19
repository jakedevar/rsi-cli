// The file explorer's add / mkdir / rename / move / copy / delete / undo
// prompts (#113, hub #3012), pure (no DOM). The DOM side is files-ops.ts; every
// change goes through rsid (CreateProjectEntry, MoveProjectEntry,
// CopyProjectEntry, TrashProjectEntry, UndoProjectFileOperation) and rsid owns
// the confinement, the never-overwrite rule and the undo log. This module only
// turns what the user typed into a root-relative request, with the TUI's
// prompt wording, destination rules and success messages
// (crates/rsi/src/overlay/file_explorer/mod.rs, fs_ops.rs).
import type { OperationResult, TreeOp, UndoResult } from './files-model.js';

export type PromptOp = Exclude<TreeOp, 'undo'>;

/** The entry an operation acts on (the explorer selection). */
export interface OpTarget { path: string; name: string; isDir: boolean }

export interface OpPrompt {
  op: PromptOp;
  target: OpTarget | null;
  /** Add / mkdir: the folder the new entry goes in (root-relative, "" for the root). */
  dir: string;
  input: string;
  /** Caret position (UTF-16 units). */
  cursor: number;
}

const nameOf = (p: string): string => p.slice(p.lastIndexOf('/') + 1);
const parentOf = (p: string): string => (p.includes('/') ? p.slice(0, p.lastIndexOf('/')) : '');

/** Caret before the extension dot of a file name (the rename prompt, `stem_cursor`). */
export function stemCursor(name: string): number | null {
  const dot = name.lastIndexOf('.');
  return dot > 0 ? [...name.slice(0, dot)].join('').length : null;
}

/**
 * Open a prompt for `op`. `target` is the selected entry (null when nothing is
 * selected); `dir` is the folder an add goes in (the selected folder itself, or
 * the file's parent, else the root). A string result is the TUI's refusal.
 */
export function startOp(op: PromptOp, target: OpTarget | null, dir: string): OpPrompt | string {
  if (op === 'add' || op === 'mkdir') return { op, target, dir, input: '', cursor: 0 };
  if (!target) return 'Nothing selected';
  if (op === 'rename') {
    const cursor = !target.isDir ? stemCursor(target.name) : null;
    return { op, target, dir, input: target.name, cursor: cursor ?? target.name.length };
  }
  if (op === 'delete') return { op, target, dir, input: '', cursor: 0 };
  return { op, target, dir, input: target.path, cursor: target.path.length };
}

const dirLabel = (dir: string): string => (dir === '' ? './' : `${dir}/`);

/** The prompt's first row (ui/overlay/file_explorer.rs `prompt_label`). */
export function promptLabel(p: OpPrompt): string {
  const name = p.target?.name ?? '';
  switch (p.op) {
    case 'add': return `New file in ${dirLabel(p.dir)} (end with / for dir)`;
    case 'mkdir': return `New directory in ${dirLabel(p.dir)}`;
    case 'rename': return `Rename ${name}`;
    case 'move': return `Move ${name} to`;
    case 'copy': return `Copy ${name} to`;
    case 'delete': return `Delete ${name}${p.target?.isDir ? '/ and its contents' : ''}?`;
  }
}

/** Delete asks for a yes or no (Enter, y, Y, d confirm; Esc, n, N, q cancel), not text. */
export const isConfirmation = (p: OpPrompt): boolean => p.op === 'delete';

export function confirmKey(key: string, ctrl: boolean): 'yes' | 'no' | null {
  if (!ctrl && (key === 'Enter' || key === 'y' || key === 'Y' || key === 'd')) return 'yes';
  if (key === 'Escape' || key === 'n' || key === 'N' || key === 'q') return 'no';
  return null;
}

export type Resolved = { ok: true; path: string } | { ok: false; error: string };

/**
 * Resolve prompt `input` (relative to `base`, or absolute inside the root) to a
 * root-relative path strictly inside the root (fs_ops.rs `resolve_destination`,
 * lexically: `..` that leaves the root, the root itself and an absolute path
 * elsewhere are refused). A symlinked folder that points out is rsid's refusal,
 * not this module's.
 */
export function resolveDestination(rootName: string, rootAbs: string | null, base: string, input: string): Resolved {
  const trimmed = input.trim().replace(/\/+$/, '');
  if (trimmed === '') return { ok: false, error: 'Name cannot be empty' };
  if (trimmed.includes('\u0000')) return { ok: false, error: 'Name cannot contain a NUL byte' };
  const outside: Resolved = { ok: false, error: `Path must stay inside ${rootName}` };
  let rel: string;
  if (trimmed.startsWith('/') || /^[A-Za-z]:[\\/]/.test(trimmed)) {
    const abs = rootAbs?.replace(/[\\/]+$/, '') ?? null;
    const norm = trimmed.replace(/\\/g, '/');
    if (abs === null || !(norm === abs.replace(/\\/g, '/') || norm.startsWith(`${abs.replace(/\\/g, '/')}/`))) return outside;
    rel = norm.slice(abs.length).replace(/^\//, '');
  } else {
    rel = base === '' ? trimmed : `${base}/${trimmed}`;
  }
  const stack: string[] = [];
  for (const seg of rel.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') { if (stack.length === 0) return outside; stack.pop(); continue; }
    stack.push(seg);
  }
  if (stack.length === 0) return outside;
  return { ok: true, path: stack.join('/') };
}

/** What submitting a prompt asks the DOM side to do. */
export type Plan =
  | { kind: 'error'; error: string }
  /** The name is unchanged: nothing to do. */
  | { kind: 'noop' }
  | { kind: 'create'; path: string; as: 'file' | 'dir' }
  | { kind: 'rename'; from: string; to: string }
  /** Move or copy: `to` may name an existing folder, which then receives the entry (`mv a dir/`); the DOM side asks rsid. */
  | { kind: 'move' | 'copy'; from: string; to: string };

/** Turn a submitted text prompt into a plan (`run_prompt`). */
export function planOp(p: OpPrompt, rootName: string, rootAbs: string | null): Plan {
  const resolved = (base: string): Resolved => resolveDestination(rootName, rootAbs, base, p.input);
  switch (p.op) {
    case 'add':
    case 'mkdir': {
      const r = resolved(p.dir);
      if (!r.ok) return { kind: 'error', error: r.error };
      return { kind: 'create', path: r.path, as: p.op === 'mkdir' || p.input.trim().endsWith('/') ? 'dir' : 'file' };
    }
    case 'rename': {
      if (!p.target) return { kind: 'error', error: 'Nothing selected' };
      const r = resolved(parentOf(p.target.path));
      if (!r.ok) return { kind: 'error', error: r.error };
      return r.path === p.target.path ? { kind: 'noop' } : { kind: 'rename', from: p.target.path, to: r.path };
    }
    case 'move':
    case 'copy': {
      if (!p.target) return { kind: 'error', error: 'Nothing selected' };
      const r = resolved('');
      if (!r.ok) return { kind: 'error', error: r.error };
      return { kind: p.op, from: p.target.path, to: r.path };
    }
    case 'delete':
      return { kind: 'noop' };
  }
}

/** `mv a dir/`: a destination that is an existing folder receives the source under its own name. */
export function intoDirectory(from: string, to: string, destIsDir: boolean): string {
  return destIsDir && to !== from ? `${to}/${nameOf(from)}` : to;
}

// ---------- the TUI's messages ----------

export function createdText(path: string, as: 'file' | 'dir'): string {
  return `Created ${path}${as === 'dir' ? '/' : ''}`;
}
export function transferredText(op: 'moved' | 'copied', from: string, to: string): string {
  if (op === 'copied') return `Copied to ${to}`;
  return parentOf(from) === parentOf(to) ? `Renamed to ${nameOf(to)}` : `Moved to ${to}`;
}
export const trashedText = (path: string): string => `Deleted: ${nameOf(path)} (u to undo)`;

/** The undo's message and the entry the tree selects afterwards (null: none). */
export function undoText(r: Pick<UndoResult, 'effect' | 'path'>): { message: string; select: string | null } {
  const name = nameOf(r.path);
  switch (r.effect) {
    case 'restored': return { message: `Restored ${name}`, select: r.path };
    case 'moved_back': return { message: `Moved ${name} back`, select: r.path };
    case 'removed': return { message: `Removed ${name}`, select: parentOf(r.path) };
    case 'trashed': return { message: `Moved ${name} to trash`, select: parentOf(r.path) };
  }
}

/** The folders whose listings an operation or undo changed, and what the tree selects. */
export function touched(r: OperationResult | UndoResult): { dirs: string[]; moved: { from: string; to: string } | null } {
  if ('op' in r) {
    switch (r.op) {
      case 'created': return { dirs: [parentOf(r.created_top ?? r.path), parentOf(r.path)], moved: null };
      case 'copied': return { dirs: [parentOf(r.to ?? r.path)], moved: null };
      case 'moved': return { dirs: [parentOf(r.path), parentOf(r.to ?? r.path)], moved: { from: r.path, to: r.to ?? r.path } };
      case 'trashed': return { dirs: [parentOf(r.path)], moved: null };
    }
  }
  const dirs = [parentOf(r.path)];
  if (r.effect === 'moved_back') {
    const from = r.from ?? r.path;
    return { dirs: [...dirs, parentOf(from)], moved: { from, to: r.path } };
  }
  if (r.effect === 'restored') return { dirs, moved: null };
  return { dirs, moved: null };
}

// ---------- Tab completion ----------

export interface CompletionEntry { name: string; isDir: boolean }

/** Split a prompt input at its last `/`: the folder part (with the slash) and the name typed so far. */
export function splitInput(input: string): { dirPart: string; partial: string } {
  const i = input.lastIndexOf('/');
  return i < 0 ? { dirPart: '', partial: input } : { dirPart: input.slice(0, i + 1), partial: input.slice(i + 1) };
}

/**
 * Complete the last segment of `input` against the folder it names
 * (`complete_path`): a unique match completes fully (a folder gains a `/`),
 * several complete to their longest common prefix; null when nothing is gained.
 */
export function completeInput(input: string, entries: readonly CompletionEntry[], showHidden: boolean): string | null {
  const { dirPart, partial } = splitInput(input);
  const hits = entries
    .filter((e) => (showHidden || partial.startsWith('.') || !e.name.startsWith('.')) && e.name.startsWith(partial))
    .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  if (hits.length === 0) return null;
  if (hits.length === 1) return `${dirPart}${hits[0]!.name}${hits[0]!.isDir ? '/' : ''}`;
  let prefix = [...hits[0]!.name];
  for (const h of hits.slice(1)) {
    const cs = [...h.name];
    let n = 0;
    while (n < prefix.length && n < cs.length && prefix[n] === cs[n]) n++;
    prefix = prefix.slice(0, n);
  }
  const common = prefix.join('');
  return [...common].length > [...partial].length ? `${dirPart}${common}` : null;
}

// ---------- entry points ----------

export interface OpMenuItem { op: TreeOp; label: string; key: string }

/** The explorer's right-click menu: what applies to the entry under the pointer (none: the whole tree). */
export function opMenu(target: OpTarget | null): OpMenuItem[] {
  const items: OpMenuItem[] = [
    { op: 'add', label: 'New file…', key: 'a' },
    { op: 'mkdir', label: 'New folder…', key: 'A' },
  ];
  if (target) {
    items.push(
      { op: 'rename', label: 'Rename…', key: 'r' },
      { op: 'move', label: 'Move…', key: 'm' },
      { op: 'copy', label: 'Copy…', key: 'c' },
      { op: 'delete', label: 'Delete (to trash)…', key: 'd' },
    );
  }
  items.push({ op: 'undo', label: 'Undo last change', key: 'u' });
  return items;
}
