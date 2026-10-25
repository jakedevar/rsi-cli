// The status card for a Failed or Interrupted open session (local #225): the live glue.
// The text rules are in session-outcome-model.ts, the drawing in session-outcome-view.ts.
import { aiStopMode, localHardPauses } from './actions/emergency.js';
import { copyText, dispatch } from './commands.js';
import { openLaunch } from './launch.js';
import { currentPlatform, isVimKeysOn } from './vimkeys.js';
import { isReadOnly, selected, state } from './state.js';
import { launchPrefill, outcomeCard } from './session-outcome-model.js';
import { drawOutcomeCard } from './session-outcome-view.js';

let drawnKey = '';

/** Redraw the card for the open session; a card that has not changed is left alone (details stay open). */
export function renderOutcome(): void {
  const host = document.getElementById('outcome-card');
  if (!host) return;
  const s = selected();
  const card = s && !isReadOnly(s)
    ? outcomeCard(
      s, state.events,
      { aiStopMode: aiStopMode(), stoppedByEmergency: localHardPauses().has(s.id) },
      isVimKeysOn(), currentPlatform(),
    )
    : null;
  const key = card?.key ?? '';
  if (key === drawnKey) return;
  drawnKey = key;
  drawOutcomeCard(host, card, {
    step: (id) => {
      const open = selected();
      if (id === 'continue') dispatch('focus-composer', { source: 'button', arg: 'i' });
      else if (id === 'login') dispatch('admin-provider-keys', { source: 'button' });
      else if (open) void openLaunch(launchPrefill(open));
    },
    copyError: (text) => { void copyText(text, 'the error message'); },
  });
}
