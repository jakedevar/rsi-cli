// New-session dialog.
import { api, invoke } from './api.js';
import { afterLaunch, launchBlocked, launchExtras, pickLaunchModel, toggleLaunchSide } from './prompts.js';
import { launchedId } from './prompt-model.js';
import { NO_FOLDER_ASK, NO_FOLDER_CHOOSE, NO_FOLDER_START, NO_FOLDER_TITLE, defaultProjectId, folderAfterProjectChange, launchHasNoPlace, launchedToast, providerDescription, providerLabel } from './launch-model.js';
import { confirmDialog } from './approvals.js';
import { $, el, toast } from './dom.js';
import { refreshSessions, select } from './sessions.js';
import { selected, state } from './state.js';
import { openProjectForm } from './actions/sessionlist-run.js';
import { ownProjects, projectLabel } from './project-model.js';

/** Project select value that opens the New project form instead of choosing a project. */
export const NEW_PROJECT_VALUE = '__new_project__';

/** localStorage key for the project the last launch used: the default when nothing else points at one (local #271). */
const LAST_PROJECT_KEY = 'rsi.desktop.launchProject.v1';

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

const NO_PROJECTS_HINT = 'No projects yet. A project is a folder with your code that sessions work in; you can also launch without one.';


/** Values the dialog opens with (a new session like an existing one, local #225). */
export interface LaunchPrefill { query?: string; provider?: string; model?: string; project_id?: string }

export async function openLaunch(prefill?: LaunchPrefill): Promise<void> {
  const form = $<HTMLFormElement>('launch-form');
  const prov = form.elements.namedItem('provider') as HTMLSelectElement;
  if (prov.options.length === 0) {
    try {
      for (const p of await api.providers()) prov.append(providerOption(p));
    } catch (e) {
      toast(String(e));
    }
  }
  wireModelPick(form);
  wireMoreOptions(form);
  const proj = form.elements.namedItem('project_id') as HTMLSelectElement;
  fillProjectSelect(proj, prefill?.project_id ?? defaultProjectId({
    projects: state.projects,
    selectedProjectId: selected()?.project_id,
    filterProjectId: $<HTMLSelectElement>('project-filter').value,
    lastUsedId: storage()?.getItem(LAST_PROJECT_KEY),
    sessions: state.sessions,
  }));
  wireProjectSelect(proj);
  wireFolderPick(form);
  syncFolder(form, proj);
  if (prefill) applyPrefill(form, prov, proj, prefill);
  $<HTMLDialogElement>('launch-dialog').showModal();
}

/** Fill the opened dialog from an existing session; a value the form has no option for is left alone. */
function applyPrefill(form: HTMLFormElement, prov: HTMLSelectElement, proj: HTMLSelectElement, p: LaunchPrefill): void {
  if (p.provider && [...prov.options].some((o) => o.value === p.provider)) prov.value = p.provider;
  const model = form.elements.namedItem('model') as HTMLInputElement | null;
  if (model && p.model !== undefined) model.value = p.model;
  const query = form.elements.namedItem('query') as HTMLTextAreaElement | null;
  if (query && p.query !== undefined) query.value = p.query;
  // The dialog's own listeners (summary, saved-grant read) see the new choices.
  prov.dispatchEvent(new Event('change', { bubbles: true }));
  proj.dispatchEvent(new Event('change', { bubbles: true }));
}

/** A Provider option: the friendly name is the text, the exact `Session.provider` enum string is the value. */
function providerOption(provider: string): HTMLOptionElement {
  const o = el('option', undefined, providerLabel(provider));
  o.value = provider;
  o.title = providerDescription(provider);
  return o;
}

/** `List…` beside Model opens the shared model dropdown for the chosen provider; the field stays free text. */
function wireModelPick(form: HTMLFormElement): void {
  const btn = form.querySelector<HTMLButtonElement>('#launch-model-pick');
  if (!btn || btn.dataset.wired) return;
  btn.dataset.wired = '1';
  btn.addEventListener('click', () => pickLaunchModel());
}

/** `More options` beside Launch shows or hides the settings side (the same flip as Ctrl-O); the prompt is kept. */
function wireMoreOptions(form: HTMLFormElement): void {
  const btn = form.querySelector<HTMLButtonElement>('#launch-more');
  if (!btn || btn.dataset.wired) return;
  btn.dataset.wired = '1';
  btn.addEventListener('click', () => toggleLaunchSide());
}

export async function submitLaunch(ev: SubmitEvent): Promise<void> {
  ev.preventDefault();
  // A planned manager waits for the project's saved grant (desktop #204).
  const blocked = launchBlocked();
  if (blocked) { toast(blocked); return; }
  const form = $<HTMLFormElement>('launch-form');
  const data = new FormData(form);
  const str = (k: string) => String(data.get(k) ?? '').trim();
  const projectId = str('project_id');
  let workingDir = str('working_dir');
  if (!workingDir && projectId) {
    workingDir = state.projects.find((p) => p.id === projectId)?.path ?? '';
  }
  // Neither a project nor a folder: the daemon picks where it runs, so ask once, in plain words.
  if (launchHasNoPlace(projectId, workingDir) && !(await confirmNoFolder())) {
    form.querySelector<HTMLButtonElement>('#launch-folder-pick')?.focus();
    return;
  }
  const request = {
    query: String(data.get('query') ?? ''),
    provider: str('provider'),
    model: str('model') || undefined,
    effort: str('effort') || undefined,
    project_id: projectId || undefined,
    working_dir: workingDir || undefined,
  };
  try {
    // The settings side's sandbox (gap sweep G4, prompts.ts) needs the launch_prompt command.
    const { sandbox } = launchExtras();
    const result = sandbox ? await invoke<unknown>('launch_prompt', { request: { ...request, sandbox } }) : await api.launch(request);
    $<HTMLDialogElement>('launch-dialog').close();
    if (request.project_id) storage()?.setItem(LAST_PROJECT_KEY, request.project_id);
    form.reset();
    toast(launchedToast(result, request.query, sandbox), true);
    void afterLaunch(result, request.project_id);
    // Show the new session: refresh the list, then select it (when the daemon returned its id).
    const id = launchedId(result);
    void refreshSessions().then(() => { if (id) select(id); });
    window.setTimeout(() => void refreshSessions(), 500);
  } catch (e) {
    toast(`Launch failed: ${String(e)}`);
  }
}

