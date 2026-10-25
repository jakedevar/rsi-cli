// The launch / continue prompts and the input modal (gap sweep G4, hub #3000),
// DOM side. Chords and the settings side are prompt-model.ts (pure, tested):
//
// - every prompt: Ctrl-Q closes (the draft is kept), Ctrl-Y compiles and
//   Ctrl-Shift-G fixes grammar into a preview (a accepts, d / Esc discard,
//   Ctrl+Enter sends the original), ? in vim normal mode opens the help;
// - the New session prompt: Ctrl-O (or Tab in normal mode) flips to the
//   settings side (model, effort, sandbox, manager, scope, policy, grant),
//   Ctrl-M opens the model dropdown, Ctrl-E steps the effort, Ctrl-B toggles
//   the sandbox, and a header row summarizes the launch;
// - the input modal: Enter sends when Submit on Enter is on.
//
// Compile and grammar reuse G1's commands (`compile_prompt`, `generate_text`)
// and its pure rules (input-model.ts). Daemon text is set with textContent.
import { invoke } from './api.js';
import { snapshot } from './commands.js';
import { $, el, toast } from './dom.js';
import { type Correction, compileFailure, compileOutcome, previewKey } from './input-model.js';
import { keyToken } from './keyengine.js';
import { openKeyHelp } from './keyhelp.js';
import { openModelDropdown } from './model-dropdown.js';
import {
  type GrantContext, type LaunchSettings, type PromptKind, PRESET_LABELS, ROW_HELP, ROW_LABELS, type SettingRow, appointArgs,
  freshPlan, grantStatus, launchBlock, launchSummary, launchedId, policyLabel, promptKey, regrantsSaved, scopeChoices,
  scopeLabel, settingRows, settingsKey, stepEffort, stepPlanPreset, stepScope,
} from './prompt-model.js';
import { HOME_WORKTREE_WHY, isHomeProjectId } from './project-model.js';
import { currentSessions, state } from './state.js';
import { providerDescription, providerLabel } from './launch-model.js';
import { editorOf, isVimField, modeOf } from './vim-field.js';
import { sendChord } from './keylabel.js';
import { currentPlatform, isVimKeysOn } from './vimkeys.js';
import { focusOwnsKey } from './modal-gate.js';
import { syncListbox } from './listbox-a11y.js';

// ---------- which prompt ----------

function promptOf(t: Element | null): { kind: PromptKind; dialog: HTMLDialogElement; field: HTMLTextAreaElement | null } | null {
  const dlg = t?.closest<HTMLDialogElement>('#launch-dialog, #continue-dialog, #g1-modal-dialog');
  if (!dlg || !dlg.open) return null;
  const kind: PromptKind = dlg.id === 'launch-dialog' ? 'launch' : dlg.id === 'continue-dialog' ? 'continue' : 'modal';
  const field = t instanceof HTMLTextAreaElement ? t : null;
  return { kind, dialog: dlg, field };
}

// ---------- the correction preview (Ctrl-Y, Ctrl-Shift-G) ----------

interface Preview { dialog: HTMLDialogElement; field: HTMLTextAreaElement; box: HTMLElement; correction: Correction | null; epoch: number }

let preview: Preview | null = null;

function previewBox(dialog: HTMLDialogElement, field: HTMLTextAreaElement): Preview {
  if (preview && preview.dialog === dialog && preview.field === field) return preview;
  preview?.box.remove();
  const box = el('div', 'g4-correction');
  field.closest('label')?.after(box) ?? field.after(box);
  preview = { dialog, field, box, correction: null, epoch: 0 };
  dialog.addEventListener('close', () => { if (preview?.dialog === dialog) { preview.box.remove(); preview = null; } }, { once: true });
  return preview;
}

function drawPreview(p: Preview, busy: string | null): void {
  p.box.replaceChildren();
  p.box.hidden = !busy && !p.correction;
  if (busy) { p.box.append(el('div', 'g4-correction-head', busy)); return; }
  const c = p.correction;
  if (!c) return;
  p.box.append(
    el('div', 'g4-correction-head', c.kind === 'compile' ? (c.clarify ? 'The compiler needs an answer' : 'Compiled prompt') : 'Grammar and spelling'),
    el('pre', 'g4-correction-text', c.text),
    el('div', 'pick-hint', c.clarify ? 'd or Esc dismiss · edit the draft and Ctrl-Y again' : `a accept · d or Esc discard · ${sendChord(currentPlatform())} sends the original`),
  );
}

