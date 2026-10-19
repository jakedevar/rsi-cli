// The project-files slice's entries for the action registry (slice 10B, hub
// #3012), pure (no DOM): the file explorer (`<Space>e`), the fuzzy file picker
// (`<Space><Space>`) and the read-only file viewer. The registry collects SLICE
// through the generated actions/index.generated.ts; the DOM side is files.ts
// (RUNTIME in actions/files-run.ts), the pure rules files-model.ts. Keys inside
// the drawer, the picker and the viewer are the TUI's overlay keys, resolved
// by files-model.ts (treeKey, pickerKey, viewerKey).
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type FilesActionId =
  | 'files-explorer' | 'files-find' | 'files-close-viewer'
  | 'files-toggle-hidden' | 'files-follow-preview' | 'files-refresh' | 'files-collapse-all'
  | 'files-copy-path' | 'files-copy-name' | 'files-copy-relative' | 'files-info'
  | 'files-edit' | 'files-save' | 'files-save-force' | 'files-reload'
  | 'files-add' | 'files-mkdir' | 'files-rename' | 'files-move' | 'files-copy' | 'files-delete' | 'files-undo';

/** What the file actions can see (built by files.ts). */
export interface FilesSnapshot {
  /** A root resolves (an open session, or a list filtered to a project with a folder). */
  hasRoot: boolean;
  /** Why not, when it does not. */
  noRoot?: string;
  explorer: boolean;
  viewer: boolean;
  picker: boolean;
  showHidden: boolean;
  followPreview: boolean;
  /** The explorer selection or the viewer's file: what the copy actions copy. */
  target: boolean;
  /** The viewer's file is being edited. */
  editing: boolean;
  /** The edited buffer has unsaved changes. */
  dirty: boolean;
  /** The explorer is open with an entry selected: what rename, move, copy and delete act on. */
  selection: boolean;
}

type Rule = (s: UiSnapshot) => string | null;

const NEED_EXPLORER = 'Open the file explorer first (<Space>e)';
const root: Rule = (s) => (s.files?.hasRoot ? null : (s.files?.noRoot ?? 'No project folder to show'));
const explorer: Rule = (s) => (s.files?.explorer ? null : NEED_EXPLORER);
const target: Rule = (s) => (s.files?.target ? null : 'Select a file in the explorer or open one in the viewer first');

const viewing: Rule = (s) => (s.files?.viewer ? null : 'No file is open in the viewer');
const editing: Rule = (s) => (s.files?.editing ? null : 'The file is not being edited (press i in the viewer, or Edit)');
const selection: Rule = (s) => explorer(s) ?? (s.files?.selection ? null : 'Select a file or folder in the explorer first');

export const FILES_RULES: Readonly<Record<FilesActionId, Rule>> = {
  'files-explorer': (s) => (s.files?.explorer ? null : root(s)),
  'files-find': root,
  'files-close-viewer': (s) => (s.files?.viewer ? null : 'No file is open in the viewer'),
  'files-toggle-hidden': explorer,
  'files-follow-preview': explorer,
  'files-refresh': explorer,
  'files-collapse-all': explorer,
  'files-copy-path': target,
  'files-copy-name': target,
  'files-copy-relative': target,
  'files-info': (s) => explorer(s) ?? target(s),
  'files-edit': (s) => viewing(s) ?? (s.files?.editing ? 'The file is already being edited' : null),
  'files-save': editing,
  'files-save-force': editing,
  'files-reload': viewing,
  'files-add': explorer,
  'files-mkdir': explorer,
  'files-rename': selection,
  'files-move': selection,
  'files-copy': selection,
  'files-delete': selection,
  'files-undo': explorer,
};

/**
 * Menu order (View menu, beside the terminal: File keeps Quit last); the
 * palette lists them all. `extraKeys` are keys inside the drawer or viewer
 * that no global binding uses; the ones that clash with a global key (`H`,
 * `yy`, `i`, `P`, `R`, `<Space>q`) are named in `detail` instead, so the palette never shows a key
 * on two rows.
 */
