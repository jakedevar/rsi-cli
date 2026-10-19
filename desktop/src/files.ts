// Project files (slice 10B and #113, hub #3012), DOM side: the file explorer
// drawer, the fuzzy file picker and the file viewer/editor with its git gutter.
// The rules are files-model.ts, files-edit-model.ts and files-ops-model.ts
// (pure, tested), the registry entries actions/files.ts, and RUNTIME is
// actions/files-run.ts. Every file byte comes from, and every change goes to,
// rsid's project-root-scoped methods through files-api.ts (a save is
// WriteProjectFile fenced by the version ReadProjectFile answered; the
// explorer's add / rename / move / copy / trash / undo are files-ops.ts); file
// text and names are set with textContent or a text box's value only.
//
// The panels cover the session detail pane (the TUI draws its explorer and
// viewer there too) and leave the session list usable. While the tree or the
// viewer has focus every plain key goes to it, as in the TUI; Ctrl/Cmd chords
// they do not use (the palette, the terminal) pass through.
import type { FilesActionId, FilesSnapshot } from './actions/files.js';
import { type Invocation, copyText, dispatch } from './commands.js';
import {
  type Draft, DraftStore, type Eol, editRefusal, eolOf, failedText, lineCol, normalEntryOffset, refusedChangedText,
  refusedUnverifiedText, saveRequest, savedText, staleVerdict, toBuffer, toDisk, tooLargeToSave, upToDateText, UNSAVED, UNSAVED_RELOAD,
} from './files-edit-model.js';
import { autoPairEdit, pastedText, selectionRows, shiftSigns, skipHidden, type TextEdit } from './files-editor-model.js';
import { type Ops, type OpsHost, createOps } from './files-ops.js';
import { el, toast } from './dom.js';
import { filesApi } from './files-api.js';
import {
  DIRECTORY_ENTRIES, DRAWER_DEFAULT, type DirEntry, FileTree, type FileRead, type FileRoot, GUTTER_CHARS, type GutterSign,
  type EditEntry, MAX_READ_BYTES, NO_VIEWER_KEYS, PICKER_PATHS, READ_BYTES, type Ranked, type RootChoice, type TreeEffect, type TreeOp, type ViewMove,
  type ViewPos, type ViewerKeys, absolutePath, canReadMore, chooseRoot, drawerWidth, dropSegment, entryInfo, fileErrorText, gutterNote,
  gutterSigns, moveView, movePickerSelection, pickerKey, rankPaths, readSummary, refusalCode, settle, splitLines, treeKey, viewerCommand,
  viewerKey, writeErrorText,
} from './files-model.js';
import { isTypingTarget } from './keyengine.js';
import { api } from './api.js';
import { pickerLeader } from './overlay-keys.js';
import { runLeader } from './pickers.js';
import { highlightCodeBlocks } from './codeblocks.js';
import {
  type Deco, Folds, type Lang, type LexState, MAX_HIGHLIGHT_LINE, type Match, advanceMatch, bracketPartner, colMotion,
  displayIndex, findMatches, foldRegions, highlightLine, isMarkdown, languageOf, lastCol, lexStates, matchFrom, segments,
  wordUnderCursor,
} from './viewer-model.js';
import { type Lines, offsetToPos, posToOffset, toLines } from './vim-text.js';
import { relativeTo } from './input-model.js';
import { selected, state, uiChanged } from './state.js';
import { focusOwnsKey, paneKeyAllowed } from './modal-gate.js';
import { currentPlatform, isVimKeysOn } from './vimkeys.js';
import { editorOf, enterInsert, enterNormal, modeOf, prepareForTyping, vimKey } from './vim-field.js';

interface Ctx {
  root: FileRoot;
  key: string;
  label: string;
  /** Canonical absolute root, from the first answer. */
  abs: string | null;
}

interface Viewer {
  ctx: Ctx;
  path: string;
  read: FileRead | null;
  lines: string[];
  signs: Array<GutterSign | null>;
  note: string | null;
  cap: number;
  /** `cursor` is a line index, `top` a display row (folded lines removed). */
  pos: ViewPos;
  error: string | null;
  loading: boolean;
  /** The lines as code points, for the column motions. */
  chars: Lines;
  /** Cursor column (code points) and the column vertical moves aim for. */
  col: number;
  want: number;
  lang: Lang | null;
  states: LexState[];
  folds: Folds;
  /** Space m: the rendered Markdown in place of the source. */
  preview: boolean;
  /** The editor, while the file is being edited (#113). */
  edit: Editor | null;
}

/** An open edit: what the text box started from, and the fence the save sends. */
interface Editor {
  eol: Eol;
  /** The disk text (LF breaks) the buffer started from; the buffer differs from it when there are unsaved changes. */
  base: string;
  /** The version ReadProjectFile answered for that text; WriteProjectFile refuses a save when the file moved on. */
  version: string;
  /** The file changed on disk under the buffer (a refused save, or a draft opened on a newer file). */
  conflict: boolean;
  /** `:set autopair` / `:set noautopair` (on by default, as in the TUI). */
  autoPair: boolean;
  /** The lines of `base` and the git gutter signs that belong to them; the signs are carried over unsaved edits. */
  baseLines: string[];
  baseSigns: Array<GutterSign | null>;
}

interface Search {
  pattern: string;
  forward: boolean;
  matches: Match[];
  byRow: Map<number, Match[]>;
  current: number;
}

interface Picker {
  ctx: Ctx;
  paths: string[];
  truncated: boolean;
  loading: boolean;
  error: string | null;
  results: Ranked[];
  selected: number;
  /** Opened from the explorer (`/`): Enter also reveals the file in the tree. */
  reveal: boolean;
}

type Focus = 'tree' | 'viewer' | 'none';

const LINE_H = 18;
const ROW_H = 22;
const OVERSCAN = 40;
const WIDTH_KEY = 'rsi.desktop.files.drawerWidth';

let tree = new FileTree();
let treeCtx: Ctx | null = null;
let explorerOpen = false;
let followPreview = false;
let treePending = '';
let viewer: Viewer | null = null;
let vkeys: ViewerKeys = NO_VIEWER_KEYS;
let picker: Picker | null = null;
let focus: Focus = 'none';
let width = DRAWER_DEFAULT;
let epoch = 0;
let pickerEpoch = 0;
let pickerReturn: Focus = 'none';
let search: Search | null = null;
/** The `:` / `/` / `?` prompt kind and where the cursor was when a search prompt opened. */
let prompt: { kind: ':' | '/' | '?'; origin: { row: number; col: number } } | null = null;
let previewMarks: HTMLElement[] = [];
let previewCurrent = -1;
/** Unsaved buffers of files that are not the open one (the TUI's file_viewer_cache). */
const drafts = new DraftStore();
/** The text box of the file being edited; a fresh one per edit so the vim state never leaks between files. */
let area: HTMLTextAreaElement | null = null;
let saving = false;
let shownDirty = false;
let ops: Ops | null = null;
/** The caret row at the last paint and a half-typed `z` (fold) key in the editor. */
let editRow = 0;
let zPending = false;
let paintQueued = false;
/** The text the viewer's lines were last split from. */
let bufferText = '';
let heavyTimer: ReturnType<typeof setTimeout> | null = null;
/** Past this many lines the lexer, folds, search and gutter wait for a pause in typing. */
const HEAVY_LINES = 2000;

let ui: {
  overlay: HTMLElement;
  drawer: HTMLElement;
  title: HTMLElement;
  flags: HTMLElement;
  tree: HTMLElement;
  view: HTMLElement;
  path: HTMLElement;
  meta: HTMLElement;
  more: HTMLButtonElement;
  editBtn: HTMLButtonElement;
  saveBtn: HTMLButtonElement;
  editHost: HTMLElement;
  calm: HTMLElement;
  body: HTMLElement;
  spacer: HTMLElement;
  window: HTMLElement;
  cmdline: HTMLFormElement;
  colon: HTMLElement;
  cmdinput: HTMLInputElement;
  preview: HTMLElement;
  picker: HTMLElement;
  pinput: HTMLInputElement;
  pstatus: HTMLElement;
  plist: HTMLElement;
} | null = null;

// ---------- roots ----------

function projectFilter(): string | null {
  const sel = document.getElementById('project-filter') as HTMLSelectElement | null;
  return sel?.value || null;
}

function currentChoice(): RootChoice {
  return chooseRoot(selected(), state.projects, projectFilter());
}

/**
 * Open an absolute path in the file viewer (gf1..gf9, gap sweep G1): the path
 * must lie inside the open session's root. Null when it opened, else why not.
 */
export function openAbsoluteFile(path: string, relativeTo: (root: string, path: string) => string | null): string | null {
  const c = currentChoice();
  if (!c.ok) return c.why;
  const s = selected();
  const base = 'project_id' in c.root
    ? state.projects.find((p) => p.id === (c.root as { project_id: string }).project_id)?.path ?? ''
    : s?.working_dir ?? '';
  const rel = relativeTo(base, path);
  if (rel === null) return `${path} is outside ${base || 'the session root'}`;
  const ctx: Ctx = viewer?.ctx.key === c.key ? viewer.ctx : treeCtx?.key === c.key ? treeCtx : { root: c.root, key: c.key, label: c.label, abs: null };
  openFile(ctx, rel, true);
  return null;
}

/** The root the next explorer or picker opens on: the tree's, the viewer's, else the current one. */
function activeCtx(): Ctx | { why: string } {
  if (explorerOpen && treeCtx) return treeCtx;
  if (viewer) return viewer.ctx;
  const c = currentChoice();
  if (!c.ok) return { why: c.why };
  if (treeCtx?.key === c.key) return treeCtx;
  return { root: c.root, key: c.key, label: c.label, abs: null };
}

// ---------- registry surface ----------

function yankTarget(): { rel: string; root: string | null; entry: DirEntry | null } | null {
  if (explorerOpen && (focus !== 'viewer' || !viewer)) {
    const e = tree.selectedEntry();
    if (e) return { rel: e.path, root: treeCtx?.abs ?? null, entry: e };
  }
  if (viewer) return { rel: viewer.path, root: viewer.read?.root ?? viewer.ctx.abs, entry: null };
  return null;
}

export function filesSnapshot(): FilesSnapshot {
  const c = explorerOpen || viewer ? null : currentChoice();
  const hasRoot = c === null || c.ok;
  return {
    hasRoot,
    ...(c && !c.ok ? { noRoot: c.why } : {}),
    explorer: explorerOpen,
    viewer: viewer !== null,
    picker: picker !== null,
    showHidden: tree.showHidden,
    followPreview,
    target: yankTarget() !== null,
    editing: viewer?.edit != null,
    dirty: editDirty(),
    selection: explorerOpen && tree.selectedEntry() !== null,
  };
}

