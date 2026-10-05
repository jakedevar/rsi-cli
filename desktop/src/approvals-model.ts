// Pure helpers for operator attention (pending questions, pause state).
// No DOM; unit-tested under tests/.
import type { QuestionItem, Session } from './api.js';

/** Sent when the operator declines; matches the TUI's DECLINE_RESPONSE. */
export const DECLINE_RESPONSE = '(declined — no preference; use your best judgment)';

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
  return level === 'none' ? '' : `${level} pause`;
}
