// DOM helpers shared by the admin overlays (slice 13a). Daemon text is set
// with textContent only. Secret fields are password inputs that are cleared
// after every submit or cancel; their value never leaves the input except as
// the argument of one Tauri command.
import { el } from './dom.js';
import { button, dialogEl, errorLine, focusDialog, showError } from './actions/detail.js';

export { button, errorLine, focusDialog, showError };

/** Which admin overlay is open (for the slice snapshot). */
let openName: string | null = null;
export function adminOpen(): string | null { return openName; }

/** A fresh admin dialog; `name` is reported in the snapshot until it closes. */
export function adminDialog(id: string, name: string): HTMLDialogElement {
  const dlg = dialogEl(id);
  dlg.classList.add('admin-dialog');
  openName = name;
  dlg.onclose = () => { if (openName === name) openName = null; };
  return dlg;
}

export function note(text = ''): HTMLElement {
  return el('div', 'dialog-note', text);
}

export function listBox(label: string): HTMLUListElement {
  const ul = el('ul', 'pick-list admin-list');
  ul.setAttribute('role', 'listbox');
  ul.setAttribute('aria-label', label);
  return ul;
}

/** One selectable row: a label and a value column. */
export function rowEl(label: string, value: string, selected: boolean, cls = ''): HTMLLIElement {
  const li = el('li', `pick-item admin-row${selected ? ' sel' : ''}${cls ? ` ${cls}` : ''}`);
  li.setAttribute('role', 'option');
  li.setAttribute('aria-selected', String(selected));
  li.append(el('span', 'admin-label', label), el('span', 'admin-value', value));
  return li;
}

export function textField(label: string, value: string, opts: { placeholder?: string; readOnly?: boolean } = {}): { wrap: HTMLLabelElement; input: HTMLInputElement } {
  const wrap = el('label', undefined, label);
  const input = el('input');
  input.type = 'text';
  if (!opts.readOnly) input.dataset.vim = 'on';
  input.value = value;
  input.spellcheck = false;
  input.autocomplete = 'off';
  if (opts.placeholder) input.placeholder = opts.placeholder;
  if (opts.readOnly) input.readOnly = true;
  wrap.append(input);
  return { wrap, input };
}

/** A write-only credential field: masked, never autofilled, never spell-checked. */
export function secretField(label: string): { wrap: HTMLLabelElement; input: HTMLInputElement } {
  const { wrap, input } = textField(label, '');
  input.type = 'password';
  input.dataset.vim = 'off';
  input.autocomplete = 'new-password';
  input.setAttribute('data-secret', 'true');
  return { wrap, input };
}

export function checkField(label: string, checked: boolean): { wrap: HTMLLabelElement; input: HTMLInputElement } {
  const wrap = el('label', 'admin-check');
  const input = el('input');
  input.type = 'checkbox';
  input.checked = checked;
  wrap.append(input, document.createTextNode(` ${label}`));
  return { wrap, input };
}

export function actions(...buttons: HTMLButtonElement[]): HTMLElement {
  const a = el('div', 'actions');
  a.append(...buttons);
  return a;
}

/** Show a dialog built from `box` and give it the keyboard. */
export function show(dlg: HTMLDialogElement, box: HTMLElement): void {
  dlg.replaceChildren(box);
  if (!dlg.open) dlg.showModal();
  focusDialog(dlg);
}

/** A list-key handler should ignore keys typed into a field of the dialog. */
export function inField(ev: KeyboardEvent): boolean {
  const t = ev.target as HTMLElement | null;
  return !!t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
}

export const ADMIN_CSS = `
.admin-dialog { min-width: min(820px, 92vw); }
.admin-list { max-height: 52vh; }
.admin-row .admin-label { white-space: nowrap; font-weight: 600; }
.admin-row .admin-value { color: var(--subtext); font-family: var(--mono, monospace); font-size: 12px; text-align: right; overflow-wrap: anywhere; }
.admin-row.admin-off .admin-label { color: var(--overlay1, var(--subtext)); }
.admin-row.admin-header { cursor: default; color: var(--subtext); font-size: 12px; box-shadow: none; background: none; }
.admin-tabs { display: flex; gap: 6px; }
.admin-tabs .btn.on { border-color: var(--blue); color: var(--blue); }
.admin-check { flex-direction: row !important; align-items: center; gap: 6px; }
.admin-phrase { font-family: var(--mono, monospace); user-select: all; background: var(--surface0); padding: 6px 8px; border-radius: 4px; overflow-wrap: anywhere; }
.admin-grid { display: grid; grid-template-columns: max-content 1fr; gap: 4px 14px; font-size: 13px; }
.admin-grid .k { color: var(--subtext); }
`;