export function runFiles(id: FilesActionId, _inv: Invocation): void {
  switch (id) {
    case 'files-explorer': if (explorerOpen) closeExplorer(); else openExplorer(); return;
    case 'files-find': openPicker(explorerOpen); return;
    case 'files-close-viewer': closeViewer(); return;
    case 'files-edit': startEdit('i', null); return;
    case 'files-save': void saveFile(false, false); return;
    case 'files-save-force': void saveFile(true, false); return;
    case 'files-reload': reloadFile(true); return;
    case 'files-add': ops?.run('add'); return;
    case 'files-mkdir': ops?.run('mkdir'); return;
    case 'files-rename': ops?.run('rename'); return;
    case 'files-move': ops?.run('move'); return;
    case 'files-copy': ops?.run('copy'); return;
    case 'files-delete': ops?.run('delete'); return;
    case 'files-undo': ops?.run('undo'); return;
    case 'files-toggle-hidden': tree.toggleHidden(); renderTree(); return;
    case 'files-follow-preview':
      followPreview = !followPreview;
      toast(followPreview ? 'Follow preview on: the viewer shows each file you move to' : 'Follow preview off', true);
      return;
    case 'files-refresh': refreshTree(); return;
    case 'files-collapse-all': tree.collapseAll(); renderTree(); return;
    case 'files-copy-path': yank('absolute'); return;
    case 'files-copy-name': yank('name'); return;
    case 'files-copy-relative': yank('relative'); return;
    case 'files-info': {
      const e = tree.selectedEntry();
      if (e) toast(entryInfo(e), true);
      return;
    }
  }
}

function yank(what: 'absolute' | 'name' | 'relative'): void {
  const t = yankTarget();
  if (!t) return;
  if (what === 'relative') { void copyText(t.rel, 'project-relative path'); return; }
  if (what === 'name') { void copyText(t.rel.split('/').pop() ?? t.rel, 'file name'); return; }
  if (!t.root) { toast('The absolute root is not known yet; try again once the folder has loaded'); return; }
  void copyText(absolutePath(t.root, t.rel), 'absolute path');
}

// ---------- layout ----------

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

function build(): NonNullable<typeof ui> {
  if (ui) return ui;
  const overlay = el('section', 'files-overlay');
  overlay.hidden = true;
  overlay.setAttribute('aria-label', 'Project files');

  const drawer = el('aside', 'files-drawer');
  drawer.hidden = true;
  const dhead = el('header', 'files-head');
  const title = el('span', 'files-title', 'Files');
  const flags = el('span', 'files-flags');
  const dclose = el('button', 'btn', 'Close');
  dclose.type = 'button';
  dclose.dataset.action = 'files-explorer';
  dhead.append(title, flags, dclose);
  const treeEl = el('ul', 'files-tree');
  treeEl.tabIndex = 0;
  treeEl.setAttribute('role', 'tree');
  treeEl.setAttribute('aria-label', 'Project files');
  const fileOps = createOps(opsHost());
  ops = fileOps;
  drawer.append(dhead, fileOps.toolbar, treeEl, fileOps.bar);

  const view = el('section', 'files-viewer');
  view.hidden = true;
  const vhead = el('header', 'files-head');
  const path = el('span', 'files-path');
  const meta = el('span', 'files-meta');
  const more = el('button', 'btn', `Read up to ${MAX_READ_BYTES >> 20} MiB`);
  more.type = 'button';
  more.hidden = true;
  const editBtn = el('button', 'btn', 'Edit');
  editBtn.type = 'button';
  editBtn.title = 'Edit this file (i). Save with Ctrl-S or :w';
  editBtn.dataset.action = 'files-edit';
  const saveBtn = el('button', 'btn', 'Save');
  saveBtn.type = 'button';
  saveBtn.hidden = true;
  saveBtn.title = 'Save (Ctrl-S or :w). :w! overwrites a file that changed on disk';
  saveBtn.dataset.action = 'files-save';
  const vclose = el('button', 'btn', 'Close');
  vclose.type = 'button';
  vclose.dataset.action = 'files-close-viewer';
  vhead.append(path, meta, more, editBtn, saveBtn, vclose);
  const calm = el('div', 'files-calm');
  calm.hidden = true;
  const body = el('div', 'files-body');
  body.tabIndex = 0;
  body.setAttribute('aria-label', 'File contents');
  const spacer = el('div', 'files-spacer');
  const win = el('div', 'files-window');
  spacer.append(win);
  body.append(spacer);
  const editHost = el('div', 'files-edit-host');
  editHost.hidden = true;
  const cmdline = el('form', 'files-cmdline');
  cmdline.hidden = true;
  const cmdinput = el('input', 'files-cmdinput');
  cmdinput.type = 'text';
  cmdinput.setAttribute('aria-label', 'Viewer command');
  const colon = el('span', 'files-colon', ':');
  cmdline.append(colon, cmdinput);
  const preview = el('div', 'files-preview md');
  preview.hidden = true;
  preview.tabIndex = 0;
  preview.setAttribute('aria-label', 'Markdown preview');
  view.append(vhead, calm, body, editHost, preview, cmdline);
  overlay.append(drawer, view);

  const pick = el('div', 'files-picker');
  pick.hidden = true;
  pick.setAttribute('role', 'dialog');
  pick.setAttribute('aria-label', 'Find file');
  const pinput = el('input', 'files-picker-input');
  pinput.type = 'text';
  pinput.placeholder = 'Find file…';
  pinput.spellcheck = false;
  pinput.autocomplete = 'off';
  const pstatus = el('div', 'files-picker-status');
  const plist = el('ul', 'files-picker-list');
  plist.setAttribute('role', 'listbox');
  pick.append(pinput, pstatus, plist);

  document.body.append(overlay, pick);
  ui = {
    overlay, drawer, title, flags, tree: treeEl, view, path, meta, more, editBtn, saveBtn, editHost, calm, body, spacer, window: win,
    cmdline, colon, cmdinput, preview, picker: pick, pinput, pstatus, plist,
  };
  wire(ui);
  return ui;
}

/** Cover the session detail pane (or the window when the layout has none). */
function place(): void {
  if (!ui) return;
  const host = document.querySelector<HTMLElement>('main.detail');
  const r = host?.getBoundingClientRect();
  const o = ui.overlay.style;
  if (r && r.width > 0 && r.height > 0) {
    o.left = `${r.left}px`; o.top = `${r.top}px`; o.width = `${r.width}px`; o.height = `${r.height}px`;
  } else {
    o.left = '0px'; o.top = '0px'; o.width = `${window.innerWidth}px`; o.height = `${window.innerHeight}px`;
  }
  const space = r && r.width > 0 ? r.width : window.innerWidth;
  width = drawerWidth(width, 0, space);
  ui.drawer.style.width = viewer ? `${width}px` : '100%';
  ui.drawer.style.maxWidth = viewer ? '' : `${width}px`;
}

function show(): void {
  const u = build();
  u.drawer.hidden = !explorerOpen;
  u.view.hidden = viewer === null;
  u.overlay.hidden = !explorerOpen && viewer === null;
  u.overlay.classList.toggle('drawer-only', explorerOpen && viewer === null);
  place();
  if (focus === 'tree' && !explorerOpen) focus = viewer ? 'viewer' : 'none';
  if (focus === 'viewer' && !viewer) focus = explorerOpen ? 'tree' : 'none';
  u.drawer.classList.toggle('focused', focus === 'tree');
  u.view.classList.toggle('focused', focus === 'viewer');
  uiChanged();
}

function setFocus(f: Focus, dom = true): void {
  focus = f;
  vkeys = NO_VIEWER_KEYS;
  treePending = '';
  show();
  if (!dom || !ui) return;
  if (f === 'tree') ui.tree.focus({ preventScroll: true });
  else if (f === 'viewer') (viewer?.edit && area ? area : viewer?.preview ? ui.preview : ui.body).focus({ preventScroll: true });
  else if (ui.overlay.contains(document.activeElement)) (document.activeElement as HTMLElement | null)?.blur();
}

// ---------- explorer ----------

function openExplorer(): void {
  const c = activeCtx();
  if (!('key' in c)) { toast(c.why); return; }
  if (treeCtx?.key !== c.key) {
    const hidden = tree.showHidden;
    tree = new FileTree();
    tree.showHidden = hidden;
    treeCtx = c;
  }
  explorerOpen = true;
  build().title.textContent = c.label;
  if (!tree.dirs.has('') || tree.dirs.get('')?.status === 'error') void loadDir('');
  if (viewer && viewer.ctx.key === c.key) for (const dir of tree.reveal(viewer.path)) void loadDir(dir);
  renderTree();
  setFocus('tree');
}

function closeExplorer(): void {
  explorerOpen = false;
  ops?.cancel();
  ops?.closeMenu();
  setFocus(viewer && focus !== 'none' ? 'viewer' : 'none');
}

async function loadDir(path: string): Promise<void> {
  const c = treeCtx;
  if (!c) return;
  tree.setLoading(path);
  renderTree();
  try {
    const l = await filesApi.listDirectory(c.root, path, DIRECTORY_ENTRIES);
    if (treeCtx !== c) return;
    c.abs = l.root;
    tree.setListing(path, l);
  } catch (e) {
    if (treeCtx !== c) return;
    tree.setError(path, fileErrorText(e));
  }
  renderTree();
  uiChanged();
}

function refreshTree(): void {
  for (const dir of tree.loadedDirs()) void loadDir(dir);
  toast('Explorer refreshed', true);
}

function renderTree(): void {
  if (!ui || !explorerOpen) return;
  const u = ui;
  u.title.textContent = treeCtx?.label ?? 'Files';
  u.title.title = treeCtx?.abs ?? '';
  u.flags.textContent = [tree.showHidden ? 'hidden shown' : '', followPreview ? 'follow' : ''].filter(Boolean).join(' · ');
  const openPath = viewer && treeCtx && viewer.ctx.key === treeCtx.key ? viewer.path : null;
  const unsaved = treeCtx ? unsavedPaths(treeCtx.key) : new Set<string>();
  const items: HTMLElement[] = [];
  for (const row of tree.rows()) {
    const li = el('li', 'files-row');
    li.style.paddingLeft = `${8 + row.depth * 14}px`;
    if (row.kind === 'note') {
      li.classList.add('note', row.tone);
      li.textContent = row.text;
      items.push(li);
      continue;
    }
    const e = row.entry;
    li.dataset.path = e.path;
    li.setAttribute('role', 'treeitem');
    if (e.kind === 'dir') li.setAttribute('aria-expanded', String(row.expanded));
    li.classList.add(e.kind);
    if (e.ignored) li.classList.add('ignored');
    if (e.hidden) li.classList.add('hidden-entry');
    if (e.path === tree.selected) { li.classList.add('selected'); li.setAttribute('aria-selected', 'true'); }
    if (e.path === openPath) li.classList.add('open');
    const icon = e.kind === 'dir' ? (row.expanded ? '▾' : '▸') : e.kind === 'symlink' ? '↪' : ' ';
    li.append(el('span', 'files-icon', icon), el('span', 'files-name', e.kind === 'dir' ? `${e.name}/` : e.name));
    if (unsaved.has(e.path)) {
      li.classList.add('draft');
      const dot = el('span', 'files-draft', '●');
      dot.title = 'Unsaved changes';
      li.append(dot);
    }
    items.push(li);
  }
  u.tree.replaceChildren(...items);
  u.tree.querySelector<HTMLElement>('.files-row.selected')?.scrollIntoView({ block: 'nearest' });
}

function treeRows(): number {
  return Math.max(1, Math.floor((ui?.tree.clientHeight ?? 0) / ROW_H));
}

function applyTree(effect: TreeEffect, focusViewer: boolean): void {
  if (effect.kind === 'load') void loadDir(effect.path);
  if (effect.kind === 'open' && treeCtx) openFile(treeCtx, effect.path, focusViewer);
  renderTree();
}

