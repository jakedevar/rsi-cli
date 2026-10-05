// Entry point: wires DOM events and owns the polling timers. Feature code
// lives in its own module (sessions, conversation, actions, launch).
import { interrupt, sendMessage } from './actions.js';
import { initApprovals } from './approvals.js';
import { refreshEvents } from './conversation.js';
import { $, toast } from './dom.js';
import { startLive } from './live.js';
import { wireMarkdownEvents } from './markdown.js';
import { openLaunch, submitLaunch } from './launch.js';
import { refreshHealth, refreshProjects, refreshSessions, renderSessionList, select, setView } from './sessions.js';
import { selected, state } from './state.js';

const HEALTH_POLL_MS = 5000;

function wire(): void {
  initApprovals({ toast, refresh: () => void refreshSessions(), selected });
  $('session-list').addEventListener('click', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.session-item');
    if (li?.dataset.id) select(li.dataset.id);
  });
  for (const id of ['project-filter', 'show-finished']) $(id).addEventListener('change', renderSessionList);
  $('search').addEventListener('input', renderSessionList);
  $('composer').addEventListener('submit', (ev) => { ev.preventDefault(); void sendMessage(); });
  $('message').addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) { ev.preventDefault(); void sendMessage(); }
  });
  wireMarkdownEvents($('events'));
  $('interrupt').addEventListener('click', () => void interrupt());
  $('toggle-tools').addEventListener('click', () => {
    state.hideTools = !state.hideTools;
    $('events').classList.toggle('hide-tools', state.hideTools);
    $('toggle-tools').textContent = state.hideTools ? 'Show tools' : 'Hide tools';
  });
  $('view-active').addEventListener('click', () => void setView('active'));
  $('view-archived').addEventListener('click', () => void setView('archived'));
  $('new-session').addEventListener('click', () => void openLaunch());
  $('launch-cancel').addEventListener('click', () => $<HTMLDialogElement>('launch-dialog').close());
  $('launch-form').addEventListener('submit', (ev) => void submitLaunch(ev as SubmitEvent));
}

async function boot(): Promise<void> {
  wire();
  await refreshHealth();
  await refreshProjects();
  await refreshSessions();
  window.setInterval(() => void refreshHealth(), HEALTH_POLL_MS);
  startLive({
    refreshSessions,
    refreshEvents: () => refreshEvents(),
    selectedId: () => state.selectedId,
    onLink: () => void refreshHealth(),
  });
  window.setInterval(() => void refreshProjects(), 60_000);
}

void boot();
