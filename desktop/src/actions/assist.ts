// The assist overlays' slice of the action registry (hub #3015 slice 13B):
// memory search (`<Space>M`), the dialectic (`:ask`), the entity-card editor
// (`:card`, `:card user`) and the AI command / AI chat on a draft. Pure (no
// DOM); the DOM side is src/assist.ts behind actions/assist-run.ts. The
// overlays' own keys are in src/assist-model.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type AssistActionId =
  | 'memory-search' | 'dialectic' | 'card' | 'card-user' | 'ai-command' | 'ai-chat';

/** What the assist actions can see (built by assist.ts). */
export interface AssistSnapshot {
  /** The open session's message box holds text (the AI command / chat source). */
  draft: boolean;
}

export const ASSIST_ACTIONS: readonly ActionSpec[] = [
  { id: 'memory-search', title: 'Memory search…', group: 'view', menuBreak: true, detail: 'Search the daemon’s stored memories (<Space>M)' },
  { id: 'dialectic', title: 'Ask a question…', group: 'view', detail: 'The dialectic: ask about accumulated knowledge (:ask; :ask <question> fills it in)' },
  { id: 'card', title: 'Edit project card…', group: 'view', detail: 'Facts injected into new sessions of the current project (:card; :card add <fact>)' },
  { id: 'card-user', title: 'Edit user card…', group: 'view', detail: 'Facts about you injected into every new session (:card user; :card user add <fact>)' },
  {
    id: 'ai-command', title: 'AI command on message text…', group: 'session', menuBreak: true, extraKeys: ['Ctrl-Alt-A'],
    detail: 'Transform the draft with an instruction through rsid’s prompt processor; a to accepts, d discards',
  },
  {
    id: 'ai-chat', title: 'AI chat about message text…', group: 'session', extraKeys: ['Ctrl-Shift-A'],
    detail: 'Ask rsid’s prompt processor about the draft; the draft is not changed',
  },
];

const always = (): null => null;
const draft = (s: UiSnapshot): string | null => (s.assist?.draft ? null : 'Type a message in the message box first');

export const ASSIST_RULES: Readonly<Record<AssistActionId, (s: UiSnapshot) => string | null>> = {
  'memory-search': always,
  dialectic: always,
  card: always,
  'card-user': always,
  'ai-command': draft,
  'ai-chat': draft,
};

const IDS: ReadonlySet<string> = new Set(ASSIST_ACTIONS.map((a) => a.id));
export const isAssistAction = (id: string): id is AssistActionId => IDS.has(id);

/** TUI Normal-mode keys. The AI chords live in text fields (assist.ts), not the engine. */
export const ASSIST_KEY_BINDINGS: Readonly<Record<string, DesktopBinding<AssistActionId>>> = {
  '<Space>M': { action: 'memory-search' },
};

/** TUI `:` commands (canonical names). `:card` takes `user` / `add <fact>` as its argument. */
export const ASSIST_COMMAND_BINDINGS: Readonly<Record<string, DesktopBinding<AssistActionId>>> = {
  ask: { action: 'dialectic' },
  card: { action: 'card' },
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The assist overlays (slice 13B, assist.ts). */
    assist?: AssistSnapshot;
  }
}

export const SLICE: SliceSpec<AssistActionId> = {
  name: 'assist overlays (slice 13B, hub #3015)',
  order: 70,
  actions: ASSIST_ACTIONS,
  rules: ASSIST_RULES,
  keys: ASSIST_KEY_BINDINGS,
  commands: ASSIST_COMMAND_BINDINGS,
};