function afterTreeMove(): void {
  renderTree();
  const e = tree.selectedEntry();
  if (followPreview && treeCtx && e && e.kind !== 'dir') openFile(treeCtx, e.path, false);
}

function runTreeKey(ev: KeyboardEvent): boolean {
  const { cmd, pending } = treeKey(treePending, ev);
  treePending = pending;
  switch (cmd.kind) {
    case 'pass': return false;
    case 'swallow': return true;
    case 'move': tree.move(cmd.n); afterTreeMove(); return true;
    case 'half': tree.move(Math.max(1, Math.floor(treeRows() / 2)) * cmd.n); afterTreeMove(); return true;
    case 'activate': applyTree(tree.activate(), true); return true;
    case 'open-node': applyTree(tree.openNode(), true); return true;
    case 'close-node': tree.closeNode(); renderTree(); return true;
    case 'preview': applyTree(tree.activate(), false); return true;
    case 'follow': dispatch('files-follow-preview', { source: 'key' }); renderTree(); return true;
    case 'collapse-all': dispatch('files-collapse-all', { source: 'key' }); return true;
    case 'refresh': dispatch('files-refresh', { source: 'key' }); return true;
    case 'finder': openPicker(true); return true;
    case 'hidden': dispatch('files-toggle-hidden', { source: 'key' }); return true;
    case 'yank': dispatch(cmd.what === 'absolute' ? 'files-copy-path' : cmd.what === 'name' ? 'files-copy-name' : 'files-copy-relative', { source: 'key' }); return true;
    case 'info': dispatch('files-info', { source: 'key' }); return true;
    case 'close': closeExplorer(); return true;
    case 'focus-viewer': if (viewer) setFocus('viewer'); else toast('No file is open in the viewer'); return true;
    case 'resize': resize(cmd.by); return true;
    case 'reset-width': resize(0, true); return true;
    case 'help': dispatch('help', { source: 'key' }); return true;
    case 'op': ops?.run(cmd.op); return true;
    case 'say': toast(cmd.text); return true;
  }
}

function resize(by: number, reset = false): void {
  const host = document.querySelector<HTMLElement>('main.detail');
  const space = host?.getBoundingClientRect().width || window.innerWidth;
  width = reset ? DRAWER_DEFAULT : drawerWidth(width, by, space);
  storage()?.setItem(WIDTH_KEY, String(width));
  place();
}

// ---------- viewer ----------

function openFile(ctx: Ctx, path: string, focusViewer: boolean, cap = READ_BYTES, reload: { resume: boolean } | null = null): void {
  const same = viewer && viewer.ctx.key === ctx.key && viewer.path === path ? viewer : null;
  // Opening the file being edited again (the tree, the picker) is not a reload: keep the buffer.
  if (same?.edit && reload === null) { setFocus('viewer'); return; }
  // Another file: the unsaved buffer is kept as a draft, the way the TUI caches its viewers.
  if (viewer?.edit && !same) stashEdit();
  if (same?.edit) unmountArea();
  const pos = same ? same.pos : { cursor: 0, top: 0 };
  if (!same) search = null;
  viewer = {
    ctx, path, read: same?.read ?? null, lines: same?.lines ?? [], signs: same?.signs ?? [], note: null, cap, pos, error: null, loading: true,
    chars: same?.chars ?? [], col: same?.col ?? 0, want: same?.want ?? 0, lang: languageOf(path), states: same?.states ?? [],
    folds: same?.folds ?? new Folds(), preview: same?.preview ?? false, edit: null,
  };
  const mine = ++epoch;
  build();
  renderViewer();
  if (focusViewer) setFocus('viewer'); else show();
  renderTree();
  void (async () => {
    const [read, gutter] = await Promise.allSettled([filesApi.readFile(ctx.root, path, cap), filesApi.gitGutter(ctx.root, path)]);
    if (mine !== epoch || !viewer) return;
    viewer.loading = false;
    if (read.status === 'rejected') {
      viewer.error = fileErrorText(read.reason);
      viewer.read = null;
      viewer.lines = [];
      viewer.signs = [];
    } else {
      const r = read.value;
      ctx.abs ??= r.root;
      viewer.read = r;
      viewer.lines = r.binary ? [] : splitLines(r.content ?? '');
      viewer.chars = toLines(viewer.lines.join('\n'));
      viewer.states = viewer.lang ? lexStates(viewer.lang, viewer.lines) : [];
      viewer.folds.setRegions(foldRegions(viewer.lang, viewer.lines));
      if (!isMarkdown(path)) viewer.preview = false;
      if (search) setSearch(search.pattern, search.forward);
      if (gutter.status === 'fulfilled') {
        viewer.signs = gutterSigns(gutter.value.markers, viewer.lines.length);
        viewer.note = gutterNote(gutter.value.status);
      } else {
        viewer.signs = [];
        viewer.note = `no git gutter: ${fileErrorText(gutter.reason)}`;
      }
      settleViewer();
    }
    renderViewer();
    if (viewer.preview) void renderPreview();
    // A kept draft comes back where it was left; a reload with `:e!` keeps editing.
    const draft = viewer.read ? drafts.get(ctx.key, path) : undefined;
    if (draft && viewer.read) {
      drafts.drop(ctx.key, path);
      resumeDraft(draft, viewer.read.version);
    } else if (reload?.resume && viewer.read) {
      startEdit('normal', null);
    }
    renderTree();
    uiChanged();
  })();
}

/**
 * Open `path` (absolute inside the root, or relative to it) in the viewer: a
 * right-clicked path in the transcript (G2). It goes through openAbsoluteFile,
 * the one path gf1..gf9 use too.
 */
export function openPathInViewer(path: string): void {
  const absolute = /^(?:[\\/]|[A-Za-z]:[\\/])/.test(path);
  const why = openAbsoluteFile(path, absolute ? relativeTo : (_root, p) => p.replace(/\\/g, '/'));
  if (why) toast(why);
}

/** Close the viewer. An unsaved buffer refuses (`:q`) unless `force` (`:q!`, which discards it). */
function closeViewer(force = false): boolean {
  if (viewer?.edit) {
    if (!force && editDirty()) { toast(UNSAVED); return false; }
    drafts.drop(viewer.ctx.key, viewer.path);
    unmountArea();
  }
  viewer = null;
  search = null;
  prompt = null;
  epoch++;
  if (ui) { ui.cmdline.hidden = true; ui.window.replaceChildren(); ui.preview.replaceChildren(); ui.preview.hidden = true; }
  setFocus(explorerOpen && focus !== 'none' ? 'tree' : 'none');
  renderTree();
  uiChanged();
  return true;
}

function viewRows(): number {
  return Math.max(1, Math.floor((ui?.body.clientHeight ?? 0) / LINE_H));
}

function metaText(v: Viewer): string {
  const parts: string[] = [];
  if (v.loading) parts.push('Loading…');
  else if (v.read) parts.push(readSummary(v.read, v.lines.length, v.cap));
  if (v.note && v.read && !v.read.binary && !v.edit) parts.push(v.note);
  if (v.read && !v.read.binary) {
    if (v.edit) parts.push(editStatus());
    else if (v.preview) parts.push('preview (Space m: source)');
    else parts.push(`${v.pos.cursor + 1}:${v.col + 1}`);
    if (v.folds.foldedCount() > 0 && !v.preview) parts.push(`${v.folds.foldedCount()} fold${v.folds.foldedCount() === 1 ? '' : 's'} closed`);
    if (search) parts.push(searchSummary(search));
  }
  return parts.join(' · ');
}

function renderViewer(): void {
  if (!ui || !viewer) return;
  const u = ui;
  const v = viewer;
  u.path.textContent = v.path;
  u.path.title = v.read?.root ? absolutePath(v.read.root, v.path) : v.path;
  u.meta.textContent = metaText(v);
  u.more.hidden = !(v.read && canReadMore(v.read, v.cap)) || !!v.edit;
  const calm = v.error ?? (v.read?.binary ? `Binary file (${readSummary(v.read, 0, v.cap).replace(/^Binary file, /, '')})` : null);
  u.calm.hidden = calm === null;
  u.calm.textContent = calm ?? '';
  u.body.hidden = calm !== null || v.preview;
  u.body.classList.toggle('editing', !!v.edit);
  u.editHost.hidden = calm !== null || !v.edit;
  u.preview.hidden = calm !== null || !v.preview || !!v.edit;
  u.editBtn.hidden = calm !== null || v.loading || !v.read || !!v.edit;
  u.saveBtn.hidden = !v.edit;
  u.view.classList.toggle('truncated', !!v.read?.truncated);
  const digits = String(Math.max(1, v.lines.length)).length;
  u.body.style.setProperty('--files-ln', `${Math.max(3, digits)}ch`);
  u.spacer.style.height = `${display().length * LINE_H}px`;
  if (Math.round(u.body.scrollTop / LINE_H) !== v.pos.top) u.body.scrollTop = v.pos.top * LINE_H;
  renderLines();
}

function searchSummary(s: Search): string {
  if (s.matches.length === 0) return `/${s.pattern}: no match`;
  if (s.current < 0) return `/${s.pattern} ${s.matches.length}${s.matches.length >= 20_000 ? '+' : ''} match${s.matches.length === 1 ? '' : 'es'}`;
  return `/${s.pattern} ${s.current + 1}/${s.matches.length}${s.matches.length >= 20_000 ? '+' : ''}`;
}

/** The viewer's shown lines (folded interiors removed). */
function display(): number[] {
  return viewer ? viewer.folds.display(viewer.lines.length) : [];
}

/** Keep the cursor on a shown line and on screen, and its column on a character. */
function settleViewer(): void {
  if (!viewer) return;
  // Editing: the caret is the text box's; only the scroll follows it.
  if (viewer.edit) { paintEdit(); return; }
  const disp = display();
  const ci = displayIndex(disp, viewer.pos.cursor);
  const p = settle({ cursor: ci, top: viewer.pos.top }, disp.length, viewRows());
  viewer.pos = { cursor: disp[p.cursor] ?? 0, top: p.top };
  viewer.col = Math.min(viewer.want, lastCol(viewer.chars, viewer.pos.cursor));
}

/** Put the cursor on line `row`, column `col` (opening a fold that hides it). */
function jumpTo(row: number, col: number): void {
  if (!viewer || !ui) return;
  viewer.folds.reveal(row);
  if (viewer.edit && area) { setCaret(row, col); paintEdit(); return; }
  const disp = display();
  const ci = displayIndex(disp, row);
  const rows = viewRows();
  const top = ci < viewer.pos.top || ci >= viewer.pos.top + rows ? ci - Math.floor(rows / 2) : viewer.pos.top;
  const p = settle({ cursor: ci, top }, disp.length, rows);
  viewer.pos = { cursor: disp[p.cursor] ?? 0, top: p.top };
  viewer.col = Math.min(col, lastCol(viewer.chars, viewer.pos.cursor));
  viewer.want = viewer.col;
  renderViewer();
}

