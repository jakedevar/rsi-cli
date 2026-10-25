// Pure helpers for operator attention (pending questions, pause state).
// No DOM; unit-tested under tests/.
import type { QuestionItem, Session } from './api.js';

/** Sent when the operator declines; matches the TUI's DECLINE_RESPONSE. */
export const DECLINE_RESPONSE = '(declined — no preference; use your best judgment)';

/**
 * What that response means to the agent is "I have no preference, decide yourself", never "no". The buttons
 * that send it say so (local #270): the TUI has no separate deny answer, so a user who means "no" picks an
 * option or types it.
 */
export const SKIP_LABEL = 'Let the agent decide';
export const SKIP_TITLE = 'Tells the agent you have no preference, so it uses its own judgment. This is not a "no".';
export const SKIP_NOTE = 'To say no, pick an option or type your answer. "Let the agent decide" gives no preference.';
export const SKIP_TOAST = 'Sent: the agent will decide';

const LIVE = new Set(['Starting', 'Running', 'WaitingApproval']);

export function pendingQuestions(s: Pick<Session, 'pending_question'>): QuestionItem[] {
  return s.pending_question?.questions ?? [];
}

/** True when a live session is blocked on the operator. */
export function needsAttention(s: Pick<Session, 'status' | 'pending_question'>): boolean {
  if (!LIVE.has(s.status)) return false;
  return s.status === 'WaitingApproval' || pendingQuestions(s).length > 0;
}

export function attentionCount(all: Array<Pick<Session, 'status' | 'pending_question'>>): number {
  return all.reduce((n, s) => n + (needsAttention(s) ? 1 : 0), 0);
}

/**
 * The sessions that newly need the operator since the last refresh (local #245): needs attention
 * now, did not in `before`. `before` is null on the first refresh, which only seeds the set (the
 * window title already carries the count at boot). Returns the new id set and the newcomers.
 */
export function newlyNeedingYou<S extends Pick<Session, 'id' | 'status' | 'pending_question'>>(
  before: ReadonlySet<string> | null, all: readonly S[],
): { now: Set<string>; fresh: S[] } {
  const now = new Set<string>();
  const fresh: S[] = [];
  for (const s of all) {
    if (!needsAttention(s)) continue;
    now.add(s.id);
    if (before !== null && !before.has(s.id)) fresh.push(s);
  }
  return { now, fresh };
}

/** The spoken line for sessions that just started waiting: one name, or the count and the first names. */
export function waitingAnnouncement(names: readonly string[]): string {
  if (names.length === 0) return '';
  if (names.length === 1) return `${names[0]} is waiting for you`;
  const shown = names.slice(0, 3).join(', ');
  const more = names.length > 3 ? ` and ${names.length - 3} more` : '';
  return `${names.length} sessions are waiting for you: ${shown}${more}`;
}

/** Stable key so the banner is only rebuilt when the request changes. */
export function questionKey(s: Pick<Session, 'id' | 'pending_question'>): string {
  return `${s.id}:${JSON.stringify(pendingQuestions(s))}`;
}

export interface Answer {
  /** Selected option indexes (one for single-select, any for multi). */
  selected: number[];
  /** Free text, used when no option is selected. */
  text: string;
}

/**
 * Build the `response_text` for AnswerQuestion exactly like the TUI: one
 * question sends its bare answer; several send `Q<n> (<header>): <answer>`
 * lines. Returns an error message when a question is unanswered.
 */
export function formatAnswers(
  questions: QuestionItem[],
  answers: Answer[],
): { ok: true; text: string } | { ok: false; error: string } {
  const parts: Array<[string, string]> = [];
  for (let i = 0; i < questions.length; i++) {
    const q = questions[i]!;
    const a = answers[i] ?? { selected: [], text: '' };
    let response: string;
    if (a.selected.length > 0) {
      const labels: string[] = [];
      for (const idx of [...a.selected].sort((x, y) => x - y)) {
        const opt = q.options?.[idx];
        if (!opt) return { ok: false, error: `Invalid option for question ${i + 1}` };
        labels.push(opt.label);
      }
      response = labels.join(', ');
    } else {
      response = a.text.trim();
    }
    if (!response) return { ok: false, error: `Please answer question ${i + 1}` };
    parts.push([q.header || 'Question', response]);
  }
  if (parts.length === 0) return { ok: false, error: 'Nothing to answer' };
  if (parts.length === 1) return { ok: true, text: parts[0]![1] };
  return { ok: true, text: parts.map(([h, r], i) => `Q${i + 1} (${h}): ${r}`).join('\n') };
}

export type PauseLevel = 'none' | 'soft' | 'hard';

