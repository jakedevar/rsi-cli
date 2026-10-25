// Every empty state names its next step both ways (local #208, from the #205 audit): a
// visible button or menu path first, then the Vim key in secondary text. The page renders
// the Vim-keys-on wording; the key-hints DOM pass turns it into the off wording when the
// switch is off, so with Vim keys off an empty state still names its button, never a dead
// end. Pure (tests/empty-states.test.mjs); the panels read their line from EMPTY_STATES.
import { type NextStep, nextStep } from './key-hints.js';
import type { Platform } from './keyengine.js';

export interface EmptyStateSpec {
  /** What is (not) there: "No peers." */
  readonly lead: string;
  /** The next step: the button on screen (or the menu path) and its key. */
  readonly step: NextStep;
  /** What the step does, after it: " to add one". */
  readonly tail?: string;
}

/** The empty states of the panels, by panel (the button names match the panel's buttons). */
export const EMPTY_STATES = {
  'admin-peers': { lead: 'No peers.', step: { click: 'Add', key: 'a' }, tail: ' to add one' },
  'admin-links': { lead: 'No links for this peer.', step: { click: 'Add', key: 'a' }, tail: ' to add one' },
  'admin-links-no-peer': { lead: 'Add a peer first.', step: { click: 'Peers, then Add' } },
  'admin-sessions': { lead: 'No cached sessions for this peer.', step: { click: 'Refresh', key: 'r' }, tail: ' to read them again' },
  'admin-schedules': { lead: 'No scheduled jobs.', step: { click: 'New', key: 'n' }, tail: ' to create one' },
  'admin-mcp': { lead: 'No MCP servers.', step: { click: 'Add', key: 'a' }, tail: ' to add one' },
  'assist-facts': { lead: 'No facts yet.', step: { click: 'Add', key: 'a' }, tail: ' to add one' },
  budgets: {
    lead: 'No explicit budget policies: the daemon’s built-in per-scope defaults apply.',
    step: { click: 'Add policy', key: 'a' }, tail: ' to add one',
  },
  'prompts': { lead: 'No prompts yet.', step: { click: 'New', key: 'Ctrl+N' }, tail: ' to create one' },
  'issues-inspector': { lead: 'No Issue selected.', step: { click: 'an Issue in the list', key: 'j / k' }, tail: ' to read it here' },
  'issues-dispatched': { lead: 'No dispatched Issue selected.', step: { click: 'one in the list', key: 'j / k' }, tail: ' to read it here' },
  'graph-node': { lead: 'No node selected.', step: { click: 'a node in the graph' }, tail: ' to see it here' },
} as const satisfies Record<string, EmptyStateSpec>;

export type EmptyStateId = keyof typeof EMPTY_STATES;

/** One spec as a line: "No peers. Click Add (or press a) to add one." (Vim keys on) or "... Click Add to add one." (off). */
export function emptyLine(spec: EmptyStateSpec, vimOn = true, platform?: Platform): string {
  return `${spec.lead} ${nextStep(spec.step, vimOn, platform)}${spec.tail ?? ''}.`;
}

/** A panel's empty-state line as the page renders it (the Vim keys on wording; the key-hints pass handles off). */
export function emptyText(id: EmptyStateId, vimOn = true, platform?: Platform): string {
  return emptyLine(EMPTY_STATES[id], vimOn, platform);
}

/**
 * The last entry of a key-only footer hint (the managers panel): with Vim keys off its letter
 * keys go, so it ends on the way to every action the panel offers, the Actions… menu.
 */
export function moreActionsHint(platform?: Platform): string {
  return `${nextStep({ click: 'Actions…', key: 'Ctrl+K' }, true, platform)} for every action`;
}

/** The same hint spelled for non-Mac keyboards (Ctrl+K). */
export const MORE_ACTIONS_HINT = moreActionsHint();
