// The settings, themes and help slice's entries for the action registry (pure,
// no DOM; slice 7, hub #3009): the settings pane, the theme picker and role
// editor, and the pane's apply / discard / refresh. The keybindings help
// overlay is the core `help` action (`?`, Ctrl-Alt-G, `:help`); its runtime
// (actions/settings-run.ts) shows the overlay after it runs.
// The registry collects SLICE through the generated actions/index.generated.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionId, ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type SettingsActionId =
  | 'settings-open' | 'settings-hooks' | 'settings-skills'
  | 'settings-apply' | 'settings-discard' | 'settings-refresh'
  | 'theme-picker' | 'theme-roles' | 'theme-reset';

/** What the slice's actions can see (built by actions/settings-run.ts). */
export interface SettingsSnapshot {
  /** The settings pane is open. */
  open: boolean;
  /** Staged daemon edits that differ from the live config. */
  pending: number;
  /** Theme role overrides in effect. */
  overrides: number;
}

type Rule = (s: UiSnapshot) => string | null;

const needPane = (more?: (p: SettingsSnapshot) => string | null): Rule => (s) => {
  if (!s.settings?.open) return 'Open settings first (click Settings in the top bar, or press Ctrl-,)';
  return more ? more(s.settings) : null;
};

export const SETTINGS_RULES: Readonly<Record<SettingsActionId, Rule>> = {
  'settings-open': () => null,
  'settings-hooks': () => null,
  'settings-skills': () => null,
  'settings-apply': needPane((p) => (p.pending > 0 ? null : 'No pending settings changes')),
  'settings-discard': needPane((p) => (p.pending > 0 ? null : 'No pending settings changes')),
  'settings-refresh': needPane(),
  'theme-picker': () => null,
  'theme-roles': () => null,
  'theme-reset': (s) => ((s.settings?.overrides ?? 0) > 0 ? null : 'No theme role is overridden'),
};

export const SETTINGS_ACTIONS: readonly ActionSpec[] = [
  { id: 'settings-open', title: 'Settings…', group: 'view', menuBreak: true, extraKeys: ['Ctrl-,'], detail: 'Theme, workspace, models, safety, automation and host settings (the TUI settings page)' },
  { id: 'settings-hooks', title: 'Hook settings…', group: 'view', noMenu: true, detail: 'Settings at the Claude hooks list' },
  { id: 'settings-skills', title: 'Skill settings…', group: 'view', noMenu: true, detail: 'Settings at the Claude skills list' },
  { id: 'settings-apply', title: 'Apply pending settings', group: 'view', noMenu: true, detail: 'One daemon update with every staged value (w in the settings pane)' },
  { id: 'settings-discard', title: 'Discard pending settings', group: 'view', noMenu: true, detail: 'Drop every staged value' },
  { id: 'settings-refresh', title: 'Refresh settings', group: 'view', noMenu: true, detail: 'Re-read the daemon config (R in the settings pane)' },
  { id: 'theme-picker', title: 'Choose theme…', group: 'view', detail: 'The TUI built-in themes; the preview is live and Esc rolls back' },
  { id: 'theme-roles', title: 'Edit theme roles…', group: 'view', detail: 'Override a semantic colour role with a contrast check' },
  { id: 'theme-reset', title: 'Reset theme role overrides', group: 'view', detail: 'Clear every role override; the theme stays' },
];

type Bindings = Readonly<Record<string, DesktopBinding<ActionId>>>;
const b = (action: ActionId): DesktopBinding<ActionId> => ({ action });

/** TUI Normal-mode keys the slice runs (keybindings.md notation). */
export const SETTINGS_KEY_BINDINGS: Bindings = {
  '<Space>,': b('settings-open'),
  T: b('theme-picker'),
};

/** TUI `:` commands (canonical names) the slice runs. */
export const SETTINGS_COMMAND_BINDINGS: Bindings = {
  set: b('settings-open'),
  hooks: b('settings-hooks'),
  skills: b('settings-skills'),
  theme: b('theme-picker'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The settings pane and the theme (slice 7, actions/settings-run.ts). */
    settings?: SettingsSnapshot;
  }
}

export const SLICE: SliceSpec<SettingsActionId> = {
  name: 'settings (slice 7, hub #3009)',
  order: 60,
  actions: SETTINGS_ACTIONS,
  rules: SETTINGS_RULES,
  keys: SETTINGS_KEY_BINDINGS,
  commands: SETTINGS_COMMAND_BINDINGS,
};
