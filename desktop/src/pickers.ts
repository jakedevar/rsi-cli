// Generic DOM pieces for the slice-4 overlays: a filterable picker, a small
// form, and a read-only info panel. Each opens its own <dialog>, so index.html
// stays untouched. Daemon text is set with textContent only.
import { el, toast } from './dom.js';
import { type ColorChoice, filterByName } from './listmodel.js';
import { movePick } from './registry.js';
import { pickerLeader } from './overlay-keys.js';
import { focusOwnsKey } from './modal-gate.js';

export interface PickItem {
  id: string;
  label: string;
  detail?: string;
  /** Swatch color (#rrggbb). */
  color?: string;
  /** Shown with a check mark (the current choice). */
  current?: boolean;
}

export interface PickButton {
  text: string;
  /** Ctrl-<letter> shortcut shown in the hint and honored while the picker is focused. */
  ctrl?: string;
  danger?: boolean;
  /** Needs a highlighted real entry (not the "all" / "none" row with id ""); otherwise `missing` is shown and the picker stays open. */
  needsItem?: string;
  /** Runs after the picker closed; `item` is the highlighted row. */
  run: (item: PickItem | null) => void;
}

export interface PickerOpts {
  title: string;
  hint: string;
  items: readonly PickItem[];
  filterable?: boolean;
  emptyText?: string;
  buttons?: readonly PickButton[];
  /** Plain j / k / q keys when there is no filter box (the sort picker). */
  vimKeys?: boolean;
  onPick: (item: PickItem) => void | Promise<void>;
}

/** Run the picker leader's action once the picker has closed. */
export function runLeader(what: 'launch' | 'palette'): void {
  void import('./commands.js').then((m) => m.dispatch(what, { source: 'key' }));
}

function dialog(cls: string, label: string): HTMLDialogElement {
  const dlg = el('dialog', `pick ${cls}`);
  dlg.setAttribute('aria-label', label);
  dlg.addEventListener('close', () => dlg.remove());
  document.body.append(dlg);
  return dlg;
}

