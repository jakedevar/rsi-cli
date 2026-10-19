// The prompt creator slice's entries for the action registry (pure, no DOM;
// local #88, hub #3040): <Space>gP opens the daemon's prompt files
// (~/.rsi/prompts) over the prompt-file RPCs. The DOM side is
// actions/promptcreator-run.ts (feature code prompt-creator.ts, rules in
// prompt-creator-model.ts). The hooks, custom-provider and bridge dialogs are
// the operator-configuration slice (actions/operatorconfig.ts, hub #3039).
// The registry collects SLICE through the generated actions/index.generated.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionId, ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type PromptCreatorActionId = 'prompt-creator';

/** What the slice's actions can see (built by actions/promptcreator-run.ts). */
export interface PromptCreatorSnapshot {
  /** The slice's overlay that is open, if any. */
  open: string | null;
}

type Rule = (s: UiSnapshot) => string | null;

export const PROMPT_CREATOR_RULES: Readonly<Record<PromptCreatorActionId, Rule>> = {
  'prompt-creator': () => null,
};

export const PROMPT_CREATOR_ACTIONS: readonly ActionSpec[] = [
  { id: 'prompt-creator', title: 'Prompt creator', group: 'view', menuBreak: true, detail: "Write and edit the daemon's prompt files (~/.rsi/prompts); delete moves a prompt to the recoverable trash (<Space>gP)" },
];

type Bindings = Readonly<Record<string, DesktopBinding<ActionId>>>;
const b = (action: ActionId): DesktopBinding<ActionId> => ({ action });

/** TUI Normal-mode keys the slice runs (keybindings.md notation). */
export const PROMPT_CREATOR_KEY_BINDINGS: Bindings = {
  '<Space>gP': b('prompt-creator'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The prompt creator and skills overlays (actions/promptcreator-run.ts). */
    promptcreator?: PromptCreatorSnapshot;
  }
}

export const SLICE: SliceSpec<PromptCreatorActionId> = {
  name: 'prompt creator and skills (local #88, hub #3040 / #3062)',
  order: 71,
  actions: PROMPT_CREATOR_ACTIONS,
  rules: PROMPT_CREATOR_RULES,
  keys: PROMPT_CREATOR_KEY_BINDINGS,
};
