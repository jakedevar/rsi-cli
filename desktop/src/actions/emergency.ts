// Emergency stop, DOM side: ask (menus, palette, buttons), call the
// `emergency_stop_all` Tauri command, toast the TUI's message and mark the
// interrupted sessions HARD in the session list. The rules are in
// src/emergency-model.ts (pure, tested) and the registry entry in emergency-spec.ts.
import { invoke } from '../api.js';
import { confirmDialog, forgetPause } from '../approvals.js';
import type { Invocation, SliceRuntime } from '../commands.js';
import { $, toast } from '../dom.js';
import {
  type LocalPauses, NO_PAUSES, STOP_CONFIRM_BODY, STOP_CONFIRM_TITLE, applyStopReport, dropPause, parseStopReport,
  prunePauses, stopMessage, stopNeedsConfirm,
} from '../emergency-model.js';
import { refreshSessions, renderHeader } from '../sessions.js';
import { onUiChange, selected, state } from '../state.js';
import { type EmergencyActionId, SLICE } from './emergency-spec.js';

let pauses: LocalPauses = NO_PAUSES;
let stopping = false;

/** The sessions this app marked HARD with an emergency stop (for tests and the badge). */
export function localHardPauses(): LocalPauses {
  return pauses;
}

async function emergencyStop(inv: Invocation): Promise<void> {
  if (stopping) return;
  if (stopNeedsConfirm(inv.source)) {
    const ok = await confirmDialog(`${STOP_CONFIRM_TITLE} ${STOP_CONFIRM_BODY}`, 'Stop all');
    if (!ok) return;
  }
  stopping = true;
  try {
    const report = parseStopReport(await invoke<unknown>('emergency_stop_all'));
    if (!report) { toast('Failed to apply emergency stop: the daemon sent no report'); return; }
    pauses = applyStopReport(pauses, report);
    toast(stopMessage(report), true);
    forgetPause();
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
    badge.textContent = 'HARD';
    badge.title = 'Hard paused by the emergency stop';
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
  init: () => onUiChange(decorateRows),
  initOrder: 45,
};
