// The file explorer's add / mkdir / rename / move / copy / delete-to-trash /
// undo (#113, hub #3012), DOM side. The rules are files-ops-model.ts (pure,
// tested); every change is one rsid call through files-api.ts and rsid owns the
// confinement, the never-overwrite rule, the trash and the undo log. Entry
// points: the explorer keys (a A r m c d u), a toolbar under the drawer title,
// a right-click menu on a row and the View menu / palette actions (files.ts
// runs those through `start` and `undo`). Names and messages are set with
// textContent only.
import { el, toast } from './dom.js';
import { filesApi } from './files-api.js';
import {
  type DirEntry, type FileRoot, type OperationResult, type UndoResult, type TreeOp, writeErrorText,
} from './files-model.js';
import {
  type OpPrompt, type OpTarget, type PromptOp, completeInput, confirmKey, createdText, intoDirectory, isConfirmation, opMenu,
  planOp, promptLabel, resolveDestination, splitInput, startOp, touched, transferredText, trashedText, undoText,
} from './files-ops-model.js';

/** What the explorer (files.ts) lends the operations. */
export interface OpsHost {
  /** The tree's root, or null when no explorer is open. */
  root(): { root: FileRoot; abs: string | null; name: string } | null;
  /** The selected entry, or null. */
  selection(): DirEntry | null;
  /** Whether hidden names complete. */
  showHidden(): boolean;
  /** The loaded entries of a folder (root-relative, "" the root), or null when it is not loaded. */
  listed(dir: string): DirEntry[] | null;
  /** A folder's entries from rsid (a completion into a folder the tree has not opened), or null. */
  list(dir: string): Promise<DirEntry[] | null>;
  /** Is `path` an existing folder? Asked of rsid, as the TUI asks the filesystem (`mv a dir/`). */
  isDir(path: string): Promise<boolean>;
  /** After rsid changed files: reload these folders, select `select`, and retarget open viewers / drafts. */
  changed(c: { dirs: string[]; select: string | null; moved: { from: string; to: string } | null }): void;
  /** Give the keyboard back to the tree. */
  focusTree(): void;
}

export interface Ops {
  /** The toolbar under the drawer title. */
  toolbar: HTMLElement;
  /** The prompt row under the tree. */
  bar: HTMLElement;
  /** Run an explorer operation (a key, a button, a menu item or a palette action). */
  run(op: TreeOp, entry?: DirEntry | null): void;
  /** A prompt (or the delete confirmation) is open and owns the keys. */
  isOpen(): boolean;
  /** Cancel the open prompt. */
  cancel(): void;
  /** Right-click menu at the pointer over `entry` (null: the empty tree). */
  menu(x: number, y: number, entry: DirEntry | null): void;
  /** Close the right-click menu. */
  closeMenu(): void;
}

const nameOf = (p: string): string => p.slice(p.lastIndexOf('/') + 1);
const asTarget = (e: DirEntry | null): OpTarget | null => (e ? { path: e.path, name: e.name, isDir: e.kind === 'dir' } : null);

const TOOLBAR: ReadonlyArray<{ op: TreeOp; label: string; title: string }> = [
  { op: 'add', label: '+ File', title: 'New file (a). End the name with / for a folder' },
  { op: 'mkdir', label: '+ Folder', title: 'New folder (A)' },
  { op: 'rename', label: 'Rename', title: 'Rename the selected entry (r)' },
  { op: 'move', label: 'Move', title: 'Move the selected entry (m)' },
  { op: 'copy', label: 'Copy', title: 'Copy the selected entry (c)' },
  { op: 'delete', label: 'Delete', title: 'Move the selected entry to the trash (d); u undoes it' },
  { op: 'undo', label: 'Undo', title: 'Undo the last change (u)' },
];