async function correct(kind: 'compile' | 'grammar', dialog: HTMLDialogElement, field: HTMLTextAreaElement): Promise<void> {
  const draft = field.value;
  if (draft.trim() === '') { toast('Nothing to correct: the draft is empty'); return; }
  const p = previewBox(dialog, field);
  const mine = ++p.epoch;
  p.correction = null;
  drawPreview(p, kind === 'compile' ? 'COMPILING…' : 'CORRECTING…');
  try {
    if (kind === 'compile') {
      const out = compileOutcome(await invoke<unknown>('compile_prompt', { input: draft }));
      if (mine !== p.epoch) return;
      p.correction = out.preview;
      if (out.notice) toast(out.notice);
    } else {
      const r = await invoke<{ text: string }>('generate_text', { draft, question: null });
      if (mine !== p.epoch) return;
      p.correction = { text: r.text, clarify: false, kind: 'grammar' };
    }
  } catch (e) {
    if (mine !== p.epoch) return;
    if (kind === 'compile') {
      const out = compileFailure(String(e));
      p.correction = out.preview;
      if (out.notice) toast(out.notice);
    } else {
      toast(`Grammar fix failed: ${String(e)}`);
    }
  }
  drawPreview(p, null);
}

/** A key while a preview shows; true when it took the key. */
function previewKeyDown(ev: KeyboardEvent, insert: boolean): boolean {
  const p = preview;
  if (!p?.correction || !p.dialog.open) return false;
  const token = keyToken(ev, currentPlatform());
  if (token === null) return false;
  const plain = !/^[CAS]-/.test(token) && token !== 'Esc';
  if (insert && plain && document.activeElement === p.field) return false;
  const k = previewKey(token, p.correction);
  if (!k || k === 'send-original' || k === 'compile' || k === 'grammar') {
    if (k === 'send-original') { p.correction = null; drawPreview(p, null); }
    return false;
  }
  if (k === 'accept') {
    p.field.value = p.correction.text;
    p.field.dispatchEvent(new Event('input', { bubbles: true }));
  }
  p.correction = null;
  drawPreview(p, null);
  return true;
}

// ---------- the New session prompt's settings side ----------

let sandbox = false;
let manager: LaunchSettings['manager'] = null;
let openPolicyAfter = false;
/** The planned manager's project's saved grant (`GrantContextState`, #3270 / desktop #204). */
let grant: GrantContext | null = null;
let grantSeq = 0;
let side: { panel: HTMLElement; row: number } | null = null;

const form = (): HTMLFormElement => $<HTMLFormElement>('launch-form');
const field = <T extends HTMLElement>(name: string): T => form().elements.namedItem(name) as unknown as T;

function settings(): LaunchSettings {
  return {
    provider: field<HTMLSelectElement>('provider').value || 'Claude',
    model: field<HTMLInputElement>('model').value.trim(),
    effort: field<HTMLSelectElement>('effort').value,
    sandbox: sandbox && !homeChosen(),
    manager,
  };
}

function workingDir(): string {
  const dir = field<HTMLInputElement>('working_dir').value.trim();
  if (dir) return dir;
  const pid = field<HTMLSelectElement>('project_id').value;
  return state.projects.find((p) => p.id === pid)?.path ?? '';
}

function projectName(): string {
  const pid = field<HTMLSelectElement>('project_id').value;
  return state.projects.find((p) => p.id === pid)?.name ?? '';
}

function summaryEl(): HTMLElement {
  let s = document.getElementById('launch-summary');
  if (!s) {
    s = el('div', 'g4-launch-summary meta');
    s.id = 'launch-summary';
    form().querySelector('h2')?.after(s);
  }
  return s;
}

