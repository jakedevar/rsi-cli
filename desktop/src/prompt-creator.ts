// The prompt creator (<Space>gP; local #88, hub #3040): the TUI's
// Prompt Creator / Editor view (crates/rsi/src/prompt_creator_keys.rs and
// ui/prompt_creator.rs after the #3040 move). The prompts are the daemon's
// `~/.rsi/prompts/*.md`, reached only through the prompt-file Tauri commands
// (src-tauri/src/prompt_files.rs); `d` moves a prompt to the daemon's
// recoverable trash and names the trash path. Rules in prompt-creator-model.ts.
//
// List mode: j/k move, Enter opens the editor, Ctrl+N creates a prompt, d
// deletes, m toggles the model picker, q / Esc close. Editor mode: the prompt
// is a text area (a vim field when Vim keys are on); Ctrl+S saves, Enter
// always inserts a line break, and q (normal mode) or Esc closes the editor
// and returns to the list.
import { invoke } from './api.js';
import { el, toast } from './dom.js';
import { button, errorLine, showError } from './admin-dom.js';
import { overlayKey } from './actions/detail.js';
import { modelIds, parseLaunchDefault } from './nav-model.js';
import { openPicker } from './pickers.js';
import {
  NEW_PROMPT_CONTENT, type PromptEntry, formatSize, movePromptCursor, newPromptName, promptCursorAfter, promptListKey,
  relativeTime,
} from './prompt-creator-model.js';
import { isVimKeysOn } from './vimkeys.js';
import { modeOf } from './vim-field.js';
import { focusOwnsKey } from './modal-gate.js';

interface Creator {
  dir: string;
  files: PromptEntry[];
  truncated: boolean;
  at: number;
  /** The prompt open in the editor, if any. */
  editing: string | null;
  /** The editor text differs from what was loaded or last saved. */
  dirty: boolean;
  /** A close of a dirty editor was asked once (the second one discards). */
  discardArmed: boolean;
  /** The model badge (the TUI's Toggle Model picker; display only, as in the TUI). */
  model: string | null;
}

const DIALOG_ID = 'prompt-creator-dialog';
const st: Creator = { dir: '', files: [], truncated: false, at: 0, editing: null, dirty: false, discardArmed: false, model: null };

let dlg: HTMLDialogElement | null = null;
let ui: {
  title: HTMLElement; list: HTMLUListElement; empty: HTMLElement; editorBox: HTMLElement; editorTitle: HTMLElement;
  editor: HTMLTextAreaElement; err: HTMLElement;
} | null = null;

export function isPromptCreatorOpen(): boolean {
  return !!dlg?.open;
}

async function loadList(select: string | null = null): Promise<boolean> {
  try {
    const list = await invoke<{ dir?: string; files: PromptEntry[]; truncated?: boolean }>('list_prompt_files');
    st.dir = list.dir ?? '';
    st.files = list.files ?? [];
    st.truncated = !!list.truncated;
    st.at = promptCursorAfter(st.files, select, st.at);
    return true;
  } catch (e) {
    toast(`Failed to list prompts: ${String(e)}`);
    return false;
  }
}

function draw(): void {
  if (!ui) return;
  const now = Date.now();
  ui.title.replaceChildren(el('span', 'pc-title', 'Prompts'), ...(st.model ? [el('span', 'pc-model', st.model)] : []),
    el('span', 'pc-dir', st.dir));
  ui.empty.hidden = st.files.length > 0;
  ui.list.replaceChildren(...st.files.map((f, i) => {
    const li = el('li', `pick-item pc-card${i === st.at ? ' sel' : ''}${f.name === st.editing ? ' open' : ''}`);
    li.setAttribute('role', 'option');
    li.setAttribute('aria-selected', String(i === st.at));
    li.append(el('div', 'pc-name', f.name), el('div', 'pc-first', f.first_line || '(empty)'),
      el('div', 'pc-meta', [relativeTime(f.mtime, now), formatSize(f.size)].filter(Boolean).join(' · ')));
    li.addEventListener('click', () => { st.at = i; draw(); });
    li.addEventListener('dblclick', () => { st.at = i; void openEditor(); });
    return li;
  }));
  if (st.truncated) ui.list.append(el('li', 'pick-empty', 'The listing was truncated by the daemon.'));
  ui.list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  ui.editorBox.hidden = st.editing === null;
  ui.editorTitle.textContent = st.editing ? `${st.editing}${st.dirty ? ' ●' : ''}` : '';
  dlg?.classList.toggle('pc-editing', st.editing !== null);
}