function renderLines(): void {
  if (!ui || !viewer) return;
  const v = viewer;
  const disp = display();
  const rows = viewRows();
  const from = Math.max(0, v.pos.top - OVERSCAN);
  const to = Math.min(disp.length, v.pos.top + rows + OVERSCAN);
  const partner = focus === 'viewer' ? bracketPartner(v.chars, { row: v.pos.cursor, col: v.col }) : null;
  const current = search && search.current >= 0 ? search.matches[search.current] : undefined;
  const sel = v.edit ? editSelection(v) : new Map<number, Array<{ from: number; to: number }>>();
  const typing = v.edit ? typingCaret() : false;
  const out: HTMLElement[] = [];
  for (let d = from; d < to; d++) {
    const i = disp[d]!;
    const line = el('div', i === v.pos.cursor ? 'files-line cursor' : 'files-line');
    line.dataset.line = String(i);
    const sign = v.signs[i] ?? null;
    const region = v.folds.regionAt(i);
    const hidden = v.folds.summaryAt(i);
    const text = v.lines[i] ?? '';
    const decos: Deco[] = [];
    for (const m of search?.byRow.get(i) ?? []) decos.push({ from: m.start, to: m.end, cls: m === current ? 'current' : 'match' });
    if (partner && partner.row === i) decos.push({ from: partner.col, to: partner.col + 1, cls: 'bracket' });
    for (const r of sel.get(i) ?? []) decos.push({ from: r.from, to: r.to, cls: 'select' });
    if (i === v.pos.cursor) {
      if (partner) decos.push({ from: v.col, to: v.col + 1, cls: 'bracket' });
      decos.push({ from: v.col, to: v.col + 1, cls: typing ? 'caret' : 'cursor' });
    }
    const spans = v.lang && i < v.states.length && text.length <= MAX_HIGHLIGHT_LINE ? highlightLine(v.lang, text, v.states[i]!).spans : [];
    const body = el('span', 'files-text');
    for (const seg of segments(text, spans, decos)) {
      const cls = [seg.tok ? `hl-${seg.tok}` : '', ...seg.decos.map((c) => `v-${c}`)].filter(Boolean).join(' ');
      body.append(cls ? el('span', cls, seg.text) : document.createTextNode(seg.text));
    }
    if (hidden !== null) body.append(el('span', 'files-fold-summary', ` ··· ${hidden} line${hidden === 1 ? '' : 's'}`));
    line.append(
      el('span', sign ? `files-sign ${sign}` : 'files-sign', sign ? GUTTER_CHARS[sign] : ' '),
      el('span', 'files-fold', hidden !== null ? '▸' : region ? '▾' : ' '),
      el('span', 'files-ln', String(i + 1)),
      body,
    );
    out.push(line);
  }
  ui.window.style.transform = `translateY(${from * LINE_H}px)`;
  ui.window.replaceChildren(...out);
}

function moveViewer(m: ViewMove): void {
  if (!viewer || !ui) return;
  if (viewer.preview) { scrollPreview(m); return; }
  if (m.by === 'goto') { jumpTo(Math.max(0, Math.min(viewer.lines.length - 1, m.line - 1)), viewer.want); return; }
  const disp = display();
  const p = moveView({ cursor: displayIndex(disp, viewer.pos.cursor), top: viewer.pos.top }, disp.length, viewRows(), m);
  viewer.pos = { cursor: disp[p.cursor] ?? 0, top: p.top };
  viewer.col = Math.min(viewer.want, lastCol(viewer.chars, viewer.pos.cursor));
  ui.body.scrollTop = viewer.pos.top * LINE_H;
  renderViewer();
}

function moveCol(motion: Parameters<typeof colMotion>[2], n: number): void {
  if (!viewer || viewer.preview || viewer.chars.length === 0) return;
  const p = colMotion(viewer.chars, { row: viewer.pos.cursor, col: viewer.col }, motion, n);
  const want = motion === '$' ? Number.MAX_SAFE_INTEGER : p.col;
  jumpTo(p.row, p.col);
  viewer.want = want;
}

// ---------- search (/ ? n N * #) ----------

function setSearch(pattern: string, forward: boolean): void {
  if (!viewer) return;
  const matches = findMatches(viewer.lines, pattern);
  const byRow = new Map<number, Match[]>();
  for (const m of matches) {
    const l = byRow.get(m.row);
    if (l) l.push(m); else byRow.set(m.row, [m]);
  }
  search = { pattern, forward, matches, byRow, current: -1 };
}

/** Land on the hit nearest the cursor in the search's direction. */
function landSearch(from: { row: number; col: number }, strict = false): void {
  if (!viewer || !search) return;
  search.current = matchFrom(search.matches, from, search.forward, strict);
  const m = search.matches[search.current];
  if (m) jumpTo(m.row, m.start); else renderViewer();
}

function nextMatch(forward: boolean, n: number): void {
  if (!viewer) return;
  if (viewer.preview) { stepPreviewMark(forward === (search?.forward ?? true)); return; }
  if (!search || search.matches.length === 0) { toast(search ? `Pattern not found: ${search.pattern}` : 'No previous search pattern', true); return; }
  const dir = forward === search.forward;
  for (let i = 0; i < Math.max(1, n); i++) search.current = advanceMatch(search.current, search.matches.length, dir);
  const m = search.matches[search.current]!;
  jumpTo(m.row, m.start);
}

function starSearch(forward: boolean): void {
  if (!viewer || viewer.preview) return;
  const word = wordUnderCursor(viewer.chars, { row: viewer.pos.cursor, col: viewer.col });
  if (!word) { toast('No word under the cursor', true); return; }
  setSearch(word, forward);
  landSearch({ row: viewer.pos.cursor, col: viewer.col }, true);
}

function runFold(op: 'a' | 'o' | 'c' | 'M' | 'R'): void {
  if (!viewer || viewer.preview) return;
  const f = viewer.folds;
  if (f.regions.length === 0) { toast(viewer.lang ? 'Nothing to fold in this file' : 'Folds need a language the viewer knows (Rust, Python, JS/TS, Go, C/C++, shell, JSON, TOML, YAML, Markdown)', true); return; }
  const row = viewer.pos.cursor;
  let ok = true;
  if (op === 'a') ok = f.toggle(row);
  else if (op === 'o') ok = f.set(row, false);
  else if (op === 'c') ok = f.set(row, true);
  else if (op === 'M') f.closeAll();
  else f.openAll();
  if (!ok) { toast('No fold starts on this line', true); return; }
  settleViewer();
  renderViewer();
}

// ---------- markdown preview (Space m) ----------

function togglePreview(): void {
  if (!viewer || !ui) return;
  if (!isMarkdown(viewer.path)) { toast('Markdown preview is for .md files', true); return; }
  viewer.preview = !viewer.preview;
  renderViewer();
  if (viewer.preview) void renderPreview();
  setFocus('viewer');
}

async function renderPreview(): Promise<void> {
  if (!viewer || !ui) return;
  const v = viewer;
  const u = ui;
  u.preview.textContent = 'Rendering…';
  try {
    const [html] = await api.renderMarkdown([v.lines.join('\n')]);
    if (viewer !== v || !v.preview) return;
    u.preview.innerHTML = html ?? ''; // sanitized by the ammonia allowlist in markdown.rs, like every message body
    highlightCodeBlocks(u.preview);
    previewMarks = [];
    previewCurrent = -1;
    if (search) markPreview(search.pattern);
  } catch (e) {
    if (viewer === v) u.preview.textContent = `Markdown rendering unavailable: ${String(e)}`;
  }
}

function scrollPreview(m: ViewMove): void {
  if (!ui) return;
  const p = ui.preview;
  const page = p.clientHeight;
  switch (m.by) {
    case 'line': p.scrollTop += m.n * LINE_H; return;
    case 'half': p.scrollTop += m.n * Math.floor(page / 2); return;
    case 'page': p.scrollTop += m.n * page; return;
    case 'top': p.scrollTop = 0; return;
    case 'bottom': p.scrollTop = p.scrollHeight; return;
    case 'goto': p.scrollTop = viewer && viewer.lines.length > 0 ? (p.scrollHeight * (m.line - 1)) / viewer.lines.length : 0; return;
  }
}

/** Highlight `pattern` in the rendered preview (text nodes only). */
function markPreview(pattern: string): void {
  if (!ui) return;
  for (const m of ui.preview.querySelectorAll('mark.v-match')) m.replaceWith(document.createTextNode(m.textContent ?? ''));
  ui.preview.normalize();
  previewMarks = [];
  previewCurrent = -1;
  if (pattern === '') return;
  const walker = document.createTreeWalker(ui.preview, NodeFilter.SHOW_TEXT);
  const nodes: Text[] = [];
  for (let n = walker.nextNode(); n; n = walker.nextNode()) nodes.push(n as Text);
  for (const node of nodes) {
    let rest: Text = node;
    for (let at = rest.data.indexOf(pattern); at >= 0; at = rest.data.indexOf(pattern)) {
      const hit = rest.splitText(at);
      rest = hit.splitText(pattern.length);
      const mark = el('mark', 'v-match', hit.data);
      hit.replaceWith(mark);
      previewMarks.push(mark);
      if (previewMarks.length >= 2_000) return;
    }
  }
}

function stepPreviewMark(forward: boolean): void {
  if (previewMarks.length === 0) { toast(search ? `Pattern not found: ${search.pattern}` : 'No previous search pattern', true); return; }
  previewMarks[previewCurrent]?.classList.remove('v-current');
  previewCurrent = advanceMatch(previewCurrent, previewMarks.length, forward);
  const m = previewMarks[previewCurrent]!;
  m.classList.add('v-current');
  m.scrollIntoView({ block: 'center' });
}

function runViewerKey(ev: KeyboardEvent): boolean {
  const { cmd, next } = viewerKey(vkeys, ev);
  vkeys = next;
  switch (cmd.kind) {
    case 'pass': return false;
    case 'swallow': return true;
    case 'move': moveViewer(cmd.move); return true;
    case 'col': moveCol(cmd.motion, cmd.n); return true;
    case 'search': openPrompt(cmd.forward ? '/' : '?'); return true;
    case 'next-match': nextMatch(cmd.forward, cmd.n); return true;
    case 'star': starSearch(cmd.forward); return true;
    case 'fold': runFold(cmd.op); return true;
    case 'preview': togglePreview(); return true;
    case 'close': closeViewer(); return true;
    case 'explorer': dispatch('files-explorer', { source: 'key' }); return true;
    case 'picker': openPicker(false); return true;
    case 'focus-tree': if (explorerOpen) setFocus('tree'); return true;
    case 'back': setFocus('none'); toast('Focus is on the session list; click the viewer to come back', true); return true;
    case 'cmdline': openPrompt(':'); return true;
    case 'edit': startEdit(cmd.how, cmd.replay ? ev : null); return true;
    case 'save': void saveFile(false, false); return true;
    case 'say': toast(cmd.text); return true;
  }
}

function openPrompt(kind: ':' | '/' | '?'): void {
  if (!ui || !viewer) return;
  prompt = { kind, origin: { row: viewer.pos.cursor, col: viewer.col } };
  ui.colon.textContent = kind;
  ui.cmdinput.setAttribute('aria-label', kind === ':' ? 'Viewer command' : kind === '/' ? 'Search forward' : 'Search backward');
  ui.cmdline.hidden = false;
  ui.cmdinput.value = '';
  ui.cmdinput.focus();
}

function closeCmdline(): void {
  if (!ui) return;
  prompt = null;
  ui.cmdline.hidden = true;
  if (viewer) (viewer.edit && area ? area : viewer.preview ? ui.preview : ui.body).focus({ preventScroll: true });
}