export function createOps(host: OpsHost): Ops {
  let prompt: OpPrompt | null = null;
  let busy = false;
  let menuEl: HTMLElement | null = null;

  // ---------- toolbar ----------
  const toolbar = el('div', 'files-ops');
  toolbar.setAttribute('role', 'toolbar');
  toolbar.setAttribute('aria-label', 'File operations');
  for (const t of TOOLBAR) {
    const b = el('button', 'btn files-op', t.label);
    b.type = 'button';
    b.title = t.title;
    b.dataset.op = t.op;
    // The tree keeps focus: a click must not steal it (the prompt takes it itself).
    b.addEventListener('mousedown', (ev) => ev.preventDefault());
    b.addEventListener('click', () => run(t.op));
    toolbar.append(b);
  }

  // ---------- prompt ----------
  const bar = el('form', 'files-prompt');
  bar.hidden = true;
  bar.tabIndex = -1;
  const label = el('div', 'files-prompt-label');
  const input = el('input', 'files-prompt-input');
  input.type = 'text';
  input.spellcheck = false;
  input.autocomplete = 'off';
  input.setAttribute('aria-label', 'File operation');
  const hint = el('div', 'files-prompt-hint');
  const yes = el('button', 'btn danger', 'Delete');
  yes.type = 'button';
  const no = el('button', 'btn', 'Cancel');
  no.type = 'button';
  const buttons = el('div', 'files-prompt-buttons');
  buttons.append(yes, no);
  bar.append(label, input, hint, buttons);

  const rootInfo = () => host.root();

  function show(p: OpPrompt): void {
    prompt = p;
    const confirm = isConfirmation(p);
    bar.hidden = false;
    bar.classList.toggle('confirm', confirm);
    label.textContent = promptLabel(p);
    input.hidden = confirm;
    buttons.hidden = !confirm;
    hint.textContent = confirm
      ? 'Enter or y moves it to the trash (u undoes it); Esc or n cancels'
      : p.op === 'add' ? 'Enter creates it, Tab completes, Esc cancels; a name ending with / makes a folder'
        : 'Enter applies it, Tab completes, Esc cancels';
    if (confirm) bar.focus({ preventScroll: true });
    else {
      input.value = p.input;
      input.focus({ preventScroll: true });
      input.setSelectionRange(p.cursor, p.cursor);
    }
  }

  function close(): void {
    if (!prompt) return;
    prompt = null;
    bar.hidden = true;
    input.value = '';
    host.focusTree();
  }

  function run(op: TreeOp, entry?: DirEntry | null): void {
    closeMenu();
    if (busy) return;
    const r = rootInfo();
    if (!r) { toast('Open the file explorer first (<Space>e)'); return; }
    if (op === 'undo') { void undo(); return; }
    const at = entry === undefined ? host.selection() : entry;
    // Add goes in the selected folder itself, or the selected file's parent, else the root.
    const dir = at ? (at.kind === 'dir' ? at.path : at.path.includes('/') ? at.path.slice(0, at.path.lastIndexOf('/')) : '') : '';
    const p = startOp(op as PromptOp, asTarget(at), dir);
    if (typeof p === 'string') { toast(p); return; }
    show(p);
  }

  function cancel(): void { close(); }

  async function submit(): Promise<void> {
    const p = prompt;
    const r = rootInfo();
    if (!p || !r || busy) return;
    if (isConfirmation(p)) { await trash(p, r); return; }
    p.input = input.value;
    const plan = planOp(p, r.name, r.abs);
    if (plan.kind === 'error') { toast(plan.error); return; }
    if (plan.kind === 'noop') { close(); return; }
    busy = true;
    try {
      if (plan.kind === 'create') {
        const res = await filesApi.createEntry(r.root, plan.path, plan.as);
        done(res, createdText(plan.path, plan.as), plan.path);
      } else if (plan.kind === 'rename') {
        const res = await filesApi.moveEntry(r.root, plan.from, plan.to);
        done(res, transferredText('moved', plan.from, plan.to), plan.to);
      } else {
        const into = await host.isDir(plan.to);
        const to = intoDirectory(plan.from, plan.to, into);
        if (to === plan.from) { close(); return; }
        const res = plan.kind === 'move' ? await filesApi.moveEntry(r.root, plan.from, to) : await filesApi.copyEntry(r.root, plan.from, to);
        done(res, transferredText(plan.kind === 'move' ? 'moved' : 'copied', plan.from, to), to);
      }
    } catch (e) {
      // The prompt stays open so the input can be corrected.
      toast(writeErrorText(e, { name: nameOf(plan.kind === 'create' ? plan.path : plan.to), rootName: r.name }));
    } finally {
      busy = false;
    }
  }

  function done(res: OperationResult, message: string, select: string | null): void {
    close();
    toast(message, true);
    const t = touched(res);
    host.changed({ dirs: t.dirs, select, moved: t.moved });
  }

  async function trash(p: OpPrompt, r: NonNullable<ReturnType<typeof rootInfo>>): Promise<void> {
    if (!p.target) { close(); return; }
    busy = true;
    try {
      const res = await filesApi.trashEntry(r.root, p.target.path);
      close();
      toast(trashedText(p.target.path), true);
      host.changed({ dirs: touched(res).dirs, select: null, moved: null });
    } catch (e) {
      close();
      toast(`Delete failed: ${writeErrorText(e, { name: p.target.name, rootName: r.name })}`);
    } finally {
      busy = false;
    }
  }

  async function undo(): Promise<void> {
    const r = rootInfo();
    if (!r || busy) return;
    busy = true;
    try {
      const res: UndoResult = await filesApi.undo(r.root);
      const u = undoText(res);
      toast(u.message, true);
      const t = touched(res);
      host.changed({ dirs: t.dirs, select: u.select, moved: t.moved });
    } catch (e) {
      toast(writeErrorText(e, { rootName: r.name }));
    } finally {
      busy = false;
    }
  }

  async function complete(): Promise<void> {
    const p = prompt;
    const r = rootInfo();
    if (!p || !r || isConfirmation(p)) return;
    const { dirPart } = splitInput(input.value);
    // The folder the typed part names: relative to the prompt's base, or absolute inside the root.
    const base = p.op === 'add' || p.op === 'mkdir' ? p.dir : p.op === 'rename' ? (p.target && p.target.path.includes('/') ? p.target.path.slice(0, p.target.path.lastIndexOf('/')) : '') : '';
    let dir: string;
    if (dirPart === '') dir = base;
    else {
      const res = resolveDestination(r.name, r.abs, base, dirPart);
      if (!res.ok) return;
      dir = res.path;
    }
    const entries = host.listed(dir) ?? (await host.list(dir));
    if (!entries || prompt !== p) return;
    const completed = completeInput(input.value, entries.map((e) => ({ name: e.name, isDir: e.kind === 'dir' })), host.showHidden());
    if (completed !== null) {
      input.value = completed;
      input.setSelectionRange(completed.length, completed.length);
    }
  }

  bar.addEventListener('submit', (ev) => { ev.preventDefault(); void submit(); });
  yes.addEventListener('click', () => void submit());
  no.addEventListener('click', cancel);
  // Keys stay in the prompt: neither the tree nor a global binding sees them.
  bar.addEventListener('keydown', (ev) => {
    if (ev.isComposing) return;
    const p = prompt;
    if (!p) return;
    if (isConfirmation(p)) {
      if ((ev.target as HTMLElement).closest('button') && (ev.key === 'Enter' || ev.key === ' ')) return;
      const k = confirmKey(ev.key, ev.ctrlKey || ev.metaKey);
      if (k) {
        ev.preventDefault();
        ev.stopPropagation();
        if (k === 'yes') void submit(); else cancel();
      }
      return;
    }
    if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); cancel(); return; }
    if (ev.key === 'Tab') { ev.preventDefault(); ev.stopPropagation(); void complete(); return; }
    if (ev.ctrlKey && !ev.altKey && !ev.metaKey) {
      const at = input.selectionStart ?? 0;
      switch (ev.key.toLowerCase()) {
        case 'a': input.setSelectionRange(0, 0); break;
        case 'e': input.setSelectionRange(input.value.length, input.value.length); break;
        case 'b': input.setSelectionRange(Math.max(0, at - 1), Math.max(0, at - 1)); break;
        case 'f': input.setSelectionRange(Math.min(input.value.length, at + 1), Math.min(input.value.length, at + 1)); break;
        case 'h': input.setRangeText('', Math.max(0, at - 1), at, 'end'); break;
        case 'u': input.setRangeText('', 0, at, 'end'); break;
        case 'w': {
          const head = input.value.slice(0, at);
          const cut = head.replace(/\s+$/, '').replace(/[^\s/]+$|\/+$/, '');
          input.setRangeText('', cut.length, at, 'end');
          break;
        }
        default: return;
      }
      ev.preventDefault();
    }
    ev.stopPropagation();
  });
  // ---------- right-click menu ----------
  function closeMenu(): void {
    menuEl?.remove();
    menuEl = null;
  }

  function menu(x: number, y: number, entry: DirEntry | null): void {
    closeMenu();
    const target = asTarget(entry);
    const m = el('ul', 'files-menu');
    m.setAttribute('role', 'menu');
    for (const item of opMenu(target)) {
      const li = el('li', 'files-menu-item');
      li.setAttribute('role', 'menuitem');
      li.tabIndex = 0;
      li.append(el('span', 'files-menu-label', item.label), el('span', 'files-menu-key', item.key));
      const pick = () => { closeMenu(); run(item.op, entry); };
      li.addEventListener('click', pick);
      li.addEventListener('keydown', (ev) => {
        if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); ev.stopPropagation(); pick(); }
        if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); closeMenu(); host.focusTree(); }
        if (ev.key === 'ArrowDown') { ev.preventDefault(); (li.nextElementSibling as HTMLElement | null)?.focus(); }
        if (ev.key === 'ArrowUp') { ev.preventDefault(); (li.previousElementSibling as HTMLElement | null)?.focus(); }
      });
      m.append(li);
    }
    m.style.left = `${x}px`;
    m.style.top = `${y}px`;
    document.body.append(m);
    menuEl = m;
    const r = m.getBoundingClientRect();
    if (r.right > window.innerWidth) m.style.left = `${Math.max(0, window.innerWidth - r.width - 4)}px`;
    if (r.bottom > window.innerHeight) m.style.top = `${Math.max(0, window.innerHeight - r.height - 4)}px`;
    (m.firstElementChild as HTMLElement | null)?.focus();
    const away = (ev: Event) => {
      if (menuEl && !menuEl.contains(ev.target as Node)) { closeMenu(); document.removeEventListener('mousedown', away, true); }
    };
    document.addEventListener('mousedown', away, true);
  }

  return { toolbar, bar, run, isOpen: () => prompt !== null, cancel, menu, closeMenu };
}