/**
 * Ask before a launch with no project and no folder. Enter starts it, Esc and `Choose a folder` go back to
 * the dialog (Vim keys on or off: the dialog itself takes the focus, so Enter confirms).
 */
function confirmNoFolder(): Promise<boolean> {
  return confirmDialog(NO_FOLDER_ASK, NO_FOLDER_START, { title: NO_FOLDER_TITLE, cancelLabel: NO_FOLDER_CHOOSE, primary: true, chord: [] });
}

/** The Project select: none, every project, then `New project…` (always last). */
export function fillProjectSelect(proj: HTMLSelectElement, choose: string): void {
  proj.replaceChildren(el('option', undefined, 'none'));
  (proj.firstChild as HTMLOptionElement).value = '';
  for (const p of state.projects) {
    const o = el('option', undefined, projectLabel(p));
    o.value = p.id;
    o.dataset.path = p.path ?? '';
    proj.append(o);
  }
  const add = el('option', undefined, 'New project…');
  add.value = NEW_PROJECT_VALUE;
  proj.append(add);
  proj.value = [...proj.options].some((o) => o.value === choose) ? choose : '';
  proj.dataset.prev = proj.value;
  drawProjectHint(proj);
}

/** With no project yet, say what one is and give a visible button beside the select. */
function drawProjectHint(proj: HTMLSelectElement): void {
  const row = proj.closest('.row');
  if (!row) return;
  let hint = document.getElementById('launch-project-hint');
  if (ownProjects(state.projects).length > 0) { hint?.remove(); return; }
  if (!hint) {
    hint = el('p', 'launch-project-hint');
    hint.id = 'launch-project-hint';
    const btn = el('button', 'btn', 'New project…');
    btn.type = 'button';
    btn.addEventListener('click', () => newProjectFromLaunch(proj));
    hint.append(el('span', undefined, NO_PROJECTS_HINT), btn);
    row.after(hint);
  }
}

function newProjectFromLaunch(proj: HTMLSelectElement): void {
  openProjectForm(null, (made) => {
    fillProjectSelect(proj, made.id);
    // The launch dialog's own listeners (summary, #204 saved-grant read) see the new choice.
    proj.dispatchEvent(new Event('change', { bubbles: true }));
  });
}

/** The Folder box and its text-box element. */
function folderInput(form: HTMLFormElement): HTMLInputElement {
  return form.elements.namedItem('working_dir') as HTMLInputElement;
}

/** A project's folder fills the Folder box until the person writes or picks their own (folderAfterProjectChange). */
function syncFolder(form: HTMLFormElement, proj: HTMLSelectElement): void {
  const input = folderInput(form);
  const path = state.projects.find((p) => p.id === proj.value)?.path;
  const next = folderAfterProjectChange(input.value, input.dataset.auto ?? '', path);
  input.dataset.auto = next.auto;
  if (next.value === input.value) return;
  input.value = next.value;
  // The summary line under the title reads the box on input.
  input.dispatchEvent(new Event('input', { bubbles: true }));
}

/** `Choose…` beside Folder opens the system folder chooser; the box stays a text box the person can edit. */
function wireFolderPick(form: HTMLFormElement): void {
  const btn = form.querySelector<HTMLButtonElement>('#launch-folder-pick');
  if (!btn || btn.dataset.wired) return;
  btn.dataset.wired = '1';
  btn.addEventListener('click', () => void pickFolder(form));
}

async function pickFolder(form: HTMLFormElement): Promise<void> {
  const input = folderInput(form);
  const proj = form.elements.namedItem('project_id') as HTMLSelectElement;
  const start = input.value.trim() || state.projects.find((p) => p.id === proj.value)?.path || null;
  let dir: string | null;
  try {
    dir = await invoke<string | null>('choose_folder', { start });
  } catch (e) {
    toast(`Could not open the folder chooser: ${String(e)}. Type the folder path instead.`);
    return;
  }
  // Cancelled: the box is left as it was.
  if (!dir) return;
  input.value = dir;
  // A picked folder is the person's own choice: a later Project change leaves it alone.
  input.dataset.auto = '';
  input.dispatchEvent(new Event('input', { bubbles: true }));
  input.focus();
}

function wireProjectSelect(proj: HTMLSelectElement): void {
  if (proj.dataset.newWired) return;
  proj.dataset.newWired = '1';
  // Runs before the form's own change listeners: choosing `New project…` opens the
  // form and puts the previous choice back, so the saved-grant logic never sees it.
  proj.addEventListener('change', (ev) => {
    if (proj.value !== NEW_PROJECT_VALUE) {
      proj.dataset.prev = proj.value;
      const form = proj.form;
      if (form) syncFolder(form, proj);
      return;
    }
    ev.stopPropagation();
    proj.value = proj.dataset.prev ?? '';
    newProjectFromLaunch(proj);
  });
}