/** Typing in a search prompt: incremental, every hit highlighted (`handle_search_input`). */
function searchInput(text: string): void {
  if (!viewer || !prompt || prompt.kind === ':') return;
  const forward = prompt.kind === '/';
  if (viewer.preview) {
    search = { pattern: text, forward, matches: [], byRow: new Map(), current: -1 };
    markPreview(text);
    if (previewMarks.length) stepPreviewMark(forward);
    return;
  }
  if (text === '') { search = null; jumpTo(prompt.origin.row, prompt.origin.col); return; }
  setSearch(text, forward);
  landSearch(prompt.origin);
}

/** Esc in a search prompt clears the search, as the TUI's does. */
function cancelPrompt(): void {
  if (prompt && prompt.kind !== ':') {
    search = null;
    if (viewer?.preview) markPreview('');
    renderViewer();
  }
  closeCmdline();
}

function runCmdline(text: string): void {
  const kind = prompt?.kind ?? ':';
  closeCmdline();
  if (kind !== ':') {
    if (text === '') { search = null; renderViewer(); return; }
    searchInput(text);
    return;
  }
  const c = viewerCommand(text);
  switch (c.kind) {
    case 'none': return;
    case 'quit': closeViewer(c.force); return;
    case 'goto':
      if (viewer?.edit && area) { jumpTo(Math.max(0, Math.min(viewer.lines.length - 1, c.line - 1)), 0); return; }
      moveViewer({ by: 'goto', line: c.line });
      return;
    case 'autopair':
      if (viewer?.edit) viewer.edit.autoPair = c.on;
      toast(`Auto-pair: ${c.on ? 'on' : 'off'}`, true);
      if (viewer?.edit) schedulePaint();
      return;
    case 'save': void saveFile(c.force, c.close); return;
    case 'reload': reloadFile(c.force); return;
    case 'say': toast(c.text); return;
  }
}

// ---------- editor (#113) ----------

function editDirty(): boolean {
  return !!(viewer?.edit && area && area.value !== viewer.edit.base);
}

/** Files under `key` with unsaved changes: the open buffer and the kept drafts. */
function unsavedPaths(key: string): Set<string> {
  const out = drafts.dirty(key);
  if (viewer && viewer.ctx.key === key && editDirty()) out.add(viewer.path);
  return out;
}

function editStatus(): string {
  const bits: string[] = [];
  if (isVimKeysOn() && area) {
    const m = modeOf(area);
    bits.push(m === 'insert' ? 'INSERT' : m === 'normal' ? 'NORMAL' : m === 'visual' ? 'VISUAL' : 'V-LINE');
    const pending = zPending ? 'z' : editorOf(area).pending();
    if (pending) bits.push(pending);
  } else {
    bits.push('EDIT');
  }
  if (area) {
    const lc = lineCol(area.value, caretOffset());
    bits.push(`${lc.line + 1}:${lc.col + 1}`);
  }
  if (editDirty()) bits.push('[+] unsaved');
  if (viewer?.edit && !viewer.edit.autoPair) bits.push('noautopair');
  if (viewer?.edit?.conflict) bits.push('changed on disk: :w! overwrites, :e! reloads');
  return bits.join(' · ');
}

function unmountArea(): void {
  area = null;
  shownDirty = false;
  zPending = false;
  if (heavyTimer !== null) { clearTimeout(heavyTimer); heavyTimer = null; }
  ui?.editHost.replaceChildren();
}

/**
 * A fresh text box (and so a fresh vim state) holding `text`. It is the editor's
 * input and model, not what is drawn: the lines are the viewer's, painted from
 * its text and caret (colours, search hits, bracket match, folds, git signs),
 * so the text box itself is invisible.
 */
function mountArea(text: string): HTMLTextAreaElement {
  unmountArea();
  const a = el('textarea', 'files-edit');
  a.value = text;
  a.spellcheck = false;
  a.setAttribute('wrap', 'off');
  a.setAttribute('autocapitalize', 'off');
  a.setAttribute('autocomplete', 'off');
  a.setAttribute('aria-label', 'File editor');
  // This module drives the vim editor itself (the `:` line, Esc, Ctrl-S), so the global vim-field adapter stays out.
  a.dataset.vim = 'off';
  a.addEventListener('keydown', editKey);
  a.addEventListener('input', onEditInput);
  a.addEventListener('keyup', schedulePaint);
  a.addEventListener('focus', schedulePaint);
  a.addEventListener('blur', schedulePaint);
  // A paste (Cmd-V, the context menu) types at the caret: out of normal mode first, as the TUI's paste enters insert mode.
  a.addEventListener('paste', () => { if (isVimKeysOn() && modeOf(a) !== 'insert') prepareForTyping(a); });
  ui?.editHost.append(a);
  area = a;
  return a;
}

/** The caret as a UTF-16 offset: the text box's in insert mode, the vim editor's otherwise. */
function caretOffset(): number {
  const a = area;
  if (!a) return 0;
  if (isVimKeysOn() && modeOf(a) !== 'insert') return Math.min(editorOf(a).cursorOffset(), a.value.length);
  return a.selectionStart;
}

/** Is the caret a bar between characters (typing) rather than a block on one (normal mode)? */
function typingCaret(): boolean {
  return !!area && (!isVimKeysOn() || modeOf(area) === 'insert');
}

/** The visual selection per line, in code-point columns. */
function editSelection(v: Viewer): Map<number, Array<{ from: number; to: number }>> {
  const out = new Map<number, Array<{ from: number; to: number }>>();
  if (!area || !isVimKeysOn()) return out;
  const sel = editorOf(area).selection();
  if (!sel) return out;
  for (const r of selectionRows(v.chars, area.value, sel.start, sel.end)) {
    const l = out.get(r.row);
    if (l) l.push({ from: r.from, to: r.to }); else out.set(r.row, [{ from: r.from, to: r.to }]);
  }
  return out;
}

/** Put the caret on line `row`, column `col` (code points); the vim editor follows. */
function setCaret(row: number, col: number): void {
  const a = area;
  const v = viewer;
  if (!a || !v) return;
  const off = posToOffset(v.chars, { row, col });
  a.setSelectionRange(off, off);
  if (isVimKeysOn() && modeOf(a) !== 'insert') enterNormal(a);
}

/**
 * Bring the view up to date with the text box: the viewer's cursor follows the
 * caret, which opens a fold it is typing in and steps over a closed one, and
 * the view scrolls to keep it shown.
 */
function paintEdit(): void {
  const v = viewer;
  const a = area;
  if (!ui || !v?.edit || !a) return;
  let p = offsetToPos(v.chars, Math.min(caretOffset(), a.value.length));
  if (typingCaret()) {
    v.folds.reveal(p.row);
  } else {
    const shown = display();
    const row = skipHidden(shown, p.row, editRow);
    if (row !== p.row) { setCaret(row, p.col); p = offsetToPos(v.chars, Math.min(caretOffset(), a.value.length)); }
  }
  editRow = p.row;
  const disp = display();
  const s = settle({ cursor: displayIndex(disp, p.row), top: v.pos.top }, disp.length, viewRows());
  v.pos = { cursor: p.row, top: s.top };
  v.col = p.col;
  renderViewer();
}

/** One paint per frame, whatever the number of keys, input events and clicks that asked for it. */
function schedulePaint(): void {
  if (paintQueued) return;
  paintQueued = true;
  requestAnimationFrame(() => { paintQueued = false; paintEdit(); });
}

/**
 * The text changed: the viewer's lines, colours, folds, search hits and git signs
 * follow it. The lines are kept at once; the heavier parts wait for a pause when
 * the file is long.
 */
function refreshBuffer(): void {
  const v = viewer;
  const a = area;
  if (!v?.edit || !a) return;
  bufferText = a.value;
  v.lines = bufferText.split('\n');
  v.chars = toLines(bufferText);
  if (v.lines.length <= HEAVY_LINES) { refreshHeavy(); return; }
  if (heavyTimer !== null) clearTimeout(heavyTimer);
  heavyTimer = setTimeout(() => { heavyTimer = null; refreshHeavy(); schedulePaint(); }, 150);
}

function refreshHeavy(): void {
  const v = viewer;
  if (!v?.edit) return;
  v.states = v.lang ? lexStates(v.lang, v.lines) : [];
  v.folds.setRegions(foldRegions(v.lang, v.lines));
  if (search && !v.preview) setSearch(search.pattern, search.forward);
  v.signs = shiftSigns(v.edit.baseSigns, v.edit.baseLines, v.lines);
}

function onEditInput(): void {
  if (!viewer?.edit || !area) return;
  const d = editDirty();
  if (d !== shownDirty) { shownDirty = d; renderTree(); uiChanged(); }
  if (area.value !== bufferText) refreshBuffer();
  paintEdit();
}

/** The state of a new edit of text `base` read as `version`. */
function newEditor(eol: Eol, base: string, version: string, conflict: boolean, signs: Array<GutterSign | null>): Editor {
  return { eol, base, version, conflict, autoPair: true, baseLines: base.split('\n'), baseSigns: signs };
}

/** Open the text box on `text` with the caret at the viewer's cursor, in the mode `how` names. */
function openBuffer(v: Viewer, text: string, how: EditEntry, replay: KeyboardEvent | null, caret: number | null = null): void {
  const a = mountArea(text);
  refreshBuffer();
  const at = caret ?? Math.min(text.length, posToOffset(toLines(text), { row: v.pos.cursor, col: v.col }));
  renderViewer();
  setFocus('viewer');
  if (isVimKeysOn()) {
    // Enter normal mode on the character under the cursor (leaving insert steps left), then the key's own entry.
    const entry = normalEntryOffset(text, at);
    a.setSelectionRange(entry, entry);
    enterNormal(a);
    if (how !== 'normal') enterInsert(a, how);
    if (replay) editKey(replay);
  } else {
    a.setSelectionRange(at, at);
  }
  shownDirty = editDirty();
  editRow = offsetToPos(v.chars, caretOffset()).row;
  paintEdit();
  renderTree();
  uiChanged();
}

function startEdit(how: EditEntry, replay: KeyboardEvent | null): void {
  const v = viewer;
  if (!v || !ui) return;
  if (v.edit && area) { setFocus('viewer'); return; }
  if (v.loading) { toast('Still loading the file…'); return; }
  if (v.preview) { toast('Leave the Markdown preview first (Space m)'); return; }
  const read = v.read;
  if (!read) { toast(v.error ?? 'No file is loaded'); return; }
  const why = editRefusal(read, canReadMore(read, v.cap));
  if (why) { toast(why); return; }
  const eol = eolOf(read.content ?? '');
  const base = toBuffer(read.content ?? '', eol);
  v.edit = newEditor(eol, base, read.version ?? '', false, v.signs);
  openBuffer(v, base, how, replay);
}

/** A kept draft comes back (the file is open again): where it was left, with a conflict flagged when the disk moved on. */
function resumeDraft(d: Draft, diskVersion: string | undefined): void {
  const v = viewer;
  if (!v || !ui) return;
  const moved = diskVersion !== undefined && diskVersion !== d.version;
  v.edit = newEditor(d.eol, d.base, d.version, d.conflict || moved, v.signs);
  openBuffer(v, d.text, 'normal', null, Math.min(d.cursor, d.text.length));
  if (v.edit.conflict) toast(`${v.path} changed on disk since this buffer started: :w! overwrites it, :e! reloads it`);
}

