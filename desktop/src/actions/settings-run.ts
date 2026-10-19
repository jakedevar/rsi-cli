// The DOM side of the settings, themes and help slice (actions/settings.ts);
// the feature code is settings.ts, themes.ts and keyhelp.ts. Collected by
// actions/runtime.generated.ts.
//
// The keybindings help overlay is the core `help` action (`?`, Ctrl-Alt-G,
// `:help`, Help › Keyboard shortcuts): core opens the command palette, and
// afterAction('help') replaces it with the overlay (the TUI's `?`). The
// palette stays on Ctrl-K, `<Space>;` and `:commands`.
import { type Invocation, type SliceRuntime, snapshot } from '../commands.js';
import { toast } from '../dom.js';
import { openKeyHelp } from '../keyhelp.js';
import { closePalette, isPaletteOpen } from '../palette.js';
import type { ActionId } from '../registry.js';
import {
  applyPending, closeSettings, discardPending, initSettings, isSettingsOpen, openSettings, redrawSettings,
  refreshSettings, settingsPending,
} from '../settings.js';
import { SETTINGS_CSS } from '../settings-css.js';
import { registerStateRows } from '../settings-state.js';
import {
  applyThemeByName, initThemes, onThemeChange, openRoleEditor, openThemePicker, overrideCount, resetRoles,
} from '../themes.js';
import { SLICE, type SettingsActionId, type SettingsSnapshot } from './settings.js';

export function settingsSnapshot(): SettingsSnapshot {
  return { open: isSettingsOpen(), pending: settingsPending(), overrides: overrideCount() };
}

export function runSettings(id: SettingsActionId, inv: Invocation): void {
  switch (id) {
    case 'settings-open': openSettings(); return;
    case 'settings-hooks': openSettings('claude_hooks'); return;
    case 'settings-skills': openSettings('claude_skills'); return;
    case 'settings-apply': void applyPending(); return;
    case 'settings-discard': discardPending(); return;
    case 'settings-refresh': refreshSettings(); return;
    case 'theme-picker':
      if (inv.arg?.trim()) applyThemeByName(inv.arg.trim());
      else openThemePicker();
      return;
    case 'theme-roles': openRoleEditor(); return;
    case 'theme-reset': resetRoles(); return;
  }
}

function afterAction(id: ActionId): void {
  if (id === 'help') {
    if (isPaletteOpen()) closePalette();
    openKeyHelp(snapshot());
    return;
  }
  // Another full-window view replaced the pane.
  if (id === 'issues-open' && isSettingsOpen()) {
    if (settingsPending() > 0) toast('Settings closed; pending settings were discarded');
    closeSettings();
  }
}

export function initSettingsSlice(): void {
  const style = document.createElement('style');
  style.id = 'settings-css';
  style.textContent = SETTINGS_CSS;
  document.head.append(style);
  initThemes();
  initSettings();
  registerStateRows();
  onThemeChange(redrawSettings);
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runSettings(id as SettingsActionId, inv),
  snapshot: () => ({ settings: settingsSnapshot() }),
  afterAction,
  init: initSettingsSlice,
  // Before the session list (10) and the Issues view (20): the pane's capture
  // listener must see its keys first.
  initOrder: 5,
};
