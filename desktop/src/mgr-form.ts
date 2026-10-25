// A small modal form for the manager surfaces (hub #3000 gap sweep G3): typed
// fields plus an optional checklist, as the TUI's in-place editors (the tree's
// grant and node editors, the workspace's launch form) collect them. Enter in
// a field submits, Esc cancels; values come back as typed and the caller's
// pure rules check them. Daemon text is set with textContent only.
import { el } from './dom.js';

export interface FormField {
  key: string;
  label: string;
  kind: 'text' | 'number' | 'check' | 'select' | 'textarea';
  value: string | boolean;
  /** `select` choices as [value, label]. */
  options?: ReadonlyArray<readonly [string, string]>;
  hint?: string;
}

export interface FormSpec {
  title: string;
  lines?: string[];
  fields: FormField[];
  /** A checklist (projects, launches): value, label, checked. */
  checks?: Checklist;
  /** More checklists, returned by key in `lists`. */
  lists?: ReadonlyArray<Checklist & { key: string }>;
  ok: string;
}

/**
 * A checklist item may carry a `mark`: a warning that shows next to the row while the row is checked (#324: a launch
 * the global manager’s grant does not cover). The list’s `note` explains the marks and shows while any marked row is checked.
 */
export interface Checklist { title: string; note?: string; items: ReadonlyArray<{ value: string; label: string; on: boolean; mark?: string }> }

export interface FormResult { values: Record<string, string | boolean>; checked: string[]; lists: Record<string, string[]> }

export function formDialog(spec: FormSpec): Promise<FormResult | null> {
  return new Promise((resolve) => {
    const d = el('dialog', 'pick mgr-dialog mgr-form');
    const form = el('form');
    form.method = 'dialog';
    form.append(el('h2', undefined, spec.title));
    for (const l of spec.lines ?? []) form.append(el('p', 'mgr-note', l));
    const inputs = new Map<string, HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>();
    for (const f of spec.fields) {
      const lab = el('label', 'mgr-form-row');
      let input: HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement;
      if (f.kind === 'select') {
        const sel = el('select');
        for (const [v, t] of f.options ?? []) { const o = el('option', undefined, t); o.value = v; sel.append(o); }
        sel.value = String(f.value);
        input = sel;
      } else if (f.kind === 'textarea') {
        const ta = el('textarea');
        ta.rows = 4;
        ta.value = String(f.value);
        input = ta;
      } else {
        const inp = el('input');
        inp.type = f.kind === 'check' ? 'checkbox' : 'text';
        if (inp.type === 'text') inp.dataset.vim = 'on';
        if (f.kind === 'number') inp.inputMode = 'decimal';
        if (f.kind === 'check') inp.checked = f.value === true; else inp.value = String(f.value);
        input = inp;
      }
      input.name = f.key;
      inputs.set(f.key, input);
      lab.append(el('span', undefined, f.label), input);
      if (f.hint) lab.append(el('small', 'mgr-note', f.hint));
      form.append(lab);
    }
    const groups: Array<[string, HTMLInputElement[]]> = [];
    const all = [...(spec.checks ? [{ ...spec.checks, key: '' }] : []), ...(spec.lists ?? [])];
    for (const list of all) {
      form.append(el('h4', undefined, list.title));
      const wrap = el('div', 'mgr-checks');
      const boxes: HTMLInputElement[] = [];
      const marked: Array<[HTMLInputElement, HTMLElement]> = [];
      const note = list.note ? el('p', 'mgr-note mgr-mark-note', list.note) : null;
      if (note) note.setAttribute('role', 'note');
      const showMarks = () => {
        for (const [cb, mark] of marked) mark.hidden = !cb.checked;
        if (note) note.hidden = !marked.some(([cb]) => cb.checked);
      };
      for (const it of list.items) {
        const lab = el('label');
        const cb = el('input');
        cb.type = 'checkbox';
        cb.value = it.value;
        cb.checked = it.on;
        boxes.push(cb);
        lab.append(cb, document.createTextNode(` ${it.label}`));
        if (it.mark) {
          const mark = el('small', 'mgr-mark', it.mark);
          lab.append(mark);
          marked.push([cb, mark]);
          cb.addEventListener('change', showMarks);
        }
        wrap.append(lab);
      }
      groups.push([list.key, boxes]);
      form.append(wrap);
      if (note) form.append(note);
      showMarks();
    }
    const actions = el('div', 'pick-actions');
    const cancel = el('button', 'btn', 'Cancel');
    cancel.type = 'button';
    const ok = el('button', 'btn primary', spec.ok);
    ok.type = 'submit';
    actions.append(cancel, ok);
    form.append(actions);
    d.append(form);
    document.body.append(d);
    let done = false;
    const finish = (v: FormResult | null) => {
      if (done) return;
      done = true;
      d.close();
      d.remove();
      resolve(v);
    };
    const accept = () => {
      const values: Record<string, string | boolean> = {};
      for (const [k, input] of inputs) values[k] = input instanceof HTMLInputElement && input.type === 'checkbox' ? input.checked : input.value;
      const lists: Record<string, string[]> = {};
      for (const [key, boxes] of groups) lists[key] = boxes.filter((b) => b.checked).map((b) => b.value);
      finish({ values, checked: lists[''] ?? [], lists });
    };
    cancel.addEventListener('click', () => finish(null));
    form.addEventListener('submit', (ev) => { ev.preventDefault(); accept(); });
    d.addEventListener('cancel', (ev) => { ev.preventDefault(); finish(null); });
    d.showModal();
    (inputs.values().next().value ?? groups[0]?.[1][0] ?? ok).focus();
  });
}

export const FORM_CSS = `
.mgr-form { width: min(720px, 94vw); max-height: 90vh; overflow: auto; }
.mgr-form .mgr-form-row { display: grid; grid-template-columns: minmax(150px, 1fr) 2fr; gap: 4px 10px; align-items: center; margin: 4px 0; font-size: 13px; }
.mgr-form .mgr-form-row small { grid-column: 2; }
.mgr-form textarea { grid-column: 1 / -1; }
.mgr-form .mgr-mark { display: block; color: var(--peach); }
.mgr-form h4 { margin: 10px 0 4px; font-size: 12px; color: var(--subtext); }
`;
