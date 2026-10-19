// The model dropdown's DOM (gap sweep G4, hub #3000): one dialog for every
// model choice (Ctrl-M / :model, the New session dialog, the create-entity
// form). Keys and filtering are model-dropdown-model.ts; the catalog is rsid's
// DiscoverModels through the `discover_models` command (as the TUI's
// discovery). Daemon text is set with textContent only.
import { api, invoke } from './api.js';
import { el } from './dom.js';
import {
  type DropdownState, type Hit, type ModelEntry, NEW_DROPDOWN, cycleProvider, dropdownKey, filterModels, orderProviders, queryLine,
} from './model-dropdown-model.js';
import { modelIds } from './nav-model.js';
import { focusOwnsKey } from './modal-gate.js';

export interface DropdownOpts {
  title: string;
  /** The provider the picker opens on. */
  provider: string;
  /** Marked with a check. */
  current?: { provider: string; model: string };
  /** Add a "Provider default" row (id ""). */
  withDefault?: boolean;
  /** A fixed catalog: no provider cycling (Tab), and `m` closes. */
  fixed?: readonly ModelEntry[];
  onPick: (provider: string, model: ModelEntry) => void;
  onClose?: () => void;
}

const catalogs = new Map<string, Promise<ModelEntry[]>>();

/** The provider's models (cached for the window's life; a failure is not cached). */
export function providerModels(provider: string): Promise<ModelEntry[]> {
  let p = catalogs.get(provider);
  if (!p) {
    p = invoke<unknown>('discover_models', { provider }).then((v) => modelIds(v), (e: unknown) => { catalogs.delete(provider); throw e; });
    catalogs.set(provider, p);
  }
  return p;
}

let providerList: Promise<string[]> | null = null;

/** The daemon's providers, in the TUI's cycle order. */
export function providers(): Promise<string[]> {
  providerList ??= api.providers().then((p) => orderProviders(p), () => { providerList = null; return ['Claude']; });
  return providerList;
}

let open: HTMLDialogElement | null = null;

export function modelDropdownOpen(): boolean {
  return open?.open ?? false;
}