export function renderLaunchSummary(): void {
  if (!document.getElementById('launch-form')) return;
  // A planned manager whose project changed reads that project's saved grant.
  if (manager && projectId() && grant?.projectId !== projectId()) loadGrant();
  summaryEl().textContent = launchSummary(settings(), workingDir(), grant, projectName());
  // The line under the Provider list says what the chosen provider is (local #211).
  const hint = document.getElementById('launch-provider-hint');
  if (hint) hint.textContent = providerDescription(settings().provider);
  if (side) drawSide();
  syncMoreButton();
  drawConfirm();
}

/**
 * Read the project's saved grant (`fetch_grant_context`: `GetHarnessManagerPolicy`
 * through the operator path). Until it loads the launch is blocked; a later
 * read for another project supersedes this one.
 */
function loadGrant(): void {
  const pid = projectId();
  const seq = ++grantSeq;
  grant = { state: 'loading', projectId: pid };
  const timeout = new Promise<never>((_, reject) => window.setTimeout(() => reject(new Error('grant read timed out')), 3000));
  void Promise.race([invoke<unknown>('harness_manager_policy_get', { projectId: pid }), timeout]).then(
    (saved) => { if (seq === grantSeq) { grant = { state: 'loaded', projectId: pid, saved: saved ?? null }; renderLaunchSummary(); } },
    (e: unknown) => {
      if (seq !== grantSeq) return;
      const reason = e instanceof Error ? e.message : String(e);
      grant = { state: 'failed', projectId: pid, reason: reason.startsWith('grant read') ? reason : `saved grant read failed: ${reason}` };
      renderLaunchSummary();
    },
  );
}

/** Why the launch cannot be confirmed yet (`block_unloaded_grant`), else null. */
export function launchBlocked(): string | null {
  return launchBlock(manager, grant, projectId());
}

/** The Launch button waits for the saved grant, saying so. */
function drawConfirm(): void {
  const button = form().querySelector<HTMLButtonElement>('button[type="submit"]');
  if (!button) return;
  const why = launchBlocked();
  button.disabled = why !== null;
  button.title = why ?? '';
  button.textContent = why !== null && grant?.state === 'loading' ? 'Loading saved grant…' : 'Launch';
}

function setEffort(v: string): void {
  const sel = field<HTMLSelectElement>('effort');
  if (v && ![...sel.options].some((o) => o.value === v)) sel.append(el('option', undefined, v));
  sel.value = v;
}

function stepLaunchEffort(by: 1 | -1): void {
  const s = settings();
  setEffort(stepEffort(s.model, s.effort, by));
  renderLaunchSummary();
  toast(`Effort: ${field<HTMLSelectElement>('effort').value || 'model default'}`, true);
}

/** RSI home is the person's home folder, never a repository: no worktree there (desktop #319). */
function homeChosen(): boolean {
  return isHomeProjectId(projectId());
}

function toggleSandbox(): void {
  if (homeChosen()) { toast(HOME_WORKTREE_WHY); return; }
  sandbox = !sandbox;
  renderLaunchSummary();
  toast(sandbox ? 'Sandbox on: the session runs in its own git worktree' : 'Sandbox off', true);
}

export function pickLaunchModel(): void {
  const s = settings();
  openModelDropdown({
    title: 'Model for this launch',
    provider: s.provider,
    current: { provider: s.provider, model: s.model },
    withDefault: true,
    onPick: (provider, m) => {
      const prov = field<HTMLSelectElement>('provider');
      if (![...prov.options].some((o) => o.value === provider)) {
        const opt = el('option', undefined, providerLabel(provider));
        opt.value = provider;
        opt.title = providerDescription(provider);
        prov.append(opt);
      }
      prov.value = provider;
      field<HTMLInputElement>('model').value = m.id;
      renderLaunchSummary();
    },
    onClose: () => { (side ? side.panel : field<HTMLTextAreaElement>('query')).focus(); },
  });
}

function projectId(): string {
  return field<HTMLSelectElement>('project_id').value;
}

function toggleManager(): void {
  if (manager) { manager = null; openPolicyAfter = false; grant = null; grantSeq++; renderLaunchSummary(); return; }
  if (!projectId()) { toast('Choose a project first: the manager is appointed for the launched session’s project'); return; }
  manager = freshPlan();
  // Ask for what the project already has granted (#3270).
  loadGrant();
  renderLaunchSummary();
}