function build(): HTMLDialogElement {
  // Its own dialog (not dialogEl): a stray backdrop click must not drop an unsaved prompt.
  let d = document.getElementById(DIALOG_ID) as HTMLDialogElement | null;
  if (!d) {
    d = document.createElement('dialog');
    d.id = DIALOG_ID;
    d.className = 'detail-dialog prompt-creator';
    document.body.append(d);
  }
  const title = el('div', 'pc-head');
  const list = el('ul', 'pick-list pc-list');
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', 'Prompts');
  const empty = el('div', 'dialog-note', 'No prompts yet. Ctrl+N creates one.');
  const err = errorLine();
  const newBtn = button('New (Ctrl+N)');
  const delBtn = button('Delete to trash (d)', 'btn danger');
  const modelBtn = button('Model (m)');
  const closeBtn = button('Close (q)');
  const left = el('div', 'pc-left');
  left.append(list, empty);
  const editorTitle = el('div', 'pc-editor-title');
  const editor = el('textarea', 'pc-editor');
  editor.spellcheck = false;
  editor.setAttribute('aria-label', 'Prompt text');
  const saveBtn = button('Save (Ctrl+S)', 'btn primary');
  const doneBtn = button('Close editor (Esc / q)');
  const editorBox = el('div', 'pc-right');
  const editorActions = el('div', 'actions');
  editorActions.append(doneBtn, saveBtn);
  editorBox.append(editorTitle, editor, editorActions);
  const body = el('div', 'pc-body');
  body.append(left, editorBox);
  const bar = el('div', 'actions');
  bar.append(closeBtn, modelBtn, delBtn, newBtn);
  d.replaceChildren(title, body, err, bar,
    el('div', 'dialog-note', "j/k move, Enter opens, Ctrl+N new, d moves to the daemon's trash (~/.rsi/trash/prompts), m model, q/Esc close · in the editor: Ctrl+S saves, Esc or q (normal mode) returns to the list."));
  ui = { title, list, empty, editorBox, editorTitle, editor, err };
  editor.addEventListener('input', () => { if (!st.dirty) { st.dirty = true; draw(); } st.discardArmed = false; });
  newBtn.addEventListener('click', () => void createPrompt());
  delBtn.addEventListener('click', () => void deletePrompt());
  modelBtn.addEventListener('click', () => void toggleModel());
  closeBtn.addEventListener('click', () => close());
  saveBtn.addEventListener('click', () => void save());
  doneBtn.addEventListener('click', () => closeEditor());
  d.onkeydown = onKey;
  d.oncancel = (ev) => ev.preventDefault();
  d.onclose = () => { st.editing = null; st.dirty = false; st.discardArmed = false; };
  return d;
}

function onKey(ev: KeyboardEvent): void {
  if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
  if (!dlg || !ui) return;
  const inEditor = ev.target === ui.editor;
  const plain = !ev.altKey && !ev.ctrlKey && !ev.metaKey;
  if (inEditor) {
    // Ctrl+S is taken in the capture listener; Esc here is from normal mode
    // (or Vim keys off): the TUI closes the editor.
    if (ev.key === 'Escape') { ev.preventDefault(); closeEditor(); }
    return;
  }
  if ((ev.target as HTMLElement | null)?.closest('button') && ev.key === 'Enter') return;
  if (plain && overlayKey(ev, dlg)) return;
  // Physical Ctrl only: Cmd-N (Ctrl-N off macOS) is also the New session menu accelerator.
  const k = promptListKey(ev.key, ev.ctrlKey && !ev.metaKey && !ev.altKey);
  if (!k) { if (ev.key === 'Escape') ev.preventDefault(); return; }
  ev.preventDefault();
  switch (k) {
    case 'down': case 'up': st.at = movePromptCursor(st.at, st.files.length, k === 'down' ? 1 : -1); draw(); break;
    case 'open': void openEditor(); break;
    case 'new': void createPrompt(); break;
    case 'delete': void deletePrompt(); break;
    case 'model': void toggleModel(); break;
    case 'close': close(); break;
  }
}

