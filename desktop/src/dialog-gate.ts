// Installs the overlay dialog key gate (modal-gate.ts dialogKeyBlocked) on window in
// the capture phase. main.ts imports this module SECOND, right after the activation
// guard, so ES module evaluation order registers it before any dialog module's
// listener (hub #3101 follow-up, local #111). The Vim keys switch is bound by
// initVimKeys (bindDialogGate); until then the gate lets every key through.
import { installDialogGate } from './modal-gate.js';

let vimKeysOn: () => boolean = () => true;

/** Point the gate at the Vim keys switch (vimkeys.ts isVimKeysOn). */
export function bindDialogGate(source: () => boolean): void {
  vimKeysOn = source;
}

installDialogGate(window, () => vimKeysOn());