/** Set an unsaved buffer aside (another file is opening), or drop a clean one. */
function stashEdit(): void {
  const v = viewer;
  if (!v?.edit || !area) return;
  if (editDirty()) {
    drafts.put(v.ctx.key, {
      path: v.path, text: area.value, base: v.edit.base, version: v.edit.version, eol: v.edit.eol,
      conflict: v.edit.conflict, cursor: caretOffset(),
    });
  } else {
    drafts.drop(v.ctx.key, v.path);
  }
  v.edit = null;
  unmountArea();
}

/** Back to the read-only view (clean buffers only: the caret becomes the viewer's cursor). */
function leaveEditor(): void {
  const v = viewer;
  if (!v?.edit || !area) return;
  if (editDirty()) { toast('Unsaved changes: :w saves, :e! discards them, :q! closes without saving'); return; }
  const lc = lineCol(area.value, caretOffset());
  const content = toDisk(area.value, v.edit.eol);
  v.edit = null;
  unmountArea();
  applyContent(v, content);
  v.signs = gutterSignsFor(v.signs, v.lines.length);
  v.pos = { cursor: Math.min(lc.line, Math.max(0, v.lines.length - 1)), top: v.pos.top };
  v.want = lc.col;
  settleViewer();
  renderViewer();
  setFocus('viewer');
  renderTree();
  uiChanged();
}

/** The signs for `n` lines (the editor's buffer has one more line than the viewer's). */
function gutterSignsFor(signs: Array<GutterSign | null>, n: number): Array<GutterSign | null> {
  return Array.from({ length: n }, (_, i) => signs[i] ?? null);
}

/** Type `edit` into the text box through the browser's own edit (so Ctrl-Z still undoes it), then move the caret. */
function applyTextEdit(a: HTMLTextAreaElement, edit: TextEdit): void {
  if (edit.insert === '' && edit.from === edit.to) { a.setSelectionRange(edit.caret, edit.caret); return; }
  a.setSelectionRange(edit.from, edit.to);
  let done = false;
  try { done = document.execCommand(edit.insert === '' ? 'delete' : 'insertText', false, edit.insert); } catch { done = false; }
  if (!done) {
    a.setRangeText(edit.insert, edit.from, edit.to, 'end');
    a.dispatchEvent(new Event('input', { bubbles: true }));
  }
  a.setSelectionRange(edit.caret, edit.caret);
}

/** Ctrl-V: the system clipboard goes in at the caret, from any mode (`try_paste_clipboard_file_viewer`). */
async function pasteClipboard(): Promise<void> {
  const a = area;
  if (!a) return;
  let clip = '';
  try { clip = (await navigator.clipboard.readText()) ?? ''; } catch { toast('The clipboard cannot be read here: use Cmd-V', true); return; }
  if (area !== a) return;
  if (clip === '') { toast('Clipboard empty', true); return; }
  if (isVimKeysOn() && modeOf(a) !== 'insert') enterInsert(a, 'i');
  const at = a.selectionStart;
  const text = pastedText(clip);
  applyTextEdit(a, { from: at, to: a.selectionEnd, insert: text, caret: at + text.length });
}

/** Keys in the text box: vim's (when Vim keys is on), `:` for the viewer line, search and folds, Ctrl-S, Esc. */
function editKey(ev: KeyboardEvent): void {
  editKeyRun(ev);
  schedulePaint();
}

function editKeyRun(ev: KeyboardEvent): void {
  const v = viewer;
  const a = area;
  if (!v?.edit || !a) return;
  if (ev.isComposing) return;
  const stop = () => { ev.preventDefault(); ev.stopPropagation(); };
  const bare = !ev.ctrlKey && !ev.metaKey && !ev.altKey;
  if ((ev.ctrlKey || ev.metaKey) && !ev.altKey && ev.key.toLowerCase() === 's') {
    stop();
    void saveFile(false, false);
    return;
  }
  // Ctrl-V is the TUI's clipboard paste (the TUI's viewer has no visual-block mode); Cmd-V is the text box's own.
  if (ev.ctrlKey && !ev.metaKey && !ev.altKey && !ev.shiftKey && ev.key.toLowerCase() === 'v') { stop(); void pasteClipboard(); return; }
  const typing = !isVimKeysOn() || modeOf(a) === 'insert';
  // Bracket auto-pairing while typing (`handle_auto_pair`).
  if (typing && v.edit.autoPair && bare) {
    const r = autoPairEdit(a.value, a.selectionStart, a.selectionEnd, ev.key);
    if (r) { stop(); applyTextEdit(a, r); return; }
  }
  if (!isVimKeysOn()) {
    // Plain text box: Esc leaves a clean buffer; the toolbar and Ctrl-S cover the rest.
    if (ev.key === 'Escape' && !ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey) { stop(); leaveEditor(); }
    return;
  }
  const mode = modeOf(a);
  if (ev.key === 'Escape' && mode === 'normal' && editorOf(a).idle()) { stop(); zPending = false; leaveEditor(); return; }
  if (ev.key === ':' && mode !== 'insert' && bare) { stop(); openPrompt(':'); return; }
  if (mode === 'normal' && editorOf(a).idle() && bare) {
    // The viewer's normal-mode keys that work while editing: z folds, / ? n N * # search.
    if (zPending) {
      stop();
      zPending = false;
      if (ev.key === 'a' || ev.key === 'o' || ev.key === 'c' || ev.key === 'M' || ev.key === 'R') runFold(ev.key);
      return;
    }
    if (ev.key === 'z') { stop(); zPending = true; return; }
    if (ev.key === '/' || ev.key === '?') { stop(); openPrompt(ev.key); return; }
    if (ev.key === 'n' || ev.key === 'N') { stop(); nextMatch(ev.key === 'n', 1); return; }
    if (ev.key === '*' || ev.key === '#') { stop(); starSearch(ev.key === '*'); return; }
  }
  zPending = false;
  const r = vimKey(a, ev, currentPlatform());
  if (r.yank) void navigator.clipboard?.writeText(r.yank).catch(() => undefined);
  if (r.outcome === 'pass') return;
  if (r.outcome !== 'unhandled') { stop(); onEditInput(); return; }
  const t = r.token ?? '';
  // Chords and Tab belong to the app; any other key never types in normal mode.
  if (/^[CA]-/.test(t) || t === 'Tab' || t === 'S-Tab' || t === 'Esc') return;
  ev.preventDefault();
  if (t !== 'Space') ev.stopPropagation();
}

// ---------- saving ----------

/** The viewer's lines from `content` (after a save). */
function applyContent(v: Viewer, content: string): void {
  v.lines = splitLines(content);
  v.chars = toLines(v.lines.join('\n'));
  v.states = v.lang ? lexStates(v.lang, v.lines) : [];
  v.folds.setRegions(foldRegions(v.lang, v.lines));
  v.pos = { cursor: Math.min(v.pos.cursor, Math.max(0, v.lines.length - 1)), top: v.pos.top };
}

function refreshGutter(v: Viewer, ed: Editor): void {
  void filesApi.gitGutter(v.ctx.root, v.path).then((g) => {
    if (viewer !== v) return;
    v.note = gutterNote(g.status);
    if (v.edit === ed) {
      // The signs belong to the saved text; the unsaved edits since carry them along.
      ed.baseSigns = gutterSigns(g.markers, ed.baseLines.length);
      v.signs = shiftSigns(ed.baseSigns, ed.baseLines, v.lines);
    } else {
      v.signs = gutterSigns(g.markers, v.lines.length);
    }
    renderViewer();
  }, () => undefined);
}

/** The buffer is on disk (or was found equal to it): the fence moves to `version` and the buffer is clean. */
function markSaved(v: Viewer, ed: Editor, buffer: string, version: string, size: number): void {
  const content = toDisk(buffer, ed.eol);
  ed.base = buffer;
  ed.baseLines = buffer.split('\n');
  ed.version = version;
  ed.conflict = false;
  // What was written is the file's new line-ending record (a mixed file's breaks are as saved).
  ed.eol = eolOf(content);
  if (v.read) v.read = { ...v.read, content, size, version, truncated: false, binary: false };
  if (v.edit === ed && area) { ed.baseSigns = gutterSignsFor(v.signs, ed.baseLines.length); refreshBuffer(); } else applyContent(v, content);
  drafts.drop(v.ctx.key, v.path);
  if (viewer === v) {
    refreshGutter(v, ed);
    shownDirty = false;
    renderViewer();
    renderTree();
    uiChanged();
  }
}

/**
 * `:w` / `:w!` / `:wq` / Ctrl-S (file_viewer.rs `save_file`). The save is fenced
 * by the version the buffer started from: a file that changed on disk is
 * refused (rsid answers `project_file_stale`), unless the buffer equals the
 * disk (then nothing is written), the file was deleted (a user-initiated save
 * creates it), or `force` overwrites it. Resolves true when the buffer is on disk.
 */
async function saveFile(force: boolean, thenClose: boolean): Promise<boolean> {
  const v = viewer;
  if (!v || !ui) return false;
  if (saving) { toast('A save is already running'); return false; }
  // In the read-only view `:w` rewrites the file as it was read, the way the TUI's viewer does (it is always the editor).
  let ed = v.edit;
  let buffer = area?.value ?? '';
  if (!ed || !area) {
    const read = v.read;
    if (!read) { toast(v.error ?? 'No file is loaded'); return false; }
    const why = editRefusal(read, canReadMore(read, v.cap));
    if (why) { toast(why); return false; }
    const eol = eolOf(read.content ?? '');
    ed = newEditor(eol, toBuffer(read.content ?? '', eol), read.version ?? '', false, v.signs);
    buffer = ed.base;
  }
  const path = v.path;
  if (tooLargeToSave(buffer, ed.eol)) { toast(failedText(path, 'the text is larger than the 8 MiB rsid accepts for a save')); return false; }
  saving = true;
  try {
    let wrote: Awaited<ReturnType<typeof filesApi.writeFile>> | null = null;
    try {
      wrote = await filesApi.writeFile(saveRequest(v.ctx.root, path, buffer, ed.eol, ed.version, force));
    } catch (e) {
      if (force || refusalCode(e) !== 'project_file_stale') {
        toast(failedText(path, writeErrorText(e, { rootName: v.ctx.label })));
        return false;
      }
      // Changed on disk: read it again and decide as the TUI does.
      let read: FileRead | null = null;
      let code: string | null = null;
      try { read = await filesApi.readFile(v.ctx.root, path, MAX_READ_BYTES); } catch (re) { code = refusalCode(re); }
      const verdict = staleVerdict(buffer, ed.eol, read, code);
      if (verdict.kind === 'up-to-date') {
        markSaved(v, ed, buffer, verdict.version, read?.size ?? 0);
        toast(upToDateText(path), true);
        if (thenClose && viewer === v) closeViewer(true);
        return true;
      }
      if (verdict.kind === 'recreate') {
        try {
          wrote = await filesApi.writeFile(saveRequest(v.ctx.root, path, buffer, ed.eol, null, true));
        } catch (we) {
          toast(failedText(path, writeErrorText(we, { rootName: v.ctx.label })));
          return false;
        }
      } else {
        ed.conflict = true;
        toast(verdict.kind === 'refused' ? refusedChangedText(path) : refusedUnverifiedText(path));
        schedulePaint();
        return false;
      }
    }
    markSaved(v, ed, buffer, wrote.version, wrote.size);
    toast(savedText(path, wrote.forced, wrote.created), true);
    if (wrote.created && treeCtx?.key === v.ctx.key) {
      const dir = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '';
      if (tree.loadedDirs().includes(dir)) void loadDir(dir);
    }
    if (thenClose && viewer === v) closeViewer(true);
    return true;
  } finally {
    saving = false;
  }
}

