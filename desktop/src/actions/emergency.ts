// Emergency stop, DOM side: ask (menus, palette, buttons), call the
// `emergency_stop_all` Tauri command, toast the TUI's message and mark the
// interrupted sessions HARD in the session list. The rules are in
// src/emergency-model.ts (pure, tested) and the registry entry in emergency-spec.ts.
import { invoke } from '../api.js';
import { confirmDialog, forgetPause } from '../approvals.js';
import type { Invocation, SliceRuntime } from '../commands.js';
import { $, el, renderMessage, toast } from '../dom.js';
import {
  type LocalPauses, NO_PAUSES, RESUME_LABEL, STOPPED_BADGE_LABEL, STOPPED_BADGE_TIP, STOP_CONFIRM_BODY, STOP_CONFIRM_TITLE,
  applyStopReport, dropPause, parseStopReport, prunePauses, stopNeedsConfirm, stopToast,
} from '../emergency-model.js';
import { type BannerView, createModeWatch } from '../mode-watch-model.js';
import { renderOutcome } from '../session-outcome.js';
import { refreshSessions, renderHeader } from '../sessions.js';
import { onUiChange, selected, state } from '../state.js';
import { type EmergencyActionId, SLICE } from './emergency-spec.js';

let pauses: LocalPauses = NO_PAUSES;
let stopping = false;
/** How often the mode is read again, so a change from the TUI or Settings shows up here too. */
const MODE_POLL_MS = 5000;

/** Draw the "AI work is stopped" banner (null hides it). Text only, never HTML. */
function renderBanner(view: BannerView | null): void {
  const node = document.getElementById('ai-banner');
  if (!node) return;
  node.hidden = view === null;
  renderOutcome(); // a stopped session's card points at this banner while it shows
  if (!view) { node.replaceChildren(); return; }
  const btn = el('button', 'btn small primary', view.busy ? 'Resuming…' : RESUME_LABEL);
  btn.type = 'button';
  btn.disabled = view.busy;
  btn.title = 'Let new sessions and messages start again (the same switch as Settings › Safety & Spend › Model Control)';
  btn.addEventListener('click', () => void watch.resume());
  node.replaceChildren(el('span', 'ai-banner-text', view.text), btn);
  if (view.error) {
    const err = el('div', 'ai-banner-error');
    renderMessage(err, view.error);
    node.append(err);
  }
}

const watch = createModeWatch({
  readStatus: () => invoke<unknown>('model_control_overview'),
  setMode: (slug) => invoke<unknown>('set_model_control_mode', { mode: slug }),
  confirm: (title, message, label) => confirmDialog(message, label, { title, primary: true }),
  toast,
  render: renderBanner,
  afterResume: async () => { await refreshSessions(); renderHeader(); },
});

/** The sessions this app marked HARD with an emergency stop (for tests and the badge). */
export function localHardPauses(): LocalPauses {
  return pauses;
}

/** The model-control mode slug while the banner shows (`stop-all`, ...), else null. */
export function aiStopMode(): string | null {
  return watch.view()?.mode ?? null;
}

async function emergencyStop(inv: Invocation): Promise<void> {
  if (stopping) return;
  if (stopNeedsConfirm(inv.source)) {
    const ok = await confirmDialog(STOP_CONFIRM_BODY, 'Stop all', { title: STOP_CONFIRM_TITLE });
    if (!ok) return;
  }
  stopping = true;
  try {
    const report = parseStopReport(await invoke<unknown>('emergency_stop_all'));
    if (!report) { toast('Failed to apply emergency stop: the daemon sent no report'); return; }
    pauses = applyStopReport(pauses, report);
    toast(stopToast(report), true);
    forgetPause();
    await watch.refresh();
    await refreshSessions();
    renderHeader();
  } catch (e) {
    toast(`Failed to apply emergency stop: ${String(e)}`);
  } finally {
    stopping = false;
  }
}

/** Show a HARD badge on each session row the stop paused. Rows are rebuilt on every render. */
function decorateRows(): void {
  pauses = prunePauses(pauses, [...state.sessions, ...state.archived].map((s) => s.id));
  for (const li of $('session-list').querySelectorAll<HTMLElement>('.session-item')) {
    const id = li.dataset.id;
    if (!id || !pauses.has(id) || li.querySelector('.pause-badge')) continue;
    const badge = document.createElement('span');
    badge.className = 'pause-badge';
    badge.textContent = STOPPED_BADGE_LABEL;
    badge.title = STOPPED_BADGE_TIP;
    li.append(badge);
  }
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => {
    if ((id as EmergencyActionId) === 'emergency-stop') void emergencyStop(inv);
  },
  // Clearing or downgrading a pause also clears the local HARD marker.
  afterAction: (id) => {
    if (id !== 'clear-pause' && id !== 'downgrade-pause') return;
    const open = selected();
    if (open) pauses = dropPause(pauses, open.id);
  },
  init: () => {
    onUiChange(decorateRows);
    void watch.refresh();
    window.setInterval(() => void watch.refresh(), MODE_POLL_MS);
  },
  initOrder: 45,
};
