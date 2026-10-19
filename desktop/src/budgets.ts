// Budgets (gap sweep G4, hub #3000), DOM side: the Budgets list (Settings ›
// Safety & Spend › Budgets) and the budget policy form. Keys and fields are
// budgets-model.ts; reads and writes are src-tauri/src/budgets.rs. Daemon text
// is set with textContent only.
import { invoke } from './api.js';
import { el, toast } from './dom.js';
import {
  type BudgetPolicy, EMPTY_BUDGETS, FIELDS, FIELD_LABELS, type FormState, budgetFormKey, formFor, policyLine,
} from './budgets-model.js';
import { focusOwnsKey } from './modal-gate.js';

const errText = (e: unknown): string => (e instanceof Error ? e.message : String(e));

async function load(): Promise<BudgetPolicy[]> {
  const r = await invoke<{ policies?: BudgetPolicy[] }>('budget_policies');
  return Array.isArray(r.policies) ? r.policies : [];
}

/** The Budgets list: j/k move, Enter / Space edit (or add when empty), a add, d delete, R refresh, Esc / q close. */
export function openBudgets(onChange: () => void = () => undefined): void {
  const dlg = el('dialog', 'pick budgets-dialog');
  dlg.setAttribute('aria-label', 'Budgets');
  const list = el('ul', 'pick-list');
  const status = el('div', 'pick-hint', 'Loading…');
  dlg.append(el('h2', undefined, 'Budget policies'), status, list,
    el('div', 'pick-hint', 'j/k move · Enter or Space edit · a add · d delete · R refresh · Esc or q close'));
  document.body.append(dlg);
  let policies: BudgetPolicy[] = [];
  let sel = 0;
  const draw = (): void => {
    sel = Math.max(0, Math.min(policies.length - 1, sel));
    if (policies.length === 0) { list.replaceChildren(el('li', 'pick-empty', EMPTY_BUDGETS)); return; }
    list.replaceChildren(...policies.map((p, i) => {
      const li = el('li', `pick-item${i === sel ? ' sel' : ''}`, policyLine(p));
      li.addEventListener('click', () => { sel = i; draw(); });
      li.addEventListener('dblclick', () => edit());
      return li;
    }));
    list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  };
  const refresh = async (): Promise<void> => {
    status.textContent = 'Loading…';
    try {
      policies = await load();
      status.textContent = `${policies.length} polic${policies.length === 1 ? 'y' : 'ies'} (UpdateModelControlPolicy replaces the whole list)`;
    } catch (e) {
      status.textContent = `Could not read the budgets: ${errText(e)}`;
    }
    draw();
  };
  const edit = (): void => {
    const p = policies[sel];
    openBudgetForm(p ? { index: sel, policy: p } : null, () => { void refresh(); onChange(); });
  };
  const remove = async (): Promise<void> => {
    const p = policies[sel];
    if (!p) return;
    try {
      await invoke<unknown>('budget_policy_delete', { index: sel, seen: p });
      toast('Budget policy deleted', true);
      onChange();
    } catch (e) {
      toast(`Delete failed: ${errText(e)}`);
    }
    void refresh();
  };
  dlg.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    let handled = true;
    switch (ev.key) {
      case 'j': case 'ArrowDown': sel++; draw(); break;
      case 'k': case 'ArrowUp': sel--; draw(); break;
      case 'Enter': case ' ': edit(); break;
      case 'a': openBudgetForm(null, () => { void refresh(); onChange(); }); break;
      case 'd': void remove(); break;
      case 'R': void refresh(); break;
      case 'q': dlg.close(); break;
      default: handled = false;
    }
    if (handled) { ev.preventDefault(); ev.stopPropagation(); }
  });
  dlg.addEventListener('close', () => dlg.remove());
  dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg.close(); });
  dlg.showModal();
  dlg.focus();
  void refresh();
}

/** The form: Tab / Shift-Tab fields, Up / Down cycle the scope kind and model tier, Enter saves, Esc closes. */
export function openBudgetForm(editing: { index: number; policy: BudgetPolicy } | null, saved: () => void): void {
  const dlg = el('dialog', 'pick budget-form-dialog');
  dlg.setAttribute('aria-label', 'Budget Policy Form');
  const rows = el('ul', 'budget-form');
  const err = el('div', 'form-error');
  err.hidden = true;
  dlg.append(el('h2', undefined, editing ? 'Edit budget policy' : 'New budget policy'), rows, err,
    el('div', 'pick-hint', 'Tab / Shift-Tab fields · Up/Down cycle scope kind and model tier · type (digits in the limits) · Enter saves · Esc closes'));
  document.body.append(dlg);
  let st: FormState = { form: formFor(editing?.policy ?? null), field: 0 };
  let busy = false;
  const draw = (): void => {
    rows.replaceChildren(...FIELDS.map((f, i) => {
      const li = el('li', i === st.field ? 'sel' : '');
      const cyc = f === 'scope_kind' || f === 'model_tier';
      const v = st.form[f];
      li.append(el('span', 'bf-label', FIELD_LABELS[f]), el('span', 'bf-value', cyc ? `◂ ${v} ▸` : `${v}${i === st.field ? '▏' : ''}`));
      if (f === 'scope_id' && st.form.scope_kind === 'global') li.classList.add('dim');
      li.addEventListener('click', () => { st = { ...st, field: i }; draw(); });
      return li;
    }));
  };
  const save = async (): Promise<void> => {
    if (busy) return;
    busy = true;
    try {
      await invoke<unknown>('budget_policy_save', {
        form: st.form, editing: editing?.index ?? null, seen: editing?.policy ?? null,
      });
      toast(editing ? 'Budget policy saved' : 'Budget policy added', true);
      dlg.close();
      saved();
    } catch (e) {
      err.textContent = errText(e);
      err.hidden = false;
    } finally {
      busy = false;
    }
  };
  dlg.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    const r = budgetFormKey(st, ev);
    if (r.effect.kind === 'none' && r.st === st && (ev.ctrlKey || ev.metaKey || ev.altKey)) return;
    ev.preventDefault();
    ev.stopPropagation();
    st = r.st;
    if (r.effect.kind === 'save') { void save(); return; }
    if (r.effect.kind === 'close') { dlg.close(); return; }
    err.hidden = true;
    draw();
  });
  dlg.addEventListener('paste', (ev) => {
    const text = ev.clipboardData?.getData('text') ?? '';
    ev.preventDefault();
    for (const ch of text) st = budgetFormKey(st, { key: ch }).st;
    draw();
  });
  dlg.addEventListener('close', () => dlg.remove());
  draw();
  dlg.showModal();
  dlg.focus();
}

export const BUDGETS_CSS = `
.budget-form { list-style: none; margin: 6px 0; padding: 0; font-family: var(--mono); font-size: 12px; }
.budget-form li { display: flex; gap: 12px; padding: 3px 6px; border-radius: 4px; }
.budget-form li.sel { background: var(--surface1); }
.budget-form li.dim { opacity: 0.5; }
.budget-form .bf-label { width: 22ch; color: var(--subtext); }
`;