/** `:e!` reloads the file from disk, discarding the buffer; `:e` does so only for a clean one. */
function reloadFile(force: boolean): void {
  const v = viewer;
  if (!v) return;
  if (v.edit && !force && editDirty()) { toast(UNSAVED_RELOAD); return; }
  const resume = !!v.edit;
  drafts.drop(v.ctx.key, v.path);
  openFile(v.ctx, v.path, true, v.cap, { resume });
}

// ---------- explorer operations (files-ops.ts) ----------

function opsHost(): OpsHost {
  return {
    root: () => (explorerOpen && treeCtx ? { root: treeCtx.root, abs: treeCtx.abs, name: treeCtx.label } : null),
    selection: () => tree.selectedEntry(),
    showHidden: () => tree.showHidden,
    listed: (dir) => {
      const n = tree.dirs.get(dir);
      return n && n.status === 'loaded' ? n.entries : null;
    },
    list: async (dir) => {
      const c = treeCtx;
      if (!c) return null;
      try { return (await filesApi.listDirectory(c.root, dir, DIRECTORY_ENTRIES)).entries; } catch { return null; }
    },
    isDir: async (path) => {
      const c = treeCtx;
      if (!c) return false;
      try { await filesApi.listDirectory(c.root, path, 1); return true; } catch { return false; }
    },
    changed: (c) => { void afterChange(c); },
    focusTree: () => { if (explorerOpen) setFocus('tree'); },
  };
}

/** Point the open viewer and the kept drafts at `to` after `from` moved. */
function retargetOpen(key: string, from: string, to: string): void {
  drafts.retarget(key, from, to);
  const v = viewer;
  if (v && v.ctx.key === key && (v.path === from || v.path.startsWith(`${from}/`))) {
    v.path = `${to}${v.path.slice(from.length)}`;
    if (v.read) v.read = { ...v.read, path: v.path };
    renderViewer();
  }
}

/** rsid changed files: reload the touched folders, select the result, follow moved files. */
async function afterChange(c: { dirs: string[]; select: string | null; moved: { from: string; to: string } | null }): Promise<void> {
  const tctx = treeCtx;
  if (!tctx) return;
  if (c.moved) retargetOpen(tctx.key, c.moved.from, c.moved.to);
  const loaded = new Set(tree.loadedDirs());
  await Promise.all([...new Set(c.dirs)].filter((d) => loaded.has(d)).map((d) => loadDir(d)));
  if (treeCtx !== tctx) return;
  if (c.select !== null) {
    await Promise.all(tree.reveal(c.select).map((d) => loadDir(d)));
    tree.selected = c.select;
  }
  tree.fixSelection();
  renderTree();
  uiChanged();
}

// ---------- picker ----------

function openPicker(reveal: boolean): void {
  const c = activeCtx();
  if (!('key' in c)) { toast(c.why); return; }
  const u = build();
  pickerReturn = focus;
  picker = { ctx: c, paths: [], truncated: false, loading: true, error: null, results: [], selected: 0, reveal };
  u.pinput.value = '';
  u.picker.hidden = false;
  renderPicker();
  u.pinput.focus();
  uiChanged();
  const mine = ++pickerEpoch;
  void filesApi.listFiles(c.root, PICKER_PATHS).then((l) => {
    if (mine !== pickerEpoch || !picker) return;
    c.abs ??= l.root;
    picker.paths = l.paths;
    picker.truncated = l.truncated;
    picker.loading = false;
    rescore();
  }, (e) => {
    if (mine !== pickerEpoch || !picker) return;
    picker.loading = false;
    picker.error = fileErrorText(e);
    renderPicker();
  });
}

function closePicker(): void {
  picker = null;
  pickerEpoch++;
  if (!ui) return;
  ui.picker.hidden = true;
  setFocus(pickerReturn === 'tree' && explorerOpen ? 'tree' : pickerReturn === 'viewer' && viewer ? 'viewer' : 'none');
}

function rescore(): void {
  if (!picker || !ui) return;
  picker.results = rankPaths(picker.paths, ui.pinput.value);
  picker.selected = 0;
  renderPicker();
}

function renderPicker(): void {
  if (!ui || !picker) return;
  const p = picker;
  const u = ui;
  if (p.error) u.pstatus.textContent = p.error;
  else if (p.loading) u.pstatus.textContent = `Listing files in ${p.ctx.label}…`;
  else {
    const n = p.paths.length;
    u.pstatus.textContent = `${p.results.length} of ${n}${p.truncated ? '+' : ''} files in ${p.ctx.label}${p.truncated ? ' (list capped)' : ''}`;
  }
  u.pstatus.classList.toggle('error', !!p.error);
  const items = p.results.map((r, i) => {
    const li = el('li', i === p.selected ? 'files-pick selected' : 'files-pick');
    li.dataset.index = String(i);
    li.setAttribute('role', 'option');
    const hits = new Set(r.hits);
    let run = '';
    let hot = false;
    const flush = () => { if (run) li.append(hot ? el('mark', '', run) : document.createTextNode(run)); run = ''; };
    for (let j = 0; j < r.path.length; j++) {
      const h = hits.has(j);
      if (h !== hot) { flush(); hot = h; }
      run += r.path[j];
    }
    flush();
    return li;
  });
  u.plist.replaceChildren(...items);
  u.plist.querySelector<HTMLElement>('.files-pick.selected')?.scrollIntoView({ block: 'nearest' });
}

function pickOpen(): void {
  if (!picker) return;
  const r = picker.results[picker.selected];
  if (!r) return;
  const { ctx, reveal } = picker;
  closePicker();
  openFile(ctx, r.path, true);
  if (reveal && explorerOpen && treeCtx?.key === ctx.key) {
    for (const dir of tree.reveal(r.path)) void loadDir(dir);
    renderTree();
  }
}

// ---------- wiring ----------

/** The column (code points) of a click at (x, y) on a drawn line; a click past its end lands after the last character. */
function columnAt(lineEl: HTMLElement, x: number, y: number, text: string): number {
  const body = lineEl.querySelector<HTMLElement>('.files-text');
  if (!body) return 0;
  const doc = document as Document & {
    caretPositionFromPoint?: (x: number, y: number) => { offsetNode: Node; offset: number } | null;
  };
  let node: Node | null = null;
  let off = 0;
  if (typeof doc.caretRangeFromPoint === 'function') {
    const r = doc.caretRangeFromPoint(x, y);
    node = r?.startContainer ?? null;
    off = r?.startOffset ?? 0;
  } else if (typeof doc.caretPositionFromPoint === 'function') {
    const p = doc.caretPositionFromPoint(x, y);
    node = p?.offsetNode ?? null;
    off = p?.offset ?? 0;
  }
  if (!node || !body.contains(node)) return x > body.getBoundingClientRect().left ? [...text].length : 0;
  const range = document.createRange();
  range.selectNodeContents(body);
  range.setEnd(node, off);
  return [...text.slice(0, Math.min(text.length, range.toString().length))].length;
}

function wire(u: NonNullable<typeof ui>): void {
  u.tree.addEventListener('contextmenu', (ev) => {
    ev.preventDefault();
    const row = (ev.target as HTMLElement).closest<HTMLElement>('.files-row[data-path]');
    const path = row?.dataset.path ?? null;
    if (path !== null) tree.selected = path;
    setFocus('tree', false);
    renderTree();
    ops?.menu(ev.clientX, ev.clientY, path !== null ? tree.selectedEntry() : null);
  });
  u.tree.addEventListener('click', (ev) => {
    const row = (ev.target as HTMLElement).closest<HTMLElement>('.files-row[data-path]');
    if (!row) return;
    ops?.cancel();
    const path = row.dataset.path!;
    const again = tree.selected === path;
    tree.selected = path;
    setFocus('tree');
    if (again) applyTree(tree.activate(), true); else afterTreeMove();
  });
  u.tree.addEventListener('wheel', (ev) => {
    if (focus !== 'tree') return;
    ev.preventDefault();
    tree.move(ev.deltaY > 0 ? 1 : -1);
    afterTreeMove();
  }, { passive: false });
  u.body.addEventListener('scroll', () => {
    if (!viewer || !ui) return;
    const top = Math.round(ui.body.scrollTop / LINE_H);
    if (top === viewer.pos.top) return;
    viewer.pos = { cursor: viewer.pos.cursor, top };
    // Editing: scrolling does not move the caret.
    if (!viewer.edit) settleViewer();
    renderLines();
  });
  // Editing, a press on the lines keeps the text box focused (it takes the keys and the caret).
  u.body.addEventListener('mousedown', (ev) => { if (viewer?.edit) ev.preventDefault(); });
  u.body.addEventListener('click', (ev) => {
    if (!viewer) return;
    const target = ev.target as HTMLElement;
    const line = target.closest<HTMLElement>('.files-line');
    if (!line?.dataset.line) return;
    const row = Number(line.dataset.line);
    if (target.closest('.files-fold') && viewer.folds.regionAt(row)) {
      if (viewer.edit) setCaret(row, viewer.col);
      viewer.pos = { cursor: row, top: viewer.pos.top };
      runFold('a');
      return;
    }
    if (viewer.edit && area) {
      setCaret(row, columnAt(line, ev.clientX, ev.clientY, viewer.lines[row] ?? ''));
      area.focus({ preventScroll: true });
      paintEdit();
      return;
    }
    viewer.pos = { cursor: row, top: viewer.pos.top };
    settleViewer();
    renderViewer();
  });
  u.more.addEventListener('click', () => { if (viewer) openFile(viewer.ctx, viewer.path, true, MAX_READ_BYTES); });
  u.cmdline.addEventListener('submit', (ev) => { ev.preventDefault(); runCmdline(u.cmdinput.value); });
  u.cmdinput.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); cancelPrompt(); }
  });
  u.cmdinput.addEventListener('input', () => searchInput(u.cmdinput.value));
  u.cmdinput.addEventListener('blur', () => { u.cmdline.hidden = true; prompt = null; });
  u.pinput.addEventListener('input', () => rescore());
  let leader = false;
  u.pinput.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing || !picker) return;
    // Space N / Space ; (the TUI telescope's leader) while the query is empty.
    const l = pickerLeader(leader, ev, u.pinput.value === '' && !picker.reveal);
    leader = l.armed;
    if (l.run) {
      ev.preventDefault();
      ev.stopPropagation();
      if (l.run !== 'arm') { closePicker(); runLeader(l.run); }
      return;
    }
    const c = pickerKey(ev);
    if (c.kind === 'type') return;
    ev.preventDefault();
    ev.stopPropagation();
    switch (c.kind) {
      case 'close': closePicker(); return;
      case 'open': pickOpen(); return;
      case 'clear': u.pinput.value = ''; rescore(); return;
      case 'clear-segment': u.pinput.value = dropSegment(u.pinput.value); rescore(); return;
      case 'move': picker.selected = movePickerSelection(picker.selected, picker.results.length, c.n); renderPicker(); return;
    }
  });
  u.plist.addEventListener('mousedown', (ev) => ev.preventDefault()); // keep the query focused
  u.plist.addEventListener('click', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.files-pick');
    if (!li || !picker) return;
    picker.selected = Number(li.dataset.index);
    pickOpen();
  });
}

