// The DOM side of the hierarchy actions (actions/hierarchy.ts): the unified
// create-entity form (overlay/create_entity_form.rs) and gL. Containers go to
// CreateContainer, leaves to LaunchSession with their kind, parent, tags and
// execution choices (src-tauri/src/hierarchy.rs). Esc keeps the draft for the
// next open; Discard (Ctrl-D) drops it; the last successful choices prefill the
// next form. Daemon text is set with textContent only.
import { api, invoke } from '../api.js';
import type { Invocation, SliceRuntime } from '../commands.js';
import { el, toast } from '../dom.js';
import {
  ALL_KINDS, EFFORT_LEVELS, type EntityDefaults, type EntityDraft, type EntityKind, type HierSession,
  creationWorkingDir, draftFor, entityRequest, isContainer, legalChildren, parentChoices, parseTags,
  scopeParent, visibleFields,
} from '../hierarchy-model.js';
import { refreshSessions } from '../sessions.js';
import { currentSessions, selected, state } from '../state.js';
import { SWEEP_CSS, injectCss } from '../sweep-css.js';
import { type TopologyDefinition, previewText, topologyPreview } from '../topology-preview.js';
import { button, dialogEl, errorLine, showError } from './detail.js';
import { type EntityField, formKey, stepChoice } from '../entity-form-keys.js';
import { openModelDropdown } from '../model-dropdown.js';
import { openPicker } from '../pickers.js';
import { currentPlatform } from '../vimkeys.js';
import { modeOf } from '../vim-field.js';
import { CREATE_KIND, type HierSnapshot, type HierarchyActionId, SLICE } from './hierarchy.js';
import { focusOwnsKey } from '../modal-gate.js';

const DEFAULTS_KEY = 'rsi.desktop.entityDefaults';

const errText = (e: unknown): string => (e instanceof Error ? e.message : String(e));

// ---------- snapshot ----------

function byId(id: string | null | undefined): HierSession | undefined {
  if (!id) return undefined;
  return state.sessions.find((s) => s.id === id) as HierSession | undefined
    ?? currentSessions().find((s) => s.id === id) as HierSession | undefined;
}

function row(): HierSession | null {
  return byId(state.cursorId) ?? (selected() as HierSession | undefined) ?? null;
}

export function hierSnapshot(): HierSnapshot {
  const r = row();
  return { row: r, scope: scopeParent(r, byId), parent: byId(r?.parent_id) ?? null };
}

// ---------- defaults and draft ----------

let draft: EntityDraft | null = null;

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

function loadDefaults(): EntityDefaults {
  try {
    const raw = storage()?.getItem(DEFAULTS_KEY);
    const v: unknown = raw ? JSON.parse(raw) : {};
    return v && typeof v === 'object' ? (v as EntityDefaults) : {};
  } catch {
    return {};
  }
}

function saveDefaults(d: EntityDraft): void {
  const keep: EntityDefaults = {
    tags: d.tags, topologyId: d.topologyId, provider: d.provider, model: d.model, effort: d.effort, sandbox: d.sandbox,
  };
  try { storage()?.setItem(DEFAULTS_KEY, JSON.stringify(keep)); } catch { /* storage unavailable */ }
}

function projectFilter(): string | null {
  return (document.getElementById('project-filter') as HTMLSelectElement | null)?.value || null;
}

// ---------- the form ----------

interface Topology { id: string; name: string; definition?: TopologyDefinition }

async function providers(): Promise<string[]> {
  try { return await api.providers(); } catch { return ['Claude']; }
}

async function topologies(): Promise<Topology[]> {
  try {
    const v = await invoke<unknown>('topology_list', { namePrefix: null });
    const list = Array.isArray(v) ? v : (v && typeof v === 'object' && Array.isArray((v as { topologies?: unknown[] }).topologies) ? (v as { topologies: unknown[] }).topologies : []);
    return list.flatMap((t) => {
      const o = t as { id?: unknown; name?: unknown; definition?: unknown };
      if (typeof o.id !== 'string') return [];
      const def = o.definition && typeof o.definition === 'object' ? o.definition as TopologyDefinition : undefined;
      return [{ id: o.id, name: typeof o.name === 'string' ? o.name : o.id, ...(def ? { definition: def } : {}) }];
    });
  } catch {
    return [];
  }
}