function drawSide(): void {
  if (!side) return;
  const s = settings();
  const rows = settingRows(s);
  side.row = Math.max(0, Math.min(rows.length - 1, side.row));
  const value = (r: SettingRow): string => {
    switch (r) {
      case 'model': return `▾ ${s.provider} · ${s.model || 'default'}`;
      case 'effort': return `◂ ${s.effort || 'default'} ▸`;
      case 'sandbox': return s.sandbox ? '━━● ON' : '○── OFF';
      case 'manager': return s.manager ? '━━● appoint' : projectId() ? '○── OFF' : '○── needs a project';
      case 'scope': return s.manager ? `◂ ${scopeLabel(s.manager.scope)} ▸` : '';
      case 'policy': return s.manager ? `◂ ${policyLabel(s.manager, grant)} ▸` : '';
      case 'grant': {
        if (!s.manager) return '';
        const status = grantStatus(grant);
        if (status) return status;
        if (openPolicyAfter) return 'open the policy editor after launch';
        return regrantsSaved(s.manager, grant) ? 're-grants the saved grant · p' : `${PRESET_LABELS[s.manager.preset]} preset as is · p`;
      }
    }
  };
  // The person's focus stays on the same control when the panel is redrawn (a click changes the row).
  const focusedAct = side.panel.contains(document.activeElement) ? (document.activeElement as HTMLElement | null)?.dataset?.act : undefined;
  const controls = new Map<string, HTMLElement>();
  const list = el('ul', 'g4-side-rows');
  rows.forEach((r, i) => {
    const li = el('li', i === side!.row ? 'sel' : '');
    // role=row in a grid (local #334): the buttons and checkboxes in the value cell stay reachable and named.
    li.setAttribute('role', 'row');
    li.setAttribute('aria-selected', String(i === side!.row));
    li.dataset.index = String(i);
    const labelCell = el('span', 'g4-side-label', ROW_LABELS[r]);
    labelCell.setAttribute('role', 'rowheader');
    const valueCell = sideValue(r, s, value(r), controls);
    valueCell.setAttribute('role', 'gridcell');
    li.append(labelCell, valueCell);
    li.addEventListener('click', () => { side!.row = i; drawSide(); });
    list.append(li);
  });
  side.panel.replaceChildren(
    el('div', 'g4-side-title', 'Launch settings'),
    list,
    el('div', 'pick-hint', ROW_HELP[rows[side.row]!]),
    el('div', 'pick-hint', sideHint(isVimKeysOn())),
  );
  // The panel holds focus and j / k move the highlighted setting (local #330).
  syncListbox(list, side.panel, { label: 'Launch setting rows', role: 'grid' });
  if (focusedAct) controls.get(focusedAct)?.focus();
}

/** The hint line under the rows: buttons first with the Vim keys off, the keys with them on. */
export function sideHint(vim: boolean): string {
  return vim
    ? 'Click or tick a choice, or: j/k move · h/l change · Space/Enter toggle or open · p grant · Ctrl-E effort · Ctrl-B sandbox · Ctrl-M model · Ctrl-Enter launch · Ctrl-O, Tab or Esc back'
    : 'Tick a box or press a button to change a choice, Tab to move between them. Press More options (or Esc) to go back to the prompt; Launch starts the session.';
}

/** A small button that changes one choice in the settings side; `act` lets a redraw keep the focus on it. */
function sideButton(controls: Map<string, HTMLElement>, act: string, text: string, name: string, onClick: () => void): HTMLButtonElement {
  const b = el('button', 'btn small', text);
  b.type = 'button';
  b.dataset.act = act;
  b.setAttribute('aria-label', name);
  b.title = name;
  b.addEventListener('click', (ev) => { ev.stopPropagation(); onClick(); });
  controls.set(act, b);
  return b;
}