export const FILES_ACTIONS: readonly ActionSpec[] = [
  {
    id: 'files-explorer', title: 'File explorer', group: 'view', menuBreak: true,
    detail: 'The project’s file tree; Enter opens a file in the read-only viewer (Esc or q closes)',
    checked: (s) => s.files?.explorer ?? false,
  },
  { id: 'files-find', title: 'Find file…', group: 'view', detail: 'Fuzzy file picker over the project (git ls-files: tracked plus untracked, not ignored)' },
  { id: 'files-close-viewer', title: 'Close file viewer', group: 'view', extraKeys: ['q', ':q'], detail: 'In the viewer: q, <Space>q or :q (:q! discards unsaved changes)' },
  { id: 'files-edit', title: 'Edit file', group: 'view', detail: 'In the viewer: i a o A I O start the editor (Vim keys); then :w saves, :wq saves and closes, :w! overwrites a file that changed on disk, :e! reloads it, Esc leaves a clean buffer' },
  { id: 'files-save', title: 'Save file', group: 'view', extraKeys: ['Ctrl-S', ':w'], detail: 'Refused when the file changed on disk since it was opened (:w! overwrites, :e! reloads)' },
  { id: 'files-save-force', title: 'Save file, overwriting the disk copy', group: 'view', noMenu: true, extraKeys: [':w!'], detail: 'Saves even though the file changed on disk' },
  { id: 'files-reload', title: 'Reload file from disk', group: 'view', noMenu: true, extraKeys: [':e!'], detail: 'Discards the unsaved buffer and reads the file again' },
  { id: 'files-add', title: 'New file…', group: 'view', menuBreak: true, detail: 'In the tree: a. In the selected folder (or beside the selected file); end the name with / for a folder' },
  { id: 'files-mkdir', title: 'New folder…', group: 'view', detail: 'In the tree: A' },
  { id: 'files-rename', title: 'Rename…', group: 'view', noMenu: true, detail: 'In the tree: r (or right-click). Never replaces an existing name' },
  { id: 'files-move', title: 'Move…', group: 'view', noMenu: true, detail: 'In the tree: m. A folder as the destination receives the entry' },
  { id: 'files-copy', title: 'Copy…', group: 'view', noMenu: true, detail: 'In the tree: c' },
  { id: 'files-delete', title: 'Delete (to trash)…', group: 'view', detail: 'In the tree: d. Asks first; moves the entry to rsid’s trash and u undoes it' },
  { id: 'files-undo', title: 'Undo last file change', group: 'view', detail: 'In the tree: u. Undoes the last add, rename, move, copy or delete of this project' },
  { id: 'files-toggle-hidden', title: 'Show hidden files', group: 'view', extraKeys: ['.'], detail: 'In the tree: . or H', checked: (s) => s.files?.showHidden ?? false },
  { id: 'files-follow-preview', title: 'Follow preview', group: 'view', detail: 'In the tree: P. Show each file in the viewer as the tree selection moves', checked: (s) => s.files?.followPreview ?? false },
  { id: 'files-refresh', title: 'Refresh file tree', group: 'view', detail: 'In the tree: R. Re-read the open folders (expansion and selection kept)' },
  { id: 'files-collapse-all', title: 'Collapse all folders', group: 'view', extraKeys: ['z'] },
  { id: 'files-copy-path', title: 'Copy absolute path', group: 'view', noMenu: true, detail: 'In the tree: yy' },
  { id: 'files-copy-name', title: 'Copy file name', group: 'view', noMenu: true, extraKeys: ['yn'] },
  { id: 'files-copy-relative', title: 'Copy project-relative path', group: 'view', noMenu: true, extraKeys: ['yr'] },
  { id: 'files-info', title: 'File info', group: 'view', noMenu: true, detail: 'In the tree: i. Size, modified time and git flags of the selected entry' },
];

const IDS: ReadonlySet<string> = new Set(FILES_ACTIONS.map((a) => a.id));
export const isFilesAction = (id: string): id is FilesActionId => IDS.has(id);

/** TUI Normal-mode keys the files slice runs (the overlays' own keys are in files-model.ts). */
export const FILES_KEY_BINDINGS: Readonly<Record<string, DesktopBinding<FilesActionId>>> = {
  '<Space>e': { action: 'files-explorer' },
  '<Space><Space>': { action: 'files-find' },
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The file explorer, picker and viewer (slice 10B, files.ts). */
    files?: FilesSnapshot;
  }
}

export const SLICE: SliceSpec<FilesActionId> = {
  name: 'files (slice 10B, hub #3012)',
  order: 55,
  actions: FILES_ACTIONS,
  rules: FILES_RULES,
  keys: FILES_KEY_BINDINGS,
};