async function models(provider: string): Promise<string[]> {
  try {
    const v = await invoke<unknown>('discover_models', { provider });
    const list = Array.isArray(v) ? v : (v && typeof v === 'object' ? (v as { models?: unknown }).models : null);
    return Array.isArray(list) ? list.flatMap((m) => (Array.isArray(m) && typeof m[0] === 'string' ? [m[0]] : typeof m === 'string' ? [m] : [])) : [];
  } catch {
    return [];
  }
}

function labelled(text: string, control: HTMLElement): HTMLLabelElement {
  const l = el('label', undefined, text);
  l.append(control);
  return l;
}

async function openEntityForm(kind: EntityKind): Promise<void> {
  const scope = hierSnapshot().scope;
  const provs = await providers();
  const defaults = loadDefaults();
  const d: EntityDraft = draft
    ? { ...draft, kind, parentId: legalChildren(byId(draft.parentId)?.session_kind ?? null).includes(kind) ? draft.parentId : scope?.id ?? null }
    : draftFor(kind, scope?.id ?? null, defaults, provs[0] ?? 'Claude');
  if (scope && legalChildren(scope.session_kind).includes(kind)) d.parentId = scope.id;
  let submitted = false;

  const dlg = dialogEl('entity-dialog');
  dlg.classList.add('entity-dialog');
  const form = el('form', 'launch-form');
  form.noValidate = true;
  // G1's daemon-config gate (actions/input.ts onSubmit) refuses a create while the config is pending; the draft stays.
  form.dataset.configGate = '';
  const kinds = el('div', 'entity-kinds');
  kinds.setAttribute('role', 'radiogroup');
  kinds.setAttribute('aria-label', 'Kind');
  const name = el('input');
  name.type = 'text';
  name.dataset.vim = 'on';
  name.autocomplete = 'off';
  name.value = d.name;
  const tags = el('input');
  tags.type = 'text';
  tags.dataset.vim = 'on';
  tags.autocomplete = 'off';
  tags.spellcheck = false;
  tags.placeholder = 'one or more, separated by spaces or commas';
  tags.value = d.tags;
  const chips = el('div', 'entity-chips');
  const parent = el('select');
  const topo = el('select');
  const topoPreview = el('pre', 'preview-body');
  topoPreview.hidden = true;
  let tops: Topology[] = [];
  const drawPreview = (): void => {
    const def = tops.find((t) => t.id === topo.value)?.definition;
    topoPreview.hidden = !def;
    if (def) topoPreview.textContent = topologyPreview(def).map(previewText).join('\n');
  };
  const prov = el('select');
  for (const p of provs) prov.append(el('option', undefined, p));
  prov.value = provs.includes(d.provider) ? d.provider : (provs[0] ?? 'Claude');
  const model = el('input');
  model.type = 'text';
  model.dataset.vim = 'on';
  model.placeholder = 'default (m picks)';
  model.value = d.model;
  const pickModel = button('Choose…');
  pickModel.title = 'Open the model picker (m)';
  const modelList = el('datalist');
  modelList.id = 'entity-models';
  model.setAttribute('list', modelList.id);
  const effort = el('select');
  effort.append(el('option', undefined, 'default'));
  (effort.firstChild as HTMLOptionElement).value = '';
  for (const e of EFFORT_LEVELS) effort.append(el('option', undefined, e));
  effort.value = d.effort;
  const sandbox = el('input');
  sandbox.type = 'checkbox';
  sandbox.checked = d.sandbox;
  const sandboxLabel = el('label', 'entity-check');
  sandboxLabel.append(sandbox, 'Sandbox (git worktree)');
  const prompt = el('textarea');
  prompt.rows = 5;
  prompt.value = d.prompt;
  const err = errorLine();

  const rows: Record<string, HTMLElement> = {
    name: labelled('Name', name),
    tags: labelled('Tags', tags),
    parent: labelled('Parent', parent),
    topology: labelled('Topology', topo),
    provider: labelled('Provider', prov),
    model: labelled('Model', model),
    effort: labelled('Effort', effort),
    sandbox: sandboxLabel,
    prompt: labelled('Prompt', prompt),
  };
  rows.tags!.append(chips);
  rows.model!.append(pickModel);
  const pickParent = button('Choose…');
  pickParent.title = 'Open the parent picker (gp)';
  rows.parent!.append(pickParent);
  rows.topology!.append(topoPreview);
  topo.addEventListener('change', () => { read(); drawPreview(); });
  const exec = el('div', 'entity-row');
  exec.append(rows.provider!, rows.model!, rows.effort!);

  const read = (): void => {
    d.name = name.value;
    d.tags = tags.value;
    d.parentId = parent.value || null;
    d.topologyId = topo.value || null;
    d.provider = prov.value;
    d.model = model.value;
    d.effort = effort.value;
    d.sandbox = sandbox.checked;
    d.prompt = prompt.value;
  };

  const drawChips = (): void => {
    const { tags: good, bad } = parseTags(tags.value);
    chips.replaceChildren(
      ...good.map((t) => el('span', 'entity-chip', t)),
      ...bad.map((t) => el('span', 'entity-chip bad', t)),
    );
  };

  const drawParents = (): void => {
    const choices = parentChoices(d.kind, currentSessions() as HierSession[]);
    parent.replaceChildren(...choices.map((c) => {
      const o = el('option', undefined, c.label);
      o.value = c.id ?? '';
      return o;
    }));
    const want = choices.some((c) => c.id === d.parentId) ? d.parentId : (choices[0]?.id ?? null);
    parent.value = want ?? '';
    d.parentId = want;
  };

  const drawKinds = (): void => {
    const legalSomewhere = (k: EntityKind): boolean => parentChoices(k, currentSessions() as HierSession[]).length > 0;
    kinds.replaceChildren(...ALL_KINDS.map((k) => {
      const btn = button(k, 'seg');
      btn.setAttribute('role', 'radio');
      btn.setAttribute('aria-pressed', String(k === d.kind));
      btn.disabled = !legalSomewhere(k);
      btn.addEventListener('click', () => { read(); d.kind = k; draw(); });
      return btn;
    }));
  };

  const draw = (): void => {
    drawKinds();
    drawParents();
    const show = new Set(visibleFields(d.kind));
    for (const [k, node] of Object.entries(rows)) node.hidden = !show.has(k as never);
    exec.hidden = !show.has('provider');
    rows.topology!.hidden = !show.has('topology');
    drawChips();
  };

  const submit = (): void => {
    read();
    const p = byId(d.parentId);
    const projectId = p?.project_id ?? projectFilter();
    const filterProject = state.projects.find((x) => x.id === projectFilter());
    const ctx = {
      parentKind: p?.session_kind ?? null,
      projectId: projectId ?? null,
      workingDir: creationWorkingDir(filterProject?.path, p ?? null, (selected() as HierSession | undefined) ?? null),
    };
    if (p && !isContainer(p.session_kind)) { showError(err, 'The parent must be a Group or Epic'); return; }
    const r = entityRequest(d, ctx);
    if (!r.ok) { showError(err, r.why); return; }
    const { command, args } = r.request;
    invoke<unknown>(command, args as unknown as Record<string, unknown>).then(() => {
      submitted = true;
      draft = null;
      saveDefaults(d);
      dlg.close();
      toast(`Created ${d.kind}: ${d.name.trim()}`, true);
      void refreshSessions();
    }, (e: unknown) => showError(err, `Create failed: ${errText(e)}`));
  };

  const fillModels = async (): Promise<void> => {
    const ids = await models(prov.value);
    modelList.replaceChildren(...ids.map((id) => { const o = el('option'); o.value = id; return o; }));
  };
  name.addEventListener('input', () => { d.name = name.value; });
  tags.addEventListener('input', drawChips);
  parent.addEventListener('change', () => { read(); });
  prov.addEventListener('change', () => { read(); void fillModels(); });

  const discard = button('Discard', 'btn danger');
  discard.title = 'Drop the draft and close (Ctrl-D)';
  discard.addEventListener('click', () => { submitted = true; draft = null; dlg.close(); });
  const cancel = button('Close');
  cancel.title = 'Close and keep the draft for next time (Esc)';
  cancel.addEventListener('click', () => dlg.close());
  const create = button('Create', 'btn primary', 'submit');
  const actions = el('div', 'actions');
  actions.append(discard, cancel, create);
  form.append(
    el('h2', undefined, 'Create'),
    labelled('Kind', kinds),
    rows.name!, rows.tags!, rows.parent!, rows.topology!, exec, rows.sandbox!, rows.prompt!, err,
    el('div', 'dialog-note', 'Tags are required. Ctrl-Enter (Cmd-Enter) creates from any field; Esc leaves a text box, then keeps the draft and closes; Ctrl-D discards it. Outside a text box: n name, b body, k kind (h/l change it), T tags, t topology (j/k, Space preview, Enter choose, Esc clear), i edit, Ctrl-P provider, e/E effort, s sandbox, m model picker, gp parent picker, Enter create.'),
    actions, modelList,
  );
  form.addEventListener('submit', (ev) => { ev.preventDefault(); submit(); });
  form.tabIndex = -1;
  // The TUI's form keys (entity-form-keys.ts): letters in "normal" mode (focus
  // outside a text box), Esc leaves a text box first, then saves and closes.
  const fieldOf = (t: Element | null): EntityField => {
    if (!t) return 'other';
    if (kinds.contains(t)) return 'kind';
    if (t === name) return 'name';
    if (t === tags) return 'tags';
    if (t === parent) return 'parent';
    if (t === topo) return 'topology';
    if (t === prov) return 'provider';
    if (t === model) return 'model';
    if (t === effort) return 'effort';
    if (t === sandbox) return 'sandbox';
    if (t === prompt) return 'prompt';
    return 'other';
  };
  const focusField = (f: EntityField): void => {
    const target: HTMLElement | null = f === 'kind' ? kinds.querySelector<HTMLElement>('[aria-pressed="true"]') ?? kinds.querySelector('button')
      : f === 'name' ? name : f === 'tags' ? tags : f === 'topology' ? topo : f === 'prompt' ? prompt : f === 'model' ? model : null;
    if (!target || target.closest('[hidden]')) { toast(`No ${f === 'prompt' ? 'body' : f} field for a ${d.kind}`, true); return; }
    target.focus();
  };
  const pickParentNow = (): void => {
    read();
    const choices = parentChoices(d.kind, currentSessions() as HierSession[]);
    openPicker({
      title: `Parent for the ${d.kind}`,
      hint: 'Type to filter · Up/Down move · Enter sets the parent · Esc closes',
      filterable: true,
      emptyText: 'No legal parent for this kind',
      items: choices.map((c) => ({ id: c.id ?? '', label: c.label, current: c.id === d.parentId })),
      onPick: (it) => { d.parentId = it.id || null; drawParents(); parent.value = d.parentId ?? ''; },
    });
  };
  const pickModelNow = (): void => {
    read();
    if (exec.hidden) { toast(`A ${d.kind} has no model`, true); return; }
    openModelDropdown({
      title: `Model for the ${d.kind}`,
      provider: prov.value,
      current: { provider: prov.value, model: model.value },
      withDefault: true,
      onPick: (provider, m) => {
        if (![...prov.options].some((o) => o.value === provider)) prov.append(el('option', undefined, provider));
        prov.value = provider;
        model.value = m.id;
        read();
        void fillModels();
      },
    });
  };
  pickModel.addEventListener('click', pickModelNow);
  pickParent.addEventListener('click', pickParentNow);
  let pending: '' | 'g' = '';
  // The first Esc in a text box only leaves it (WebKit may still fire the dialog's cancel).
  let skipCancel = false;
  dlg.oncancel = (ev) => { if (skipCancel) { ev.preventDefault(); skipCancel = false; } };
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing || ev.defaultPrevented) return;
    const t = document.activeElement;
    const field = fieldOf(t);
    const editing = field === 'name' || field === 'tags' || field === 'model' || (field === 'prompt' && modeOf(prompt) === 'insert');
    if (t instanceof HTMLButtonElement && !kinds.contains(t) && ev.key === 'Enter' && !ev.ctrlKey && !ev.metaKey) return;
    const r = formKey({ field, editing, pending }, ev, currentPlatform() === 'mac');
    pending = r.pending;
    const c = r.cmd;
    if (c.kind === 'pass') return;
    ev.preventDefault();
    ev.stopPropagation();
    switch (c.kind) {
      case 'swallow': return;
      case 'create': form.requestSubmit(); return;
      case 'discard': discard.click(); return;
      case 'close': dlg.close(); return;
      case 'leave-edit': skipCancel = true; (t as HTMLElement | null)?.blur(); form.focus(); window.setTimeout(() => { skipCancel = false; }, 0); return;
      case 'focus': focusField(c.field); return;
      case 'kind': {
        read();
        const legal = ALL_KINDS.filter((k) => parentChoices(k, currentSessions() as HierSession[]).length > 0);
        d.kind = stepChoice(legal, d.kind, c.by);
        draw();
        focusField('kind');
        return;
      }
      case 'provider': {
        const list = [...prov.options].map((o) => o.value);
        prov.value = stepChoice(list, prov.value, c.by);
        read();
        void fillModels();
        return;
      }
      case 'effort': {
        const list = [...effort.options].map((o) => o.value);
        effort.value = stepChoice(list, effort.value, c.by);
        read();
        toast(`Effort: ${effort.value || 'default'}`, true);
        return;
      }
      case 'sandbox': if (!rows.sandbox!.hidden) { sandbox.checked = !sandbox.checked; read(); } return;
      case 'model-picker': pickModelNow(); return;
      case 'parent-picker': pickParentNow(); return;
      case 'topology-move': {
        const n = topo.options.length;
        if (n === 0) return;
        topo.selectedIndex = c.by === Infinity ? n - 1 : Math.max(0, Math.min(n - 1, topo.selectedIndex + c.by));
        read();
        if (!topoPreview.hidden || c.by === Infinity) drawPreview();
        return;
      }
      case 'topology-preview': {
        const def = tops.find((x) => x.id === topo.value)?.definition;
        if (!def) { toast('No topology chosen, or it has no definition to preview', true); return; }
        if (topoPreview.hidden) drawPreview(); else topoPreview.hidden = true;
        return;
      }
      case 'topology-choose': read(); focusField(rows.prompt!.hidden ? 'kind' : 'prompt'); return;
      case 'topology-clear': topo.value = ''; read(); drawPreview(); focusField('kind'); return;
    }
  };
  dlg.onclose = () => { if (!submitted) { read(); draft = { ...d }; } };
  dlg.replaceChildren(form);
  draw();
  dlg.showModal();
  name.focus();

  tops = await topologies();
  topo.replaceChildren(el('option', undefined, '(no topology)'), ...tops.map((t) => {
    const o = el('option', undefined, t.name);
    o.value = t.id;
    return o;
  }));
  (topo.firstChild as HTMLOptionElement).value = '';
  topo.value = tops.some((t) => t.id === d.topologyId) ? d.topologyId! : '';
  drawPreview();
  void fillModels();
}

// ---------- gL ----------

async function setLead(): Promise<void> {
  const snap = hierSnapshot();
  const t = snap.row;
  const p = snap.parent;
  if (!t || !p) return;
  try {
    await invoke<unknown>('set_epic_lead', { epicId: p.id, newLeadSessionId: t.id });
    toast('Lead set', true);
    void refreshSessions();
  } catch (e) {
    toast(`Set lead failed: ${errText(e)}`);
  }
}

// ---------- dispatch ----------

export function runHierarchy(id: HierarchyActionId, _inv: Invocation): void {
  if (id === 'set-epic-lead') { void setLead(); return; }
  void openEntityForm(CREATE_KIND[id]);
}

/** The hierarchy slice's DOM side, collected by actions/runtime.generated.ts. */
export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runHierarchy(id as HierarchyActionId, inv),
  snapshot: () => ({ hier: hierSnapshot() }),
  init: () => injectCss('sweep-css', SWEEP_CSS),
};