/** A real checkbox for an on/off choice (own git worktree, manager, open the policy editor). */
function sideCheck(controls: Map<string, HTMLElement>, act: string, text: string, checked: boolean, onChange: () => void): HTMLElement {
  const box = el('input');
  box.type = 'checkbox';
  box.checked = checked;
  box.dataset.act = act;
  box.addEventListener('click', (ev) => ev.stopPropagation());
  box.addEventListener('change', onChange);
  controls.set(act, box);
  const label = el('label', 'g4-side-check');
  label.append(box, el('span', undefined, text));
  return label;
}

/** The value cell of a settings row: its text plus the control a mouse user presses (the keys stay the same). */
function sideValue(r: SettingRow, s: LaunchSettings, text: string, controls: Map<string, HTMLElement>): HTMLElement {
  const cell = el('span', 'g4-side-value');
  switch (r) {
    case 'model':
      cell.append(el('span', undefined, text), sideButton(controls, 'model', 'List…', 'Pick the model for this launch', pickLaunchModel));
      break;
    case 'effort':
      cell.append(
        sideButton(controls, 'effort-down', '◂', 'Lower the effort', () => stepLaunchEffort(-1)),
        el('span', undefined, s.effort || 'default'),
        sideButton(controls, 'effort-up', '▸', 'Raise the effort', () => stepLaunchEffort(1)),
      );
      break;
    case 'sandbox':
      cell.append(sideCheck(controls, 'sandbox', 'Run in its own git worktree', s.sandbox, () => toggleSandbox()));
      if (homeChosen()) {
        const box = controls.get('sandbox') as HTMLInputElement;
        box.checked = false;
        box.disabled = true;
        box.title = HOME_WORKTREE_WHY;
        cell.append(el('span', 'g4-side-why', HOME_WORKTREE_WHY));
      }
      break;
    case 'manager':
      cell.append(sideCheck(controls, 'manager', projectId() || manager ? 'Appoint it as the project manager' : 'Appoint it as the project manager (choose a project first)', !!s.manager, () => {
        toggleManager();
        (controls.get('manager') as HTMLInputElement).checked = !!manager;
      }));
      break;
    case 'scope':
      if (!s.manager) break;
      cell.append(
        sideButton(controls, 'scope-down', '◂', 'Previous scope', () => stepManagerScope(-1)),
        el('span', undefined, scopeLabel(s.manager.scope)),
        sideButton(controls, 'scope-up', '▸', 'Next scope', () => stepManagerScope(1)),
      );
      break;
    case 'policy':
      if (!s.manager) break;
      cell.append(
        sideButton(controls, 'policy-down', '◂', 'Previous grant preset', () => stepManagerPolicy(-1)),
        el('span', undefined, policyLabel(s.manager, grant)),
        sideButton(controls, 'policy-up', '▸', 'Next grant preset', () => stepManagerPolicy(1)),
      );
      break;
    case 'grant':
      if (!s.manager) break;
      cell.append(el('span', undefined, text), sideButton(controls, 'grant', grant?.state === 'failed' ? 'Retry' : openPolicyAfter ? 'Do not open the editor' : 'Open the policy editor after launch', 'Grant: toggle opening the policy editor after launch', toggleGrant));
      break;
  }
  return cell;
}

function stepManagerScope(by: 1 | -1): void {
  if (!manager) return;
  manager = { ...manager, scope: stepScope(scopeChoices(currentSessions(), projectId()), manager.scope, by) };
  renderLaunchSummary();
}

function stepManagerPolicy(by: 1 | -1): void {
  if (!manager) return;
  manager = stepPlanPreset(manager, by);
  renderLaunchSummary();
}

function toggleGrant(): void {
  if (!manager) { toast('Plan a manager first (the Manager row)'); return; }
  // One bounded read per retry after a failure (`open_grant_page`).
  if (grant?.state === 'failed') { loadGrant(); renderLaunchSummary(); return; }
  openPolicyAfter = !openPolicyAfter;
  toast(openPolicyAfter ? 'After launch the manager policy editor opens on the granted policy' : regrantsSaved(manager, grant) ? 'The saved grant is re-granted as is' : 'The preset is granted as is', true);
  drawSide();
}

/** The More options button: shows or hides the settings side (the same flip as Ctrl-O). */
export function toggleLaunchSide(): void {
  flipSide();
}