/** Ctrl+S anywhere in the creator, and q in a normal-mode editor, before the app's own vim keys. */
function captureKey(ev: KeyboardEvent): void {
  if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
  if (!dlg?.open || !ui || ev.isComposing) return;
  const t = ev.target as HTMLElement | null;
  if (!t || !dlg.contains(t)) return;
  if ((ev.ctrlKey || ev.metaKey) && !ev.altKey && (ev.key === 's' || ev.key === 'S')) {
    ev.preventDefault();
    ev.stopImmediatePropagation();
    if (st.editing) void save();
    return;
  }
  if (t === ui.editor && ev.key === 'q' && !ev.ctrlKey && !ev.metaKey && !ev.altKey && isVimKeysOn() && modeOf(ui.editor) === 'normal') {
    ev.preventDefault();
    ev.stopImmediatePropagation();
    closeEditor();
  }
}

async function openEditor(): Promise<void> {
  const f = st.files[st.at];
  if (!f || !ui) return;
  if (st.editing && st.dirty && st.editing !== f.name && !st.discardArmed) {
    st.discardArmed = true;
    toast(`${st.editing} has unsaved changes: Ctrl+S saves, Enter again discards them`);
    return;
  }
  try {
    const file = await invoke<{ name: string; content: string }>('get_prompt_file', { name: f.name });
    ui.editor.value = file.content;
    st.editing = file.name;
    st.dirty = false;
    st.discardArmed = false;
    ui.err.hidden = true;
    draw();
    ui.editor.setSelectionRange(0, 0);
    ui.editor.focus();
  } catch (e) {
    showError(ui.err, `Failed to open prompt: ${String(e)}`);
  }
}

function closeEditor(): void {
  if (!ui || !st.editing) return;
  if (st.dirty && !st.discardArmed) {
    st.discardArmed = true;
    toast(`${st.editing} has unsaved changes: Ctrl+S saves, Esc / q again discards them`);
    return;
  }
  st.editing = null;
  st.dirty = false;
  st.discardArmed = false;
  ui.editor.value = '';
  dlg?.focus();
  void loadList(st.files[st.at]?.name ?? null).then(draw);
}

async function save(): Promise<void> {
  if (!ui || !st.editing) return;
  const name = st.editing;
  try {
    await invoke('put_prompt_file', { name, content: ui.editor.value, createOnly: false });
    st.dirty = false;
    st.discardArmed = false;
    ui.err.hidden = true;
    toast(`Saved: ${st.dir ? `${st.dir}/` : ''}${name}`, true);
    await loadList(name);
    draw();
  } catch (e) {
    showError(ui.err, `Save failed: ${String(e)}`);
  }
}

async function createPrompt(): Promise<void> {
  if (!ui) return;
  const name = newPromptName(Date.now());
  try {
    await invoke('put_prompt_file', { name, content: NEW_PROMPT_CONTENT, createOnly: true });
  } catch (e) {
    showError(ui.err, `Failed to create prompt: ${String(e)}`);
    return;
  }
  await loadList(name);
  st.editing = null;
  st.dirty = false;
  draw();
  await openEditor();
}

async function deletePrompt(): Promise<void> {
  const f = st.files[st.at];
  if (!f || !ui) return;
  try {
    const out = await invoke<{ trash_path?: string }>('delete_prompt_file', { name: f.name });
    toast(`Moved ${f.name} to trash: ${out.trash_path ?? '~/.rsi/trash/prompts'}`, true);
    if (st.editing === f.name) { st.editing = null; st.dirty = false; ui.editor.value = ''; }
    await loadList(null);
    draw();
  } catch (e) {
    showError(ui.err, `Failed to delete prompt: ${String(e)}`);
  }
}

