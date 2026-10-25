// The DOM side of the prompt creator slice (actions/promptcreator.ts): the
// prompt creator (prompt-creator.ts) and the Claude skills list (skills.ts, hub
// #3062). Collected by actions/runtime.generated.ts.
//
// `:skills` (the settings slice's settings-skills) opens Settings at Claude
// Skills; afterAction then opens the skills list on top, and Enter on that
// Settings row does the same (registerRowOverlay). The hooks, providers and
// bridges Settings rows belong to actions/operatorconfig.ts.
import type { Invocation, SliceRuntime } from '../commands.js';
import type { ActionId } from '../registry.js';
import { SKILL_ROW_KEYS, openSkills, skillsOpen } from '../skills.js';
import { PROMPT_CREATOR_CSS, initPromptCreator, isPromptCreatorOpen, togglePromptCreator } from '../prompt-creator.js';
import { registerRowOverlay } from '../settings.js';
import { type PromptCreatorActionId, type PromptCreatorSnapshot, SLICE } from './promptcreator.js';

export function runPromptCreator(id: PromptCreatorActionId, _inv: Invocation): void {
  switch (id) {
    case 'prompt-creator': void togglePromptCreator(); return;
  }
}

export function promptCreatorSnapshot(): PromptCreatorSnapshot {
  return { open: isPromptCreatorOpen() ? 'prompt-creator' : skillsOpen() };
}

function afterAction(id: ActionId): void {
  if (id === 'settings-skills') void openSkills();
}

function initPromptCreatorSlice(): void {
  const style = document.createElement('style');
  style.id = 'prompt-creator-css';
  style.textContent = PROMPT_CREATOR_CSS;
  document.head.append(style);
  initPromptCreator();
  registerRowOverlay('claude_skills', {
    enter: 'Opens the skills list (Enter previews SKILL.md, e enables or disables, d d deletes)',
    stored: "the daemon host's Claude skills folder",
    keys: SKILL_ROW_KEYS,
    run: () => void openSkills(),
  });
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runPromptCreator(id as PromptCreatorActionId, inv),
  snapshot: () => ({ promptcreator: promptCreatorSnapshot() }),
  afterAction,
  init: initPromptCreatorSlice,
  initOrder: 62,
};