export function openPicker(o: PickerOpts): HTMLDialogElement {
  const dlg = dialog('pick-list-dialog', o.title);
  const input = el('input');
  input.type = 'text';
  input.placeholder = 'Type to filter…';
  input.autocomplete = 'off';
  input.spellcheck = false;
  input.hidden = !o.filterable;
  const list = el('ul', 'pick-list');
  list.setAttribute('role', 'listbox');
  const keys = o.buttons?.filter((b) => b.ctrl).map((b) => `Ctrl-${b.ctrl!.toUpperCase()} ${b.text.toLowerCase()}`) ?? [];
  const hint = el('div', 'pick-hint', [o.hint, ...keys].join(' · '));
  const foot = el('div', 'pick-actions');
  dlg.append(el('h2', undefined, o.title), input, list, foot, hint);

  let shown: PickItem[] = [...o.items];
  let pick = Math.max(0, shown.findIndex((i) => i.current));

  const render = (): void => {
    if (shown.length === 0) {
      list.replaceChildren(el('li', 'pick-empty', o.emptyText ?? 'Nothing matches'));
      return;
    }
    list.replaceChildren(...shown.map((item, i) => {
      const li = el('li', `pick-item${i === pick ? ' sel' : ''}`);
      li.dataset.index = String(i);
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(i === pick));
      const mark = el('span', 'pick-mark', item.current ? '✓' : '');
      li.append(mark);
      if (item.color) {
        const sw = el('span', 'swatch');
        sw.style.background = item.color;
        li.append(sw);
      }
      li.append(el('span', 'pick-label', item.label));
      if (item.detail) li.append(el('span', 'pick-detail', item.detail));
      return li;
    }));
    list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  };

  const choose = (item: PickItem | undefined): void => {
    if (!item) return;
    dlg.close();
    void o.onPick(item);
  };
  const runButton = (b: PickButton): void => {
    const item = shown[pick] ?? null;
    if (b.needsItem && (!item || item.id === '')) { toast(b.needsItem); return; }
    dlg.close();
    b.run(item);
  };

  for (const b of o.buttons ?? []) {
    const btn = el('button', `btn${b.danger ? ' danger' : ''}`, b.text);
    btn.type = 'button';
    btn.addEventListener('click', () => runButton(b));
    foot.append(btn);
  }
  const close = el('button', 'btn', 'Cancel');
  close.type = 'button';
  close.addEventListener('click', () => dlg.close());
  foot.append(close);

  const move = (delta: number): void => { pick = movePick(shown.length, pick, delta); render(); };
  // The TUI's picker leader (gap sweep G4): Space N closes and opens a blank
  // session prompt, Space ; the command palette. With a filter box, only
  // while the filter is empty (a leading space filters nothing).
  let leader = false;
  dlg.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    const ctrl = ev.ctrlKey && !ev.metaKey && !ev.altKey;
    const typing = !input.hidden;
    const l = pickerLeader(leader, ev, !typing || input.value === '');
    leader = l.armed;
    hint.classList.toggle('leader', leader);
    if (l.run) {
      ev.preventDefault();
      ev.stopPropagation();
      if (l.run !== 'arm') { dlg.close(); runLeader(l.run); }
      return;
    }
    if (ev.key === 'ArrowDown') { ev.preventDefault(); move(1); return; }
    if (ev.key === 'ArrowUp') { ev.preventDefault(); move(-1); return; }
    if (ev.key === 'Enter') { ev.preventDefault(); choose(shown[pick]); return; }
    if (ctrl) {
      const b = o.buttons?.find((x) => x.ctrl && x.ctrl.toLowerCase() === ev.key.toLowerCase());
      if (b) { ev.preventDefault(); runButton(b); }
      return;
    }
    if (!typing && o.vimKeys) {
      if (ev.key === 'j') { ev.preventDefault(); move(1); } else if (ev.key === 'k') { ev.preventDefault(); move(-1); } else if (ev.key === 'q') { ev.preventDefault(); dlg.close(); }
    }
  });
  input.addEventListener('input', () => {
    shown = filterByName(o.items, input.value, (i) => i.label);
    pick = shown.length > 0 ? 0 : -1;
    render();
  });
  list.addEventListener('click', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.pick-item');
    if (li) choose(shown[Number(li.dataset.index)]);
  });
  dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg.close(); });

  render();
  dlg.showModal();
  list.tabIndex = -1;
  (o.filterable ? input : list).focus();
  return dlg;
}

// ---------- form ----------

export interface FormField {
  name: string;
  label: string;
  value: string;
  /** `password` hides the typed text; `checkbox` reads as 'true' / 'false' (`value` 'true' = checked). */
  type: 'text' | 'password' | 'checkbox' | 'color';
  palette?: readonly ColorChoice[];
  placeholder?: string;
}

export interface FormOpts {
  title: string;
  submit: string;
  fields: readonly FormField[];
  /** Null when the values can be sent. */
  validate: (values: Record<string, string>) => string | null;
  /** Throw to keep the form open and show the message. */
  onSubmit: (values: Record<string, string>) => Promise<void>;
}

