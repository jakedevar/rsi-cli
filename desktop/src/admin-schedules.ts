// Scheduled jobs browser and form (slice 13a; TUI `<Space>K`,
// crates/rsi/src/overlay/schedule_browser.rs and schedule_form.rs). Rules in
// admin-model.ts; Tauri commands in src-tauri/src/admin.rs.
import { api, invoke } from './api.js';
import { el, toast } from './dom.js';
import {
  type JobHold, type JobPage, type ScheduleFormState, type ScheduledJob,
  RECURRENCES, cycleRecurrence, deleteChord, editScheduleForm, jobRow, moveCursor, newScheduleForm, scheduleRequest,
} from './admin-model.js';
import {
  actions, adminDialog, button, errorLine, inField, listBox, note, rowEl, show, showError, textField,
} from './admin-dom.js';
import { emptyText } from './empty-states.js';
import { focusOwnsKey } from './modal-gate.js';
import { syncListbox } from './listbox-a11y.js';
import { loadView } from './view-load.js';

interface BrowserState {
  jobs: ScheduledJob[];
  holds: JobHold[];
  includeHistory: boolean;
  nextCursor: string | null;
  at: number;
  armed: string | null;
}

let st: BrowserState = { jobs: [], holds: [], includeHistory: false, nextCursor: null, at: 0, armed: null };

/** The browser state after one read of the jobs and the holds; the two reads run together. Changes nothing until the caller assigns it. */
async function fetchState(more = false): Promise<BrowserState> {
  const [page, holds] = await Promise.all([
    invoke<JobPage>('list_scheduled_jobs', { includeHistory: st.includeHistory, cursor: more ? st.nextCursor : null }),
    invoke<JobHold[]>('list_scheduled_job_holds').catch(() => [] as JobHold[]),
  ]);
  const jobs = more ? [...st.jobs, ...page.jobs] : page.jobs;
  return { ...st, jobs, holds, nextCursor: page.next_cursor ?? null, at: Math.min(st.at, Math.max(0, jobs.length - 1)), armed: null };
}

async function load(more = false): Promise<void> {
  st = await fetchState(more);
}

export function openSchedules(): Promise<void> {
  return loadView({
    key: 'admin-schedules', title: 'Scheduled jobs', failed: 'Failed to load scheduled jobs',
    read: () => fetchState(),
    open: (next) => { st = next; buildSchedules(); },
  });
}