function syncMoreButton(): void {
  const b = document.getElementById('launch-more');
  if (b) b.setAttribute('aria-expanded', side ? 'true' : 'false');
}

function flipSide(): void {
  const prompt = field<HTMLTextAreaElement>('query').closest('label') as HTMLElement | null;
  if (side) {
    side.panel.remove();
    side = null;
    if (prompt) prompt.hidden = false;
    syncMoreButton();
    field<HTMLTextAreaElement>('query').focus();
    return;
  }
  const panel = el('div', 'g4-side');
  panel.id = 'launch-side';
  panel.tabIndex = 0;
  panel.setAttribute('aria-label', 'Launch settings');
  (prompt ?? summaryEl()).after(panel);
  if (prompt) prompt.hidden = true;
  side = { panel, row: 0 };
  syncMoreButton();
  drawSide();
  panel.focus();
}

function sideKey(ev: KeyboardEvent): boolean {
  if (!side) return false;
  const rows = settingRows(settings());
  const row = rows[side.row]!;
  const c = settingsKey(row, ev);
  switch (c.kind) {
    case 'none': return false;
    case 'move': side.row = Math.max(0, Math.min(rows.length - 1, side.row + c.by)); drawSide(); return true;
    case 'back': flipSide(); return true;
    case 'model': pickLaunchModel(); return true;
    case 'effort': stepLaunchEffort(c.by); return true;
    case 'sandbox': toggleSandbox(); return true;
    case 'manager': toggleManager(); return true;
    case 'scope': stepManagerScope(c.by); return true;
    case 'policy': stepManagerPolicy(c.by); return true;
    case 'grant': toggleGrant(); return true;
  }
}

// ---------- launch hooks (launch.ts) ----------

/** The extras the settings side adds to a launch. */
export function launchExtras(): { sandbox: boolean } {
  return { sandbox: sandbox && !homeChosen() };
}

/** After a successful launch: appoint the planned manager, then reset the side. */
export async function afterLaunch(result: unknown, launchProject: string | undefined): Promise<void> {
  const plan = manager;
  const thenPolicy = openPolicyAfter;
  sandbox = false;
  manager = null;
  openPolicyAfter = false;
  grant = null;
  grantSeq++;
  if (side) flipSide();
  renderLaunchSummary();
  if (!plan || !launchProject) return;
  const id = launchedId(result);
  if (!id) { toast('Session launched but not appointed manager: the daemon did not return its id. Use :manager appoint on it.'); return; }
  try {
    const r = await invoke<{ message?: string }>('appoint_launched_manager', appointArgs(plan, launchProject, id));
    toast(r.message ?? 'Manager appointed', true);
    if (thenPolicy) void import('./commands.js').then((m) => m.dispatch('mgr-policy', { source: 'menu' }));
  } catch (e) {
    toast(e instanceof Error ? e.message : String(e));
  }
}

// ---------- keys ----------

let leader = false;

function submitOnEnter(): boolean {
  try { return window.localStorage.getItem('rsi.desktop.submitOnEnter') !== 'off'; } catch { return true; }
}