const CSS = `
.files-overlay { position: fixed; z-index: 8; display: flex; background: var(--base); border-left: 1px solid var(--surface0); }
.files-overlay[hidden] { display: none; }
.files-overlay.drawer-only { background: transparent; border-left: 0; pointer-events: none; }
.files-overlay.drawer-only > * { pointer-events: auto; }
.files-drawer { display: flex; flex-direction: column; flex: none; min-width: 0; background: var(--mantle); border-right: 1px solid var(--surface1); }
.files-drawer[hidden], .files-viewer[hidden] { display: none; }
.files-overlay.drawer-only .files-drawer { box-shadow: 8px 0 24px rgba(0, 0, 0, 0.35); }
.files-head { display: flex; align-items: center; gap: 8px; padding: 4px 8px 4px 12px; border-bottom: 1px solid var(--surface0); font-size: 12px; min-height: 30px; }
.files-title, .files-path { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--mono); font-weight: 600; }
.files-flags, .files-meta { color: var(--subtext); white-space: nowrap; font-size: 11px; }
.files-drawer.focused .files-head, .files-viewer.focused .files-head { border-bottom-color: var(--blue); }
.files-tree { list-style: none; margin: 0; padding: 4px 0; overflow: auto; flex: 1; outline: none; font-family: var(--mono); font-size: 12px; }
.files-row { height: ${ROW_H}px; line-height: ${ROW_H}px; padding-right: 8px; white-space: nowrap; cursor: default; display: flex; gap: 6px; }
.files-row .files-icon { width: 1ch; color: var(--subtext); flex: none; }
.files-row .files-name { overflow: hidden; text-overflow: ellipsis; }
.files-row.dir .files-name { color: var(--blue); }
.files-row.symlink .files-name { color: var(--teal); }
.files-row.ignored { opacity: 0.5; }
.files-row.open .files-name { color: var(--peach); font-weight: 600; }
.files-row.selected { background: var(--surface0); }
.files-drawer.focused .files-row.selected { background: var(--surface1); }
.files-row.note { color: var(--subtext); font-style: italic; }
.files-row.note.error { color: var(--subtext); font-style: normal; white-space: normal; height: auto; line-height: 1.4; padding-top: 4px; padding-bottom: 4px; }
.files-viewer { display: flex; flex-direction: column; flex: 1; min-width: 0; position: relative; }
.files-calm { margin: 24px; padding: 12px 14px; border: 1px solid var(--surface1); border-radius: 6px; color: var(--subtext); background: var(--mantle); }
.files-calm[hidden] { display: none; }
.files-body { position: relative; flex: 1; overflow: auto; outline: none; font-family: var(--mono); font-size: 12px; }
.files-body[hidden] { display: none; }
.files-spacer { position: relative; min-width: 100%; }
.files-window { position: absolute; left: 0; top: 0; min-width: 100%; }
.files-line { height: ${LINE_H}px; line-height: ${LINE_H}px; display: flex; white-space: pre; }
.files-line.cursor { background: var(--surface0); }
.files-sign { width: 2ch; flex: none; text-align: center; font-weight: 700; }
.files-sign.added { color: var(--green); }
.files-sign.modified { color: var(--yellow); }
.files-sign.removed { color: var(--red); }
.files-ln { width: var(--files-ln, 3ch); flex: none; text-align: right; padding-right: 1ch; color: var(--subtext); opacity: 0.7; user-select: none; }
.files-line.cursor .files-ln { opacity: 1; color: var(--text); }
.files-text { padding-right: 2ch; }
.files-fold { width: 1.5ch; flex: none; color: var(--overlay0, var(--subtext)); cursor: pointer; user-select: none; }
.files-fold-summary { color: var(--subtext); font-style: italic; }
.files-text .v-match { background: color-mix(in srgb, var(--yellow) 30%, transparent); }
.files-text .v-current { background: var(--yellow); color: var(--base); }
.files-text .v-bracket { background: color-mix(in srgb, var(--mauve) 35%, transparent); }
.files-viewer.focused .files-text .v-cursor { outline: 1px solid var(--text); background: var(--surface2, var(--surface1)); }
.files-preview { flex: 1; overflow: auto; padding: 12px 20px; outline: none; }
.files-preview[hidden] { display: none; }
.files-preview mark.v-match { background: color-mix(in srgb, var(--yellow) 30%, transparent); color: inherit; }
.files-preview mark.v-current { background: var(--yellow); color: var(--base); }
.hl-keyword { color: var(--mauve); }
.hl-function { color: var(--blue); }
.hl-string, .hl-code { color: var(--green); }
.hl-number, .hl-constant { color: var(--peach); }
.hl-type, .hl-attribute { color: var(--yellow); }
.hl-variable { color: var(--red); }
.hl-comment { color: var(--overlay0, var(--subtext)); font-style: italic; }
.hl-operator { color: var(--sky, var(--teal)); }
.hl-property { color: var(--lavender, var(--blue)); }
.hl-punctuation { color: var(--overlay2, var(--subtext)); }
.hl-tag { color: var(--red); }
.hl-heading { color: var(--blue); font-weight: 700; }
.hl-strong { font-weight: 700; }
.hl-emphasis { font-style: italic; }
.hl-link { color: var(--sapphire, var(--blue)); text-decoration: underline; }
.files-cmdline { display: flex; align-items: center; gap: 2px; padding: 4px 8px; border-top: 1px solid var(--surface0); font-family: var(--mono); }
.files-cmdline[hidden] { display: none; }
.files-cmdinput { flex: 1; background: transparent; border: 0; color: var(--text); font: inherit; outline: none; }
.files-ops { display: flex; flex-wrap: wrap; gap: 4px; padding: 4px 8px; border-bottom: 1px solid var(--surface0); }
.files-op { font-size: 11px; padding: 1px 6px; }
.files-draft { margin-left: auto; color: var(--yellow); flex: none; }
.files-prompt { border-top: 1px solid var(--surface1); background: var(--base); padding: 6px 8px; display: flex; flex-direction: column; gap: 4px; outline: none; font-size: 12px; }
.files-prompt[hidden] { display: none; }
.files-prompt-label { font-weight: 600; color: var(--blue); overflow-wrap: anywhere; }
.files-prompt.confirm .files-prompt-label { color: var(--red); }
.files-prompt-input { background: var(--mantle); border: 1px solid var(--surface1); border-radius: 4px; color: var(--text); font-family: var(--mono); padding: 4px 6px; }
.files-prompt-input[hidden], .files-prompt-buttons[hidden] { display: none; }
.files-prompt-hint { color: var(--subtext); font-size: 11px; }
.files-prompt-buttons { display: flex; gap: 6px; }
.files-menu { position: fixed; z-index: 20; list-style: none; margin: 0; padding: 4px 0; min-width: 200px; background: var(--base); border: 1px solid var(--surface1); border-radius: 6px; box-shadow: 0 8px 24px rgba(0, 0, 0, 0.45); font-size: 12px; }
.files-menu-item { display: flex; justify-content: space-between; gap: 16px; padding: 4px 12px; cursor: default; outline: none; }
.files-menu-item:hover, .files-menu-item:focus { background: var(--surface1); }
.files-menu-key { color: var(--subtext); font-family: var(--mono); }
.files-edit-host { position: absolute; left: 0; top: 0; width: 1px; height: 1px; overflow: hidden; opacity: 0; pointer-events: none; }
.files-edit-host[hidden] { display: none; }
.files-edit { width: 1px; height: 1px; border: 0; padding: 0; resize: none; outline: none; background: transparent; color: transparent; font: inherit; white-space: pre; }
.files-viewer.focused .files-text .v-caret { box-shadow: inset 2px 0 0 var(--text); }
.files-text .v-select { background: var(--surface2, var(--surface1)); }
.files-body.editing { cursor: text; }
.files-picker { position: fixed; z-index: 10; left: 50%; top: 12%; transform: translateX(-50%); width: min(720px, 90vw); max-height: 70vh; display: flex; flex-direction: column;
  background: var(--base); border: 1px solid var(--surface1); border-radius: 8px; box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5); overflow: hidden; }
.files-picker[hidden] { display: none; }
.files-picker-input { margin: 8px; padding: 6px 8px; background: var(--mantle); border: 1px solid var(--surface1); border-radius: 4px; color: var(--text); font-family: var(--mono); }
.files-picker-status { padding: 0 12px 6px; font-size: 11px; color: var(--subtext); }
.files-picker-list { list-style: none; margin: 0; padding: 0 0 6px; overflow: auto; font-family: var(--mono); font-size: 12px; }
.files-pick { padding: 3px 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; cursor: pointer; }
.files-pick.selected { background: var(--surface1); }
.files-pick mark { background: transparent; color: var(--yellow); font-weight: 700; }
`;

export function initFiles(): void {
  const style = document.createElement('style');
  style.id = 'files-style';
  style.textContent = CSS;
  document.head.append(style);
  const saved = Number(storage()?.getItem(WIDTH_KEY));
  if (Number.isFinite(saved) && saved > 0) width = saved;
  window.addEventListener('resize', () => { if (explorerOpen || viewer) { place(); if (viewer) renderLines(); } });
  // Unsaved buffers are not lost to a closed window without a word.
  window.addEventListener('beforeunload', (ev) => {
    if (drafts.anyDirty() || editDirty()) { ev.preventDefault(); ev.returnValue = ''; }
  });
  // Focus follows the pointer: the tree, the viewer, or (anywhere else) the app.
  document.addEventListener('mousedown', (ev) => {
    if (!ui) return;
    const t = ev.target as HTMLElement | null;
    if (!t || ui.picker.contains(t) || t.closest('dialog, .toast, .terminal-overlay')) return;
    // The picker is modal like the TUI's telescope: a click elsewhere closes it.
    if (picker) closePicker();
    if (!explorerOpen && !viewer) return;
    if (ui.drawer.contains(t)) { if (focus !== 'tree') setFocus('tree', false); return; }
    if (ui.view.contains(t)) { if (focus !== 'viewer') setFocus('viewer', !ui.cmdline.contains(t)); return; }
    if (focus !== 'none') setFocus('none', false);
  }, true);
  // Capture phase: the tree and the viewer take their keys before the global key engine.
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing || picker || (!explorerOpen && !viewer) || focus === 'none' || ops?.isOpen()) return;
    if (!paneKeyAllowed(isVimKeysOn(), ev)) return;
    if (document.querySelector('dialog[open]')) return;
    const active = document.activeElement as HTMLElement | null;
    if (active && (ui?.cmdline.contains(active) || active.closest('.terminal-overlay'))) return;
    if (isTypingTarget(active)) return;
    const handled = focus === 'tree' && explorerOpen ? runTreeKey(ev) : focus === 'viewer' && viewer ? runViewerKey(ev) : false;
    if (!handled) return;
    ev.preventDefault();
    ev.stopImmediatePropagation();
    uiChanged();
  }, true);
}