function buildSchedules(): void {
  const dlg = adminDialog('admin-schedules-dialog', 'admin-schedules');
  const box = el('div', 'launch-form');
  const title = el('h2');
  const ul = listBox('Scheduled jobs');
  const empty = note(emptyText('admin-schedules'));
  const info = note();
  const err = errorLine();
  const newBtn = button('New (n)');
  const editBtn = button('Edit (e)');
  const toggleBtn = button('Enable/disable (Space)');
  const runBtn = button('Run now (t)');
  const delBtn = button('Delete (dd)', 'btn danger');
  const histBtn = button('History (H)');
  const moreBtn = button('More (m)');
  const refreshBtn = button('Refresh (r)');
  const closeBtn = button('Close');
  box.append(title, ul, empty, info, err,
    actions(closeBtn, refreshBtn, histBtn, moreBtn, newBtn, editBtn, toggleBtn, runBtn, delBtn),
    note('j/k move, n new, Enter/e edit, Space enable or disable, t run now, dd delete, r refresh, H history, m more, q/Esc close, ? help.'));

  const selectedJob = (): ScheduledJob | undefined => st.jobs[st.at];
  const draw = (): void => {
    const now = Date.now();
    title.textContent = `Scheduled jobs${st.includeHistory ? ' (with history)' : ''}`;
    empty.hidden = st.jobs.length > 0;
    ul.hidden = st.jobs.length === 0;
    ul.replaceChildren(...st.jobs.map((j, i) => {
      const r = jobRow(j, st.holds, now);
      const li = rowEl(`${r.mark} ${r.name}`, r.detail, i === st.at, j.enabled ? '' : 'admin-off');
      li.addEventListener('click', () => { st = { ...st, at: i, armed: null }; draw(); });
      li.addEventListener('dblclick', () => openForm(editScheduleForm(j)));
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    syncListbox(ul, dlg);
    const job = selectedJob();
    info.textContent = st.armed && st.armed === job?.id ? `Press d again to delete "${job.name}".` : job ? job.message : '';
    for (const b2 of [editBtn, toggleBtn, runBtn, delBtn]) b2.disabled = !job;
    toggleBtn.textContent = job?.enabled ? 'Disable (Space)' : 'Enable (Space)';
    moreBtn.disabled = !st.nextCursor;
    histBtn.textContent = st.includeHistory ? 'Active only (H)' : 'History (H)';
  };
  const run = async (what: string, fn: () => Promise<unknown>): Promise<void> => {
    err.hidden = true;
    try {
      await fn();
      await load();
      draw();
    } catch (e) {
      showError(err, `${what} failed: ${String(e)}`);
    }
  };
  const toggle = (): void => {
    const job = selectedJob();
    if (job) void run('Toggle', async () => {
      const r = await invoke<{ enabled?: boolean }>('toggle_scheduled_job', { id: job.id });
      toast(`${job.name}: ${r?.enabled ? 'enabled' : 'disabled'}`, true);
    });
  };
  const trigger = (): void => {
    const job = selectedJob();
    if (job) void run('Run', async () => { await invoke('trigger_scheduled_job', { id: job.id }); toast(`Triggered: ${job.name}`, true); });
  };
  const del = (): void => {
    const step = deleteChord(st.armed, selectedJob()?.id);
    st = { ...st, armed: step.armed };
    if (!step.run) { draw(); return; }
    const id = step.run;
    void run('Delete', async () => { await invoke('delete_scheduled_job', { id }); toast('Scheduled job deleted', true); });
  };
  const openForm = (f: ScheduleFormState): void => { dlg.close(); void openScheduleForm(f); };

  newBtn.addEventListener('click', () => openForm(newScheduleForm(new Date())));
  editBtn.addEventListener('click', () => { const j = selectedJob(); if (j) openForm(editScheduleForm(j)); });
  toggleBtn.addEventListener('click', toggle);
  runBtn.addEventListener('click', trigger);
  delBtn.addEventListener('click', del);
  histBtn.addEventListener('click', () => { st = { ...st, includeHistory: !st.includeHistory, at: 0 }; void run('Refresh', async () => {}); });
  moreBtn.addEventListener('click', () => void loadMore());
  refreshBtn.addEventListener('click', () => void run('Refresh', async () => {}));
  closeBtn.addEventListener('click', () => dlg.close());
  const loadMore = async (): Promise<void> => {
    if (!st.nextCursor) { toast('No more scheduled jobs'); return; }
    try { await load(true); draw(); } catch (e) { showError(err, `Load more failed: ${String(e)}`); }
  };

  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (inField(ev) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const moved = moveCursor(st.at, st.jobs.length, ev.key);
    if (moved !== null) { ev.preventDefault(); st = { ...st, at: moved, armed: null }; draw(); return; }
    const k = ev.key;
    const hit = (fn: () => void): void => { ev.preventDefault(); fn(); };
    if (k === 'n') hit(() => openForm(newScheduleForm(new Date())));
    else if (k === 'Enter' || k === 'e') hit(() => { const j = selectedJob(); if (j) openForm(editScheduleForm(j)); });
    else if (k === ' ') hit(toggle);
    else if (k === 't') hit(trigger);
    else if (k === 'd') hit(del);
    else if (k === 'r') hit(() => void run('Refresh', async () => {}));
    else if (k === 'H') hit(() => { st = { ...st, includeHistory: !st.includeHistory, at: 0 }; void run('Refresh', async () => {}); });
    else if (k === 'm') hit(() => void loadMore());
    else if (k === 'q' || k === 'Escape') hit(() => dlg.close());
    // `?` is contextual help, as in the TUI (#321); Ctrl-Alt-G reaches the same table.
    else if (k === '?') hit(() => void import('./keyhelp.js').then((h) => import('./commands.js').then((c) => h.openKeyHelp(c.snapshot()))));
  };
  draw();
  show(dlg, box);
}

async function openScheduleForm(f: ScheduleFormState): Promise<void> {
  const editing = f.editingId !== null;
  const dlg = adminDialog('admin-schedule-form-dialog', 'admin-schedules');
  const form = el('form', 'launch-form');
  const name = textField('Name', f.name);
  const message = textField('Message (the prompt the job launches)', f.message);
  const recWrap = el('label', undefined, 'Recurrence (Left/Right)');
  const rec = el('select');
  rec.append(...RECURRENCES.map((r, i) => { const o = el('option', undefined, r.label); o.value = String(i); return o; }));
  rec.value = String(f.recurrence);
  recWrap.append(rec);
  const interval = textField('Interval', f.interval);
  interval.input.inputMode = 'numeric';
  const date = textField('Anchor date (UTC, YYYY-MM-DD)', f.date);
  const time = textField('Anchor time (UTC, HH:MM:SS)', f.time);
  const row1 = el('div', 'row');
  row1.append(recWrap, interval.wrap);
  const row2 = el('div', 'row');
  row2.append(date.wrap, time.wrap);
  form.append(el('h2', undefined, editing ? 'Edit scheduled job' : 'New scheduled job'), name.wrap, message.wrap, row1, row2);

  // Create only: the TUI takes these from its cwd and launch selection.
  const dir = textField('Working directory (empty: the daemon default)', f.workingDir, { placeholder: '/path/to/repo' });
  const providerWrap = el('label', undefined, 'Provider (empty: the daemon default)');
  const provider = el('select');
  const model = textField('Model (empty: the provider default)', f.model);
  if (!editing) {
    provider.append(Object.assign(el('option', undefined, '—'), { value: '' }));
    const providers = await api.providers().catch(() => [] as string[]);
    for (const p of providers) provider.append(Object.assign(el('option', undefined, p), { value: p }));
    provider.value = f.provider;
    providerWrap.append(provider);
    const row3 = el('div', 'row');
    row3.append(providerWrap, model.wrap);
    form.append(dir.wrap, row3);
  }
  const err = errorLine();
  const cancel = button('Cancel (Esc)');
  const save = button('Save (Enter)', 'btn primary', 'submit');
  form.append(err, actions(cancel, save), note('Tab / Shift-Tab move between fields, Left / Right on Recurrence cycle it, Enter saves, Esc returns to Scheduled jobs.'));

  const syncInterval = (): void => { interval.input.disabled = rec.value === '0'; };
  rec.addEventListener('change', syncInterval);
  rec.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'ArrowLeft' || ev.key === 'ArrowRight') {
      ev.preventDefault();
      rec.value = String(cycleRecurrence(Number(rec.value), ev.key === 'ArrowRight' ? 1 : -1));
      syncInterval();
    }
  });
  syncInterval();
  const back = (): void => { dlg.close(); void openSchedules(); };
  const current = (): ScheduleFormState => ({
    ...f, name: name.input.value, message: message.input.value, recurrence: Number(rec.value), interval: interval.input.value,
    date: date.input.value, time: time.input.value, workingDir: dir.input.value, provider: provider.value, model: model.input.value,
  });
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const r = scheduleRequest(current());
    if (!r.ok) { showError(err, r.error); return; }
    const call = editing
      ? invoke('update_scheduled_job', { id: f.editingId, form: r.wire })
      : invoke('create_scheduled_job', { form: r.wire });
    call.then(() => { toast(editing ? 'Scheduled job updated' : 'Scheduled job created', true); back(); },
      (e: unknown) => showError(err, `${editing ? 'Update' : 'Create'} failed: ${String(e)}`));
  });
  cancel.addEventListener('click', back);
  dlg.onkeydown = (ev) => { if (focusOwnsKey(ev.key, ev.target as Element | null)) return; if (ev.key === 'Escape') { ev.preventDefault(); back(); } };
  show(dlg, form);
  name.input.focus();
}