let picker: HTMLDialogElement | null = null;

/** `m`: the model picker for the launch default's provider (the badge; display only, as in the TUI). */
async function toggleModel(): Promise<void> {
  if (picker?.open) { picker.close(); return; }
  let raw: string | null = null;
  try { raw = window.localStorage.getItem('rsi.desktop.launchDefault'); } catch { raw = null; }
  const provider = parseLaunchDefault(raw)?.provider ?? 'Claude';
  let models: Array<{ id: string; label: string }> = [];
  try { models = modelIds(await invoke<unknown>('discover_models', { provider })); } catch { models = []; }
  picker = openPicker({
    title: `Prompt creator model: ${provider}`,
    hint: 'Type to filter · Enter picks · Esc closes',
    filterable: true,
    emptyText: 'No models discovered for this provider',
    items: models.map((m) => ({ id: m.id, label: m.label, ...(m.label !== m.id ? { detail: m.id } : {}), current: st.model === m.id })),
    onPick: (it) => { st.model = it.id; draw(); },
  });
  picker.addEventListener('close', () => { picker = null; dlg?.focus(); }, { once: true });
  picker.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'm' && (ev.target as HTMLInputElement | null)?.value === '') { ev.preventDefault(); picker?.close(); }
  });
}

function close(): void {
  if (st.editing && st.dirty && !st.discardArmed) {
    st.discardArmed = true;
    toast(`${st.editing} has unsaved changes: Ctrl+S saves, q again discards them`);
    return;
  }
  dlg?.close();
}

/** `<Space>gP`: open the prompt creator, or close it when it is open. */
export async function togglePromptCreator(): Promise<void> {
  if (dlg?.open) { close(); return; }
  if (!(await loadList(null))) return;
  dlg = build();
  st.editing = null;
  st.dirty = false;
  st.discardArmed = false;
  draw();
  if (!dlg.open) dlg.showModal();
  dlg.tabIndex = -1;
  dlg.focus();
}

export function initPromptCreator(): void {
  // Capture on window: ahead of the app's vim-field handler (document capture),
  // which would close the whole dialog on q.
  window.addEventListener('keydown', captureKey, true);
}

export const PROMPT_CREATOR_CSS = `
dialog.prompt-creator { width: min(1200px, 94vw); height: min(820px, 88vh); display: flex; flex-direction: column; gap: 8px; }
dialog.prompt-creator:not([open]) { display: none; }
.pc-head { display: flex; gap: 12px; align-items: baseline; }
.pc-title { font-weight: 700; color: var(--mauve, var(--blue)); }
.pc-model { color: var(--blue); font-family: var(--mono, monospace); font-size: 12px; }
.pc-dir { color: var(--subtext); font-size: 12px; margin-left: auto; overflow-wrap: anywhere; }
.pc-body { display: flex; gap: 10px; flex: 1; min-height: 0; }
.pc-left { flex: 1; min-width: 0; overflow: auto; }
dialog.pc-editing .pc-left { flex: 0 0 34%; }
.pc-right { flex: 1; display: flex; flex-direction: column; gap: 6px; min-width: 0; }
.pc-right[hidden] { display: none; }
.pc-list { max-height: none; }
.pc-card { display: flex; flex-direction: column; align-items: stretch; gap: 2px; }
.pc-card.open .pc-name::after { content: ' ✎'; color: var(--peach, var(--blue)); }
.pc-name { font-weight: 600; }
.pc-first { color: var(--text); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.pc-meta { color: var(--subtext); font-size: 11px; }
.pc-editor-title { color: var(--subtext); font-family: var(--mono, monospace); font-size: 12px; }
.pc-editor { flex: 1; resize: none; font-family: var(--mono, monospace); font-size: 13px; min-height: 0; }
`;