function onKeyDown(ev: KeyboardEvent): void {
  // Esc on a button or box inside the settings side goes back to the prompt (the draft stays) instead of closing the dialog.
  if (ev.key === 'Escape' && side && !ev.defaultPrevented && ev.target !== side.panel && side.panel.contains(ev.target as Node | null)) {
    ev.preventDefault();
    ev.stopImmediatePropagation();
    flipSide();
    return;
  }
  if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
  if (ev.defaultPrevented || ev.isComposing) return;
  const target = document.activeElement;
  const p = promptOf(target);
  if (!p) { leader = false; return; }
  // Only the prompt's own field (or the settings side) takes prompt chords.
  const onField = p.field !== null && (p.kind !== 'launch' || p.field.name === 'query');
  const onSide = !!side && target === side.panel;
  if (!onField && !onSide && p.kind !== 'launch') return;
  const vim = isVimKeysOn() && p.field !== null && isVimField(p.field);
  const normal = vim && modeOf(p.field!) === 'normal';
  if (onField && previewKeyDown(ev, !normal)) { ev.preventDefault(); ev.stopImmediatePropagation(); return; }
  const cmd = onField || onSide || ev.ctrlKey
    ? promptKey({ kind: p.kind, normal: normal || onSide, pending: normal && editorOf(p.field!).pending() !== '', leader, submitOnEnter: submitOnEnter() }, ev)
    : 'pass';
  leader = cmd === 'arm';
  if (cmd === 'pass') {
    if (onSide && sideKey(ev)) { ev.preventDefault(); ev.stopImmediatePropagation(); }
    return;
  }
  if (onSide && cmd === 'arm') {
    // Space is the settings side's toggle key, not the leader.
    leader = false;
    if (sideKey(ev)) { ev.preventDefault(); ev.stopImmediatePropagation(); }
    return;
  }
  ev.preventDefault();
  ev.stopImmediatePropagation();
  const text = p.field ?? field<HTMLTextAreaElement>('query');
  switch (cmd) {
    case 'arm': return;
    case 'stack': toast('The desktop keeps one New session dialog (no stacked prompts): this draft stays', true); return;
    case 'stack-focus': toast('The desktop keeps one New session dialog (no stacked prompts)', true); return;
    case 'close': p.dialog.close(); return;
    case 'compile': void correct('compile', p.dialog, text); return;
    case 'grammar': void correct('grammar', p.dialog, text); return;
    case 'help': openKeyHelp(snapshot()); return;
    case 'settings': flipSide(); return;
    case 'model': pickLaunchModel(); return;
    case 'effort': stepLaunchEffort(1); return;
    case 'sandbox': toggleSandbox(); return;
    case 'launch-tab': case 'launch-split':
      toast('The desktop has no tabs or splits: the session opens in the list', true);
      form().requestSubmit();
      return;
    case 'send':
      text.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true, bubbles: true }));
      return;
  }
}

const CSS = `
.g4-launch-summary { font-size: 12px; color: var(--subtext); margin: -4px 0 8px; white-space: pre-wrap; }
.g4-side { border: 1px solid var(--surface1); border-radius: 6px; padding: 8px 10px; outline: none; min-height: 9em; }
.g4-side:focus { border-color: var(--blue); }
.g4-side-title { font-weight: 600; margin-bottom: 4px; }
.g4-side-rows { list-style: none; margin: 0 0 6px; padding: 0; font-family: var(--mono); font-size: 12px; }
.g4-side-rows li { display: flex; gap: 12px; padding: 2px 6px; border-radius: 4px; cursor: default; }
.g4-side-rows li.sel { background: var(--surface1); }
.g4-side-label { width: 8ch; color: var(--subtext); flex: none; }
.g4-side-value { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.g4-side-check { display: inline-flex; align-items: center; gap: 6px; cursor: pointer; }
.launch-more { margin-right: auto; }
.g4-correction { margin: 6px 0; padding: 8px 10px; border: 1px solid var(--surface1); border-radius: 8px; background: var(--mantle); }
.g4-correction[hidden] { display: none; }
.g4-correction-head { font-weight: 600; font-size: 12px; margin-bottom: 4px; }
.g4-correction-text { white-space: pre-wrap; margin: 0 0 4px; max-height: 30vh; overflow: auto; font-family: var(--mono); font-size: 12px; }
`;

export function initPrompts(): void {
  const style = document.createElement('style');
  style.id = 'g4-prompts-style';
  style.textContent = CSS;
  document.head.append(style);
  // Window capture: ahead of G1's document-capture handler, so the prompt
  // chords win in these three dialogs; everything else passes through.
  window.addEventListener('keydown', onKeyDown, true);
  const f = document.getElementById('launch-form') as HTMLFormElement | null;
  if (f) {
    f.addEventListener('input', renderLaunchSummary);
    f.addEventListener('change', renderLaunchSummary);
  }
  const dlg = document.getElementById('launch-dialog');
  // Opening the dialog: refresh the header (the project and default model may have changed).
  new MutationObserver(() => { if ((dlg as HTMLDialogElement | null)?.open) renderLaunchSummary(); })
    .observe(dlg ?? document.body, { attributes: true, attributeFilter: ['open'] });
}
