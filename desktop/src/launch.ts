// New-session dialog.
import { api } from './api.js';
import { $, el, toast } from './dom.js';
import { refreshSessions } from './sessions.js';
import { state } from './state.js';

export async function openLaunch(): Promise<void> {
  const form = $<HTMLFormElement>('launch-form');
  const prov = form.elements.namedItem('provider') as HTMLSelectElement;
  if (prov.options.length === 0) {
    try {
      for (const p of await api.providers()) prov.append(el('option', undefined, p));
    } catch (e) {
      toast(String(e));
    }
  }
  const proj = form.elements.namedItem('project_id') as HTMLSelectElement;
  proj.replaceChildren(el('option', undefined, 'none'));
  (proj.firstChild as HTMLOptionElement).value = '';
  for (const p of state.projects) {
    const o = el('option', undefined, p.name);
    o.value = p.id;
    o.dataset.path = p.path ?? '';
    proj.append(o);
  }
  proj.value = $<HTMLSelectElement>('project-filter').value;
  $<HTMLDialogElement>('launch-dialog').showModal();
}

export async function submitLaunch(ev: SubmitEvent): Promise<void> {
  ev.preventDefault();
  const form = $<HTMLFormElement>('launch-form');
  const data = new FormData(form);
  const str = (k: string) => String(data.get(k) ?? '').trim();
  const projectId = str('project_id');
  let workingDir = str('working_dir');
  if (!workingDir && projectId) {
    workingDir = state.projects.find((p) => p.id === projectId)?.path ?? '';
  }
  try {
    await api.launch({
      query: String(data.get('query') ?? ''),
      provider: str('provider'),
      model: str('model') || undefined,
      effort: str('effort') || undefined,
      project_id: projectId || undefined,
      working_dir: workingDir || undefined,
    });
    $<HTMLDialogElement>('launch-dialog').close();
    form.reset();
    toast('Session launched', true);
    window.setTimeout(() => void refreshSessions(), 500);
  } catch (e) {
    toast(`Launch failed: ${String(e)}`);
  }
}
