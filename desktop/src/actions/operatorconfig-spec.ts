// The operator-configuration slice's entries for the action registry (pure, no
// DOM): the hooks, providers and bridges dialogs,
// which the TUI reaches through its Settings tabs and the desktop through the
// View menu and the palette (the desktop binds only commands docs/keybindings.md
// lists). All three open a dialog over the daemon's operator RPCs (hub #3039,
// Desktop 13B). The DOM side
// is actions/operatorconfig.ts, the pure rules src/operator-config-model.ts.
import type { ActionSpec, SliceSpec } from '../registry.js';

export type OperatorConfigActionId = 'open-hooks' | 'open-providers' | 'open-bridges';

/** Always available: the dialogs talk to the daemon, not to a selected session. */
export const OPERATOR_CONFIG_RULES: Readonly<Record<OperatorConfigActionId, () => null>> = {
  'open-hooks': () => null,
  'open-providers': () => null,
  'open-bridges': () => null,
};

export const OPERATOR_CONFIG_ACTIONS: readonly ActionSpec[] = [
  {
    id: 'open-hooks',
    title: 'Claude hooks…',
    group: 'view',
    menuBreak: true,
    detail: 'Edit the hooks block of ~/.claude/settings.json on the daemon host (:hooks opens it too)',
  },
  {
    id: 'open-providers',
    title: 'Custom providers…',
    group: 'view',
    detail: 'OpenAI-compatible providers, their API keys and the model roles routed to them',
  },
  {
    id: 'open-bridges',
    title: 'Message bridges…',
    group: 'view',
    detail: 'Signal and iMessage bridge settings',
  },
];

// `:hooks` is bound by the settings slice (settings-hooks, which opens Settings
// at Claude Hooks); this slice's afterAction opens the hooks list over it, so the
// command has one binding and the TUI's behaviour (the hooks overlay) is kept.

export function isOperatorConfigAction(id: string): id is OperatorConfigActionId {
  return id === 'open-hooks' || id === 'open-providers' || id === 'open-bridges';
}

export const SLICE: SliceSpec<OperatorConfigActionId> = {
  name: 'operator configuration (hub #3039, Desktop 13B)',
  order: 70,
  actions: OPERATOR_CONFIG_ACTIONS,
  rules: OPERATOR_CONFIG_RULES,
  keys: {},
};