export function pauseLabel(level: PauseLevel): string {
  if (level === 'none') return '';
  return level === 'hard' ? 'Paused until you resume' : 'Stopped; a manager may restart it';
}

// ---------- archive and restore (local #268) ----------

/** How long the Undo toast after Archive stays up (a keyboard user needs more than the 2.5 s of a plain toast). */
export const ARCHIVE_UNDO_MS = 8000;

/** What Archive asks first: where the session goes and that nothing is deleted. */
export function archiveConfirmText(name: string): string {
  return `Archive session "${name}"? It moves to the Archived tab; nothing is deleted, and you can restore it.`;
}

/** The toast after Archive: it names the destination; the toast carries an Undo button. */
export function archivedToastText(name: string): string {
  return `Moved "${name}" to Archived.`;
}

export const UNDO_LABEL = 'Undo';

// ---------- archive when finished (local #308; the TUI's <Space>a on a Running session) ----------

/**
 * rsid refuses ArchiveSession for any session it still runs (its active map: Starting, Running, and WaitingApproval,
 * which is parked on a question or approval but is still live), so Archive on one of those marks it to archive itself
 * when it finishes (MarkPendingArchive; rsid's finalizer honours the mark whatever status the session was in), exactly
 * as the TUI does (session_actions.rs `defers_archive`, local #315). Every other status is archived at once.
 */
export function defersArchive(status: string): boolean {
  return status === 'Starting' || status === 'Running' || status === 'WaitingApproval';
}

/** The session is marked to archive when it finishes (the mark means nothing once it is no longer running). */
export function archivePending(s: { status: string; pending_archive?: boolean | null }): boolean {
  return defersArchive(s.status) && s.pending_archive === true;
}

export const ARCHIVE_WHEN_FINISHED_LABEL = 'Archive when finished';
export const CANCEL_AUTO_ARCHIVE_LABEL = 'Cancel auto-archive';
/** The TUI's `[pending archive]` marker, in words. */
export const PENDING_ARCHIVE_NOTE = 'Archives when finished';
export const PENDING_ARCHIVE_ROW_TITLE = 'Archives itself when it finishes (Archive cancels)';
/** The TUI's two toasts, word for word. */
export const WILL_ARCHIVE_TOAST = 'Will archive when complete';
export const ARCHIVE_CLEARED_TOAST = 'Auto-archive on completion cleared';

/** The header Archive button's text and tooltip for the open session. */
export function archiveButton(s: { status: string; pending_archive?: boolean | null }): { label: string; title: string } {
  if (!defersArchive(s.status)) return { label: 'Archive', title: 'Move this session to the Archived tab' };
  return archivePending(s)
    ? { label: CANCEL_AUTO_ARCHIVE_LABEL, title: 'Stop this session from archiving itself when it finishes' }
    : { label: ARCHIVE_WHEN_FINISHED_LABEL, title: 'This session is still live, so it cannot be archived yet. Archive it automatically when its turn finishes.' };
}

/** What Archive asks on a running session: it does not move now, it archives itself later. */
export function archiveWhenFinishedConfirmText(name: string): string {
  return `Archive session "${name}" when it finishes? It archives itself when this turn finishes; nothing is deleted, and you can restore it.`;
}

/** The toast after Restore or Undo. */
export function restoredToastText(name: string): string {
  return `Restored "${name}" to the session list.`;
}

/**
 * The session header's Archive and Restore buttons. An archived session shows a primary Restore in
 * place of Archive; a deleted one shows neither choice (Archive stays, off). rsid down turns both off.
 */
export function archiveControls(status: string, down: boolean): { restoreHidden: boolean; restoreDisabled: boolean; archiveHidden: boolean; archiveDisabled: boolean } {
  const archived = status === 'Archived';
  return { restoreHidden: !archived, restoreDisabled: down, archiveHidden: archived, archiveDisabled: status === 'Deleted' || down };
}

/** How long after Submit answer / Let the agent decide a vanishing banner still counts as the answer's doing (local #297). */
export const ANSWER_LAND_MS = 15000;

/** Where focus goes when the question banner a keyboard user was in goes away (local #297). */
export type AnswerLanding = 'next-question' | 'composer' | 'conversation' | 'list';

/**
 * The next pending question if the session asks another one; else the message box when the session can take a
 * message; else the conversation (an archived session reads only); else the session list.
 */
export function answerLanding(o: { nextQuestion: boolean; sessionOpen: boolean; canMessage: boolean }): AnswerLanding {
  if (o.nextQuestion) return 'next-question';
  if (!o.sessionOpen) return 'list';
  return o.canMessage ? 'composer' : 'conversation';
}
