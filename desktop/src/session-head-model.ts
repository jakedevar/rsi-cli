// The open session's header in plain words (local #212, from the #205 audit).
//
// The header leads with the session's display name, then its status. The id,
// folder and branch are developer detail: they sit in a collapsed Details row
// and stay copyable. The controls explain themselves: what Stop this turn and
// Pause until I resume do, and how they differ, matching the TUI and daemon:
//   x / :kill  -> InterruptSession pause_level "soft": the turn is cancelled and
//                 the session is marked SOFT; a granted manager may restart it.
//   X          -> InterruptSession pause_level "hard": cancelled and marked HARD,
//                 so only the operator can clear it (Resume = SetOperatorPause none).
// No DOM; unit-tested under tests/sessionhead.test.mjs.

import { sessionLabel, statusLabel } from './model.js';

interface HeadSession {
  id: string;
  status: string;
  provider: string;
  model?: string | null;
  working_dir: string;
  git_branch?: string | null;
  title?: string | null;
  agent_role?: string | null;
  epic_spawn_ordinal?: number | null;
  query: string;
}

export interface HeadMeta {
  /** The display name or role: the first thing the header says. */
  name: string;
  /** The status in plain words. */
  status: string;
  /** Then provider, model and project, as plain values. */
  parts: string[];
}

export function headMeta(s: HeadSession, project: string | undefined): HeadMeta {
  return {
    name: sessionLabel(s as Parameters<typeof sessionLabel>[0]),
    status: statusLabel(s.status),
    parts: [s.provider, s.model ?? 'default model', ...(project ? [project] : [])],
  };
}

export interface HeadDetail {
  label: string;
  value: string;
  /** What the Copy button reports: `Copied <what>`. */
  what: string;
}

/** Developer detail kept out of the primary header, each row copyable. */
export function headDetails(s: HeadSession): HeadDetail[] {
  const rows: HeadDetail[] = [{ label: 'Session ID', value: s.id, what: 'session ID' }];
  rows.push({ label: 'Folder', value: s.working_dir, what: 'folder path' });
  if (s.git_branch) rows.push({ label: 'Branch', value: s.git_branch, what: 'branch name' });
  return rows;
}

/** The short id shown beside the Copy button. */
export function shortSessionId(id: string): string {
  return id.slice(0, 8);
}

/** Button text and help for the two stop controls and Resume (action ids unchanged). */
export const STOP_BUTTON = {
  label: 'Stop this turn',
  help: 'Stops what the agent is doing right now. You can send a message to continue, and a manager you granted may restart it. Pause until I resume holds it longer.',
} as const;

export const PAUSE_BUTTON = {
  label: 'Pause until I resume',
  help: 'Stops the current turn and starts nothing new, not even by a manager, until you choose Resume. Use it to hold a session; Stop this turn only interrupts once.',
} as const;

export const RESUME_BUTTON = {
  label: 'Resume',
  help: 'Lifts the pause so the session can run again.',
} as const;

/** The question Pause until I resume asks from a button, menu or palette. */
export function hardPauseConfirm(name: string): string {
  return `Pause “${name}”? It stops the current turn and starts nothing new until you choose Resume.`;
}

/** Only the key path (X, pressed twice, Vim keys on) has already confirmed. */
export function hardPauseNeedsConfirm(source: string): boolean {
  return source !== 'key';
}

export type HardPauseResult = 'cancelled' | 'sent';

/**
 * Pause until I resume. A key press (X, pressed twice) already confirmed and
 * sends at once; every other source asks first. Cancel sends nothing.
 */
export async function runHardPause(
  o: { name: string; confirmFirst: boolean; confirm: (message: string) => Promise<boolean>; send: () => Promise<void> },
): Promise<HardPauseResult> {
  if (o.confirmFirst && !(await o.confirm(hardPauseConfirm(o.name)))) return 'cancelled';
  await o.send();
  return 'sent';
}