export function openForm(o: FormOpts): HTMLDialogElement {
  const dlg = dialog('pick-form-dialog', o.title);
  const form = el('form', 'launch-form');
  form.method = 'dialog';
  form.noValidate = true;
  form.append(el('h2', undefined, o.title));
  const read: Array<() => [string, string]> = [];
  for (const f of o.fields) {
    const label = el('label', undefined, f.label);
    if (f.type === 'checkbox') {
      const box = el('input');
      box.type = 'checkbox';
      box.name = f.name;
      box.checked = f.value === 'true';
      label.classList.add('check');
      label.append(box);
      read.push(() => [f.name, box.checked ? 'true' : 'false']);
    } else if (f.type === 'text' || f.type === 'password') {
      const input = el('input');
      if (f.type === 'password') input.type = 'password'; else input.dataset.vim = 'on';
      input.name = f.name;
      input.value = f.value;
      input.autocomplete = 'off';
      input.spellcheck = false;
      if (f.placeholder) input.placeholder = f.placeholder;
      label.append(input);
      read.push(() => [f.name, input.value]);
    } else {
      const group = el('div', 'color-row');
      group.setAttribute('role', 'radiogroup');
      let value = f.value.toLowerCase();
      const radios = (f.palette ?? []).map((c) => {
        const r = el('input');
        r.type = 'radio';
        r.name = f.name;
        r.value = c.hex;
        r.checked = c.hex === value;
        r.title = c.name;
        r.style.setProperty('--sw', c.hex);
        r.classList.add('color-radio');
        r.setAttribute('aria-label', c.name);
        r.addEventListener('change', () => { value = c.hex; });
        return r;
      });
      group.append(...radios);
      label.append(group);
      read.push(() => [f.name, value]);
    }
    form.append(label);
  }
  const error = el('p', 'form-error');
  error.hidden = true;
  const actions = el('div', 'actions');
  const cancel = el('button', 'btn', 'Cancel');
  cancel.type = 'button';
  cancel.addEventListener('click', () => dlg.close());
  const save = el('button', 'btn primary', o.submit);
  save.type = 'submit';
  actions.append(cancel, save);
  form.append(error, actions);
  dlg.append(form);

  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const values = Object.fromEntries(read.map((r) => r()));
    const bad = o.validate(values);
    const fail = (m: string): void => { error.textContent = m; error.hidden = false; };
    if (bad) { fail(bad); return; }
    save.disabled = true;
    o.onSubmit(values).then(() => dlg.close(), (e: unknown) => {
      fail(e instanceof Error ? e.message : String(e));
      save.disabled = false;
    });
  });
  dlg.showModal();
  form.querySelector<HTMLElement>('input[type="text"]')?.focus();
  return dlg;
}

// ---------- info panel ----------

export interface InfoOpts {
  title: string;
  rows: ReadonlyArray<readonly [string, string]>;
  hint: string;
  buttons?: ReadonlyArray<{ text: string; key?: string; run: () => void }>;
}

export function openInfo(o: InfoOpts): HTMLDialogElement {
  const dlg = dialog('pick-info-dialog', o.title);
  const dl = el('dl', 'info-rows');
  for (const [k, v] of o.rows) dl.append(el('dt', undefined, k), el('dd', undefined, v));
  const foot = el('div', 'pick-actions');
  for (const b of o.buttons ?? []) {
    const btn = el('button', 'btn', b.text);
    btn.type = 'button';
    btn.addEventListener('click', () => { dlg.close(); b.run(); });
    foot.append(btn);
  }
  const close = el('button', 'btn', 'Close');
  close.type = 'button';
  close.addEventListener('click', () => dlg.close());
  foot.append(close);
  dlg.append(el('h2', undefined, o.title), dl, foot, el('div', 'pick-hint', o.hint));
  let leader = false;
  dlg.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    // Space N / Space ; (gap sweep G4), as in the TUI's read-only overlays.
    const l = pickerLeader(leader, ev, true);
    leader = l.armed;
    if (l.run) {
      ev.preventDefault();
      if (l.run !== 'arm') { dlg.close(); runLeader(l.run); }
      return;
    }
    const b = o.buttons?.find((x) => x.key && x.key === ev.key && !ev.ctrlKey && !ev.metaKey);
    if (b) { ev.preventDefault(); dlg.close(); b.run(); }
    else if (ev.key === 'q' && !ev.ctrlKey && !ev.metaKey && !ev.altKey) { ev.preventDefault(); dlg.close(); }
  });
  dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg.close(); });
  dlg.showModal();
  return dlg;
}
