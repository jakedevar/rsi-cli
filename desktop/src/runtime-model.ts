// Errors that happen AFTER boot completed (pure; tests/runtime-errors.test.mjs, Issue #295).
//
// Boot evidence (boot-model.ts) answers one question: did the app start? A page error or an unhandled
// rejection later on does not change that answer: the app is up and the rest of it works. Those errors
// get a dismissible notice in plain words, the technical text behind a Details section, one line in the
// process log (the boot_report command prints it), and a repeat of the same error is counted, not
// shown again, so a failing timer can neither stack notices nor flood the log.

import { errorSentence, plainError, type ErrorClass } from './errors-model.js';

/** Browsers report these for harmless layout work; they are not faults. */
const BENIGN = /^ResizeObserver loop (?:completed with undelivered notifications|limit exceeded)\.?$/;

/** How long a repeat of the same error is folded into the notice already shown. */
export const COALESCE_MS = 60_000;
/** Log budget: lines in one window beyond this are counted, not printed. */
export const LOG_BUDGET = 20;
export const LOG_WINDOW_MS = 10_000;
/** Distinct errors remembered at once. */
const MAX_KEYS = 50;

export interface RuntimeNotice {
  /** What happened, in plain words (never a bare `TypeError: ...`). */
  text: string;
  /** The raw message, credentials redacted: shown under Details. */
  detail: string;
  kind: ErrorClass;
  /** Same error, same key: repeats fold together. */
  key: string;
}

/**
 * The plain notice for one runtime error, or null for a harmless browser notice.
 * `view` names what the user was looking at ("the Settings dialog"), when known.
 */
export function runtimeNotice(raw: string, view: string | null): RuntimeNotice | null {
  const one = raw.replace(/\s+/g, ' ').trim();
  if (one === '' || BENIGN.test(one)) return null;
  const p = plainError(raw);
  const key = p.detail.replace(/\d+/g, '#').slice(0, 200);
  let text: string;
  if (p.kind === 'internal') {
    text = `The app could not read rsid's answer${view ? ` for ${view}` : ''}. The rest of the app still works. ${p.next}`;
  } else if (p.kind === 'local') {
    text = `Something went wrong${view ? ` in ${view}` : ''}. The rest of the app still works. Try again; if it keeps happening, copy the Details into a bug report.`;
  } else {
    text = `${errorSentence(p)} The rest of the app still works.`;
  }
  return { text, detail: p.detail, kind: p.kind, key };
}

export interface RuntimeTally {
  /** Show (or re-show) the notice: first sight, or the fold window passed. */
  show: boolean;
  /** Print a process-log line. */
  log: boolean;
  /** How many times this error has happened inside the window. */
  count: number;
}

/** Folds repeats of the same error and bounds the log. One instance per page. */
export class RuntimeTracker {
  private seen = new Map<string, { first: number; count: number }>();
  private logWindowStart = 0;
  private logged = 0;

  note(key: string, now: number): RuntimeTally {
    let e = this.seen.get(key);
    if (!e || now - e.first >= COALESCE_MS) {
      e = { first: now, count: 0 };
      if (!this.seen.has(key) && this.seen.size >= MAX_KEYS) {
        const oldest = this.seen.keys().next().value;
        if (oldest !== undefined) this.seen.delete(oldest);
      }
      this.seen.delete(key);
      this.seen.set(key, e);
    }
    e.count += 1;
    const show = e.count === 1;
    // The first three of an error, then every 100th, inside a global budget.
    let log = e.count <= 3 || e.count % 100 === 0;
    if (now - this.logWindowStart >= LOG_WINDOW_MS) {
      this.logWindowStart = now;
      this.logged = 0;
    }
    if (log) {
      if (this.logged >= LOG_BUDGET) log = false;
      else this.logged += 1;
    }
    return { show, log, count: e.count };
  }
}

/** The process-log text for one runtime error (src-tauri/src/boot.rs prints it as `page: ...`). */
export function runtimeLogLine(raw: string, count: number): string {
  const one = raw.replace(/\s+/g, ' ').trim();
  return `runtime error${count > 1 ? ` (x${count})` : ''}: ${one}`;
}

/** The notice's repeat note, or "" for a single occurrence. */
export function repeatNote(count: number): string {
  return count > 1 ? `Happened ${count} times.` : '';
}
