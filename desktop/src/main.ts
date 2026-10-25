// Entry point: wires DOM events and owns the polling timers. Feature code
// lives in its own module (sessions, conversation, actions, launch); toolbar
// buttons, keys, the palette and menus share the action registry (commands.ts).
// FIRST import: the Enter/Space activation guard registers before any other listener.
import './activation-guard.js';
// SECOND import: the overlay dialog key gate (Vim keys off drops dialog letters), still before any dialog listener.
import './dialog-gate.js';
import { sendMessage } from './actions.js';
import { refreshLabels } from './actions/sessionlist-run.js';
import { initApprovals, landOnOpenedSession } from './approvals.js';
import { initDialogFocus } from './dialog-focus.js';
import { initNotify } from './notify.js';
import { bootStep, daemonLink, finishBoot, installBootErrorCapture, onReconnect, renderOnboarding } from './boot.js';
import { wireDaemonStart } from './daemon-start.js';
import type { DaemonLink } from './onboarding-model.js';
import type { BootStep } from './boot-model.js';
import { refreshEvents } from './conversation.js';
import { $, toast } from './dom.js';
import { startLive } from './live.js';
import { wireMarkdownEvents } from './markdown.js';
import { submitLaunch } from './launch.js';
import { initMenus } from './menus.js';
import { initPalette } from './palette.js';
import { startBuildNotice } from './build-notice.js';
import { flushFilter, initShowFinished, onFilterInput, refreshHealth, refreshProjects, refreshSessions, renderHeader, renderSessionList, select, visibleRows, wireConnPill } from './sessions.js';
import { attentionQueue } from './listmodel.js';
import { settingsApi } from './settings-api.js';
import { selected, state } from './state.js';
import { initVimKeys } from './vimkeys.js';
import { initSlices, wireActionButtons } from './commands.js';
import { focusOwnsKey } from './modal-gate.js';
import { initSessionListKeys } from './sessionlist-keys.js';
import { installTabStops, makeScrollRegion } from './tab-stops.js';
import { installDialogNames } from './a11y.js';

const HEALTH_POLL_MS = 5000;

function wire(): void {
  // Every button, link, checkbox and summary is a tab stop in macOS WebKit too (local #242),
  // and the conversation is a focusable region that the arrows and paging keys scroll.
  installTabStops(document.body);
  makeScrollRegion($('events'), 'Conversation');
  // Every dialog is named from its own heading when it opens (local #245).
  installDialogNames(document.body);
  initDialogFocus();
  initApprovals({ toast, refresh: () => void refreshSessions(), selected, redraw: () => { renderSessionList(); renderHeader(); }, waiting: () => attentionQueue(visibleRows()), open: (id) => select(id) });
  // A click on an OS notification opens its session (local #294).
  initNotify((id) => { select(id); landOnOpenedSession(); });
  initVimKeys();
  initPalette();
  wireActionButtons();
  // Every slice's RUNTIME.init (actions/runtime.generated.ts), in initOrder.
  initSlices();
  initSessionListKeys();
  $('session-list').addEventListener('click', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.session-item');
    if (li?.dataset.id) select(li.dataset.id);
  });
  initShowFinished();
  for (const id of ['project-filter', 'show-finished']) $(id).addEventListener('change', renderSessionList);
  $('search').addEventListener('input', onFilterInput);
  // Enter or leaving the box settles a debounced filter at once.
  $('search').addEventListener('change', flushFilter);
  $('search').addEventListener('blur', flushFilter);
  $('composer').addEventListener('submit', (ev) => { ev.preventDefault(); void sendMessage(); });
  $('message').addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) { ev.preventDefault(); void sendMessage(); }
  });
  wireMarkdownEvents($('events'));
  $('launch-cancel').addEventListener('click', () => $<HTMLDialogElement>('launch-dialog').close());
  $('launch-form').addEventListener('submit', (ev) => void submitLaunch(ev as SubmitEvent));
}

/** The data steps of boot, in order. Re-run when rsid comes up after an offline start. */
async function dataSteps(health: DaemonLink): Promise<BootStep[]> {
  const done: BootStep[] = [
    health.kind === 'connected'
      ? { name: 'daemon', ok: true }
      : { name: 'daemon', ok: false, error: health.kind === 'unreachable' ? (health.error ?? 'rsid is not reachable') : health.kind === 'backend-error' ? health.error : 'no answer' },
  ];
  done.push(await bootStep('projects', refreshProjects));
  done.push(await bootStep('labels', refreshLabels));
  done.push(await bootStep('sessions', refreshSessions));
  // Settings reads the daemon config when it opens; read it once here so a
  // failure shows at boot instead of when the user first opens Settings.
  done.push(health.kind === 'connected'
    ? await bootStep('settings', async () => { await settingsApi.getConfig(); })
    : { name: 'settings', ok: false, skipped: true });
  return done;
}

async function boot(): Promise<void> {
  installBootErrorCapture();
  wire();
  wireConnPill();
  void initMenus();
  // Each step reports ok or its error: the result is the page's data-boot
  // attribute, a banner when a step failed, and the process log line.
  // The daemon-down card's Start rsid redraws the card and re-reads the link (local #232).
  wireDaemonStart(renderOnboarding, refreshHealth);
  const health = await refreshHealth();
  const done = await dataSteps(health);
  window.setInterval(() => void refreshHealth(), HEALTH_POLL_MS);
  startBuildNotice();
  const liveStep = await bootStep('live', async () => {
    startLive({
      refreshSessions: async () => { await refreshSessions(); },
      refreshEvents: () => refreshEvents(),
      selectedId: () => state.selectedId,
      onLink: () => void refreshHealth(),
    });
  });
  window.setInterval(() => void refreshProjects(), 60_000);
  window.setInterval(() => void refreshLabels(), 60_000);
  finishBoot([...done, liveStep]);
  // rsid came up after an offline start: redo the data steps so the report is true again.
  onReconnect(() => void dataSteps(daemonLink()).then((again) => finishBoot([...again, liveStep])));
}

void boot();
