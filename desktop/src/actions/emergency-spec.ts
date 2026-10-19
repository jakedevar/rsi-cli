// The emergency-stop slice's entries for the action registry (pure, no DOM):
// `<Space>X` / `:stopall` / `:stop-all`, the Session menu item and the palette.
// The DOM side is actions/emergency.ts, the pure rules src/emergency-model.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionId, ActionSpec, SliceSpec } from '../registry.js';

export type EmergencyActionId = 'emergency-stop';

/** Always available, like the TUI's `AvailabilitySelector::Always`. */
export const EMERGENCY_RULES: Readonly<Record<EmergencyActionId, () => null>> = {
  'emergency-stop': () => null,
};

export const EMERGENCY_ACTIONS: readonly ActionSpec[] = [
  {
    id: 'emergency-stop',
    title: 'Emergency stop all…',
    group: 'session',
    menuBreak: true,
    detail: 'Deny new model work, cancel active invocations and interrupt running sessions (<Space>X, :stopall)',
  },
];

const b = (action: ActionId): DesktopBinding<ActionId> => ({ action });

/** TUI Normal-mode keys the slice runs (keybindings.md notation). */
export const EMERGENCY_KEY_BINDINGS: Readonly<Record<string, DesktopBinding<ActionId>>> = {
  '<Space>X': b('emergency-stop'),
};

/** TUI `:` commands (canonical name; `stop-all` is its alias). */
export const EMERGENCY_COMMAND_BINDINGS: Readonly<Record<string, DesktopBinding<ActionId>>> = {
  stopall: b('emergency-stop'),
};

export function isEmergencyAction(id: string): id is EmergencyActionId {
  return id === 'emergency-stop';
}

export const SLICE: SliceSpec<EmergencyActionId> = {
  name: 'emergency stop (hub #3008 follow-up)',
  order: 25,
  actions: EMERGENCY_ACTIONS,
  rules: EMERGENCY_RULES,
  keys: EMERGENCY_KEY_BINDINGS,
  commands: EMERGENCY_COMMAND_BINDINGS,
};