export function openModelDropdown(o: DropdownOpts): HTMLDialogElement {
  open?.close();
  const dlg = el('dialog', 'pick model-dropdown');
  dlg.setAttribute('aria-label', o.title);
  const head = el('h2', undefined, o.title);
  const prov = el('div', 'md-provider');
  const query = el('div', 'md-query');
  const list = el('ul', 'pick-list');
  list.setAttribute('role', 'listbox');
  const hint = el('div', 'pick-hint', o.fixed
    ? 'j/k move · / filter · 1-9 or Enter pick · Esc, q or m close'
    : 'j/k move · g/G ends · / filter · Tab/Shift-Tab provider · 1-9 or Enter pick · Esc or q close');
  dlg.append(head, prov, query, list, hint);
  document.body.append(dlg);
  open = dlg;

  let provider = o.provider;
  let order: string[] = [provider];
  let entries: ModelEntry[] = o.fixed ? [...o.fixed] : [];
  let status: string | null = o.fixed ? null : 'Loading models…';
  let st: DropdownState = NEW_DROPDOWN;
  let hits: Hit[] = [];
  let epoch = 0;

  const withDefault = (list: ModelEntry[]): ModelEntry[] => (o.withDefault ? [{ id: '', label: 'Provider default' }, ...list] : list);

  const render = (): void => {
    hits = filterModels(entries, st.query);
    if (st.sel >= hits.length) st = { ...st, sel: Math.max(0, hits.length - 1) };
    prov.replaceChildren(
      el('span', 'md-arrow', o.fixed ? '' : '‹ '),
      el('span', 'md-name', o.fixed ? 'Models' : provider),
      el('span', 'md-arrow', o.fixed ? '' : ' ›'),
    );
    query.textContent = queryLine(st, hits.length, entries.length);
    query.classList.toggle('typing', st.typing);
    if (status) { list.replaceChildren(el('li', 'pick-empty', status)); return; }
    if (hits.length === 0) { list.replaceChildren(el('li', 'pick-empty', entries.length ? 'No model matches' : 'No models discovered for this provider')); return; }
    list.replaceChildren(...hits.map((h, i) => {
      const li = el('li', `pick-item${i === st.sel ? ' sel' : ''}`);
      li.dataset.index = String(i);
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(i === st.sel));
      const isCurrent = o.current && o.current.provider === (o.fixed ? o.current.provider : provider) && o.current.model === h.entry.id;
      li.append(el('span', 'pick-mark', isCurrent ? '✓' : i < 9 ? String(i + 1) : ''));
      const label = el('span', 'pick-label');
      let at = 0;
      for (const [a, b] of h.ranges) {
        if (a > at) label.append(document.createTextNode(h.entry.label.slice(at, a)));
        label.append(el('mark', undefined, h.entry.label.slice(a, b)));
        at = b;
      }
      label.append(document.createTextNode(h.entry.label.slice(at)));
      li.append(label);
      if (h.entry.label !== h.entry.id && h.entry.id !== '') li.append(el('span', 'pick-detail', h.entry.id));
      return li;
    }));
    list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  };

  const load = (p: string): void => {
    provider = p;
    if (o.fixed) { render(); return; }
    const mine = ++epoch;
    status = 'Loading models…';
    entries = [];
    render();
    providerModels(p).then((m) => {
      if (mine !== epoch) return;
      entries = withDefault(m);
      status = null;
      render();
    }, (e: unknown) => {
      if (mine !== epoch) return;
      entries = withDefault([]);
      status = o.withDefault ? null : `Could not list models: ${e instanceof Error ? e.message : String(e)}`;
      render();
    });
  };

  const pick = (i: number): void => {
    const h = hits[i];
    if (!h) return;
    dlg.close();
    o.onPick(o.fixed ? (o.current?.provider ?? provider) : provider, h.entry);
  };

  dlg.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    // Ctrl-M closes the global picker from inside it (the TUI's toggle).
    if (ev.ctrlKey && !ev.metaKey && !ev.altKey && ev.key.toLowerCase() === 'm') { ev.preventDefault(); ev.stopPropagation(); dlg.close(); return; }
    const r = dropdownKey(st, ev, hits.length, !!o.fixed);
    ev.preventDefault();
    ev.stopPropagation();
    st = r.st;
    switch (r.effect.kind) {
      case 'none': render(); return;
      case 'close': dlg.close(); return;
      case 'pick': pick(r.effect.index); return;
      case 'provider': load(cycleProvider(order, provider, r.effect.by)); return;
    }
  });
  list.addEventListener('click', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.pick-item');
    if (li) pick(Number(li.dataset.index));
  });
  prov.addEventListener('click', () => { if (!o.fixed) load(cycleProvider(order, provider, 1)); });
  dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg.close(); });
  dlg.addEventListener('close', () => { if (open === dlg) open = null; dlg.remove(); o.onClose?.(); });

  if (!o.fixed) {
    void providers().then((p) => {
      order = p.includes(provider) ? p : [provider, ...p];
      render();
    });
  }
  load(provider);
  dlg.showModal();
  dlg.focus();
  return dlg;
}

const CSS = `
.model-dropdown .md-provider { display: flex; gap: 2px; align-items: center; font-weight: 600; cursor: pointer; padding: 0 0 4px; }
.model-dropdown .md-arrow { color: var(--subtext); }
.model-dropdown .md-query { font-family: var(--mono); font-size: 12px; color: var(--subtext); padding: 2px 0 6px; }
.model-dropdown .md-query.typing { color: var(--text); }
.model-dropdown mark { background: transparent; color: var(--yellow); font-weight: 700; }
.model-dropdown .pick-mark { min-width: 1.5ch; color: var(--subtext); }
`;

let styled = false;

export function initModelDropdown(): void {
  if (styled) return;
  styled = true;
  const style = document.createElement('style');
  style.id = 'model-dropdown-style';
  style.textContent = CSS;
  document.head.append(style);
}
