// Why a finished session ended, in plain words (local #225, from the #221 audit).
//
// A Failed or Interrupted session used to show only the status word. The daemon already sends
// `terminal_reason` (how the run ended) and `stop_reason` (the model's own stop cause), and the
// conversation holds the last error event; the TUI shows all three
// (crates/rsi/src/ui/session.rs `terminal_reason_span`, `last_error_event_span`, `is_error_event`).
// This turns them into a status card: what happened in a sentence, the last error's first line,
// the raw values under Details, and next steps that name a visible button first and the Vim key
// second (empty-states.ts / key-hints.ts `nextStep`). No DOM; tests/session-outcome.test.mjs.

import { RESUME_LABEL } from './emergency-model.js';
import { redactCredentials } from './errors-model.js';
import { type NextStep, hintLine } from './key-hints.js';
import type { Platform } from './keyengine.js';

export interface OutcomeSession {
  id: string;
  status: string;
  provider: string;
  model?: string | null;
  project_id?: string | null;
  query: string;
  terminal_reason?: string | null;
  stop_reason?: string | null;
}

export interface OutcomeEvent {
  event_type: string;
  content: string;
  metadata?: unknown;
}

/** The TUI's cap on the error line (`take(120)`). */
export const ERROR_LINE_CHARS = 120;

const clean = (s: string | null | undefined): string => (s ?? '').trim();

// ---------- error events ----------

function metaIsError(metadata: unknown): boolean | null {
  let m = metadata;
  if (typeof m === 'string') {
    try { m = JSON.parse(m); } catch { return null; }
  }
  if (m && typeof m === 'object' && typeof (m as { is_error?: unknown }).is_error === 'boolean') {
    return (m as { is_error: boolean }).is_error;
  }
  return null;
}

/** The TUI's `is_error_event`: `metadata.is_error` when present, else a System event that says error or failed. */
export function isErrorEvent(e: OutcomeEvent): boolean {
  const flag = metaIsError(e.metadata);
  if (flag !== null) return flag;
  if (e.event_type !== 'System') return false;
  const t = e.content.toLowerCase();
  return t.includes('error') || t.includes('failed');
}

export interface LastError {
  /** The first line, at most 120 characters, credentials redacted. */
  line: string;
  /** The whole event text (redacted), what Copy puts on the clipboard. */
  full: string;
}

/** The newest error event of the conversation, or null. */
export function lastError(events: readonly OutcomeEvent[]): LastError | null {
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i]!;
    if (!isErrorEvent(e)) continue;
    const full = redactCredentials(e.content.trim());
    if (full === '') continue;
    const first = full.split('\n', 1)[0]!.trim();
    return { line: [...first].slice(0, ERROR_LINE_CHARS).join(''), full };
  }
  return null;
}

// ---------- reasons in plain words ----------

/** What a `terminal_reason` means to a person, or null when it is not one we know. */
export function terminalSentence(reason: string): string | null {
  const r = reason.toLowerCase();
  switch (r) {
    case 'completed': return 'It ended normally.';
    case 'budget_exhausted': return 'It ran out of its budget.';
    case 'aborted_streaming': return 'Its reply was cut off while it was being written.';
    case 'aborted_tools': return 'It was stopped while it was running tools.';
    case 'api_error': return 'The AI provider returned an error.';
    case 'provider_tool_rejected': return 'The provider rejected a tool call and the turn ended. Work may be left in its folder.';
    case 'provider_model_unrecognized': return 'The provider did not recognise the model this session asked for.';
    case 'max_turns': return 'It reached its limit on turns.';
    case 'model_error': return 'The model reported an error.';
    case 'blocking_limit': return 'The provider’s usage limit blocked it.';
    case 'prompt_too_long': return 'The conversation became too long for the model.';
    case 'rapid_refill_breaker': return 'It was stopped because it kept running out of room straight away.';
    case 'stop_hook_prevented':
    case 'hook_stopped': return 'A hook stopped it.';
    default: break;
  }
  if (r === 'terminal_failure:store-desync') {
    return 'RSI lost track of it: it was marked running, but its process was gone (the background service restarted or stopped).';
  }
  if (r === 'interrupted:daemon_restart') return 'RSI’s background service restarted while it was running.';
  if (r === 'provider_error:codex_usage_limit') return 'The provider’s usage limit was reached.';
  if (r.startsWith('provider_error')) return 'The AI provider reported an error.';
  if (r.startsWith('bench_start_failed')) return 'It could not be started.';
  return null;
}

/** What a `stop_reason` (the model’s own stop cause for the last turn) means; null when unknown. */
export function stopSentence(reason: string): string | null {
  switch (reason.toLowerCase()) {
    case 'end_turn':
    case 'stop':
    case 'completed': return 'the model finished its turn';
    case 'max_tokens':
    case 'length': return 'the reply hit the length limit';
    case 'tool_use': return 'the model was waiting on a tool call';
    case 'refusal': return 'the model declined the request';
    case 'stop_sequence': return 'the model reached a stop sequence';
    default: return null;
  }
}

/** A reason or error that names a missing login, key or program: the likely first failure for a newcomer. */
const LOGIN_HINT =
  /\b(not logged in|log ?in|logged out|unauthori[sz]ed|authenticat\w*|api[ _-]?key|invalid[ _-]?(?:token|key|credentials?)|credentials?|401|403|command not found|not installed|executable not found|binary not found|no such file or directory|enoent|failed to (?:spawn|start))\b/i;

export function namesLogin(...texts: ReadonlyArray<string | null | undefined>): boolean {
  return texts.some((t) => !!t && LOGIN_HINT.test(t));
}

// ---------- the card ----------

export type OutcomeStepId = 'continue' | 'new-like' | 'login';

