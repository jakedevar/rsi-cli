// Typed adapter over the settings Tauri commands (src-tauri/src/settings.rs and
// settings_state.rs): `GetDaemonConfig` (credential-like values redacted), one
// atomic `UpdateDaemonConfig {updates}` per apply, and the model-control mode,
// which rsid keeps outside the config (`GetModelControlStatus` /
// `UpdateModelControlPolicy`): the pane reads it into the config under
// `model_control_mode` so the row stages like the others.
import { invoke } from './api.js';
import type { Config, FieldUpdate, SettingsClient } from './settings-model.js';
import { MODE_FIELD } from './settings-model.js';
import { modeOfStatus } from './settings-state-model.js';

export const settingsApi: SettingsClient = {
  getConfig: async () => {
    const config = await invoke<Config>('get_daemon_config');
    try {
      const mode = modeOfStatus(await invoke<unknown>('model_control_overview'));
      return mode ? { ...config, [MODE_FIELD]: mode } : config;
    } catch {
      // The row shows "unavailable"; every other row stays usable.
      return config;
    }
  },
  update: (updates: readonly FieldUpdate[]) => invoke<unknown>('update_daemon_config', { updates }),
  setModelControlMode: (mode: string) => invoke<unknown>('set_model_control_mode', { mode }),
};