export interface OutcomeStep {
  id: OutcomeStepId;
  /** The button text. */
  label: string;
  /** The tooltip. */
  help: string;
  /** The Vim key, shown secondary. */
  key?: string;
}

export interface OutcomeContext {
  /** The model-control mode slug while it is not normal (`stop-all`, ...), else null. */
  aiStopMode?: string | null;
  /** This app marked the session HARD with an Emergency stop. */
  stoppedByEmergency?: boolean;
}

export interface OutcomeCard {
  tone: 'failed' | 'interrupted' | 'note';
  /** "This session failed." */
  title: string;
  /** The reason in a plain sentence, plus why the model stopped; null when the daemon gave none. */
  reason: string | null;
  error: LastError | null;
  steps: readonly OutcomeStep[];
  /** The steps as one line (Vim keys on wording; the key-hints pass handles off), or the way out of a stop. */
  nextLine: string;
  /** The raw values, for the Details section; null when there are none. */
  detail: string | null;
  /** Equal cards draw once (the page re-renders on every poll). */
  key: string;
}

const CONTINUE: OutcomeStep = {
  id: 'continue', label: 'Continue with a message', key: 'i',
  help: 'Move to the message box. Sending a message continues this session.',
};
// No key: Cmd-N opens an empty New session, not this prefilled one (#305), so the card names none.
const NEW_LIKE: OutcomeStep = {
  id: 'new-like', label: 'New session like this one',
  help: 'Open New session with this session’s request, provider, model and project filled in.',
};
const LOGIN: OutcomeStep = {
  id: 'login', label: 'Check provider login',
  help: 'Open Settings › Provider keys to see whether this provider is logged in or has a key.',
};

const stepOf = (s: OutcomeStep): NextStep => ({ click: s.label, ...(s.key ? { key: s.key } : {}) });

const TITLES: Record<string, string> = {
  Failed: 'This session failed.',
  Interrupted: 'This session was interrupted.',
};

/** Is `status` one the card describes: Failed, Interrupted, or Completed with an unusual reason. */
export function wantsCard(s: Pick<OutcomeSession, 'status' | 'terminal_reason'>): boolean {
  if (s.status === 'Failed' || s.status === 'Interrupted') return true;
  const reason = clean(s.terminal_reason);
  return s.status === 'Completed' && reason !== '' && reason.toLowerCase() !== 'completed';
}

/** Statuses that take a message as a continuation (the composer says so). */
export function isFinishedStatus(status: string): boolean {
  return status === 'Failed' || status === 'Interrupted' || status === 'Completed';
}

/** The card for a session and its conversation, or null for a session that needs none. */
export function outcomeCard(
  s: OutcomeSession,
  events: readonly OutcomeEvent[],
  ctx: OutcomeContext = {},
  vimOn = true,
  platform?: Platform,
): OutcomeCard | null {
  if (!wantsCard(s)) return null;
  const terminal = clean(s.terminal_reason);
  const stop = clean(s.stop_reason);
  const error = s.status === 'Completed' ? null : lastError(events);
  const stopped = ctx.aiStopMode === 'stop-all';

  let title = TITLES[s.status] ?? 'This session finished with a note.';
  const bits: string[] = [];
  if (terminal !== '') bits.push(terminalSentence(terminal) ?? `Reason given: ${terminal}.`);
  if (stop !== '') {
    const why = stopSentence(stop);
    bits.push(why ? `Stopped because ${why}.` : `Stopped because: ${stop}.`);
  }
  if (s.status === 'Interrupted' && ctx.stoppedByEmergency) {
    title = 'This session was stopped by Emergency stop.';
  }
  const reason = bits.length > 0 ? bits.join(' ') : null;

  const rawLines = [
    ...(terminal !== '' ? [`terminal_reason: ${terminal}`] : []),
    ...(stop !== '' ? [`stop_reason: ${stop}`] : []),
  ];
  const detail = rawLines.length > 0 ? redactCredentials(rawLines.join('\n')) : null;

  const steps: OutcomeStep[] = [];
  let nextLine: string;
  if (stopped) {
    // New sessions and messages are refused until the mode is back to normal: the banner is the way out.
    nextLine = `Choose ${RESUME_LABEL} in the banner at the top first, then send a message to continue.`;
  } else if (ctx.stoppedByEmergency) {
    nextLine = 'Choose Resume at the top right to lift the stop, then send a message to continue.';
  } else {
    if (s.status !== 'Completed') steps.push(CONTINUE);
    steps.push(NEW_LIKE);
    if (s.status !== 'Completed' && namesLogin(terminal, stop, error?.full)) steps.push(LOGIN);
    nextLine = hintLine(steps.map(stepOf), vimOn, platform);
  }

  const tone = s.status === 'Failed' ? 'failed' : s.status === 'Interrupted' ? 'interrupted' : 'note';
  const key = JSON.stringify([s.id, s.status, title, reason, error?.line ?? null, steps.map((x) => x.id), nextLine, detail]);
  return { tone, title, reason, error, steps, nextLine, detail, key };
}

/** The card as the plain text a person would read (tests and the Copy-all style callers). */
export function cardText(c: OutcomeCard): string {
  return [c.title, c.reason, c.error ? `Last error: ${c.error.line}` : null, c.nextLine].filter((x) => x).join(' ');
}

/** A new session like this one: the fields the launch dialog fills in. */
export interface LaunchPrefill {
  query: string;
  provider: string;
  model?: string;
  project_id?: string;
}

export function launchPrefill(s: OutcomeSession): LaunchPrefill {
  return {
    query: s.query,
    provider: s.provider,
    ...(s.model ? { model: s.model } : {}),
    ...(s.project_id ? { project_id: s.project_id } : {}),
  };
}
