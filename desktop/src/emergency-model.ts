// Pure rules for the emergency stop (hub #3008 follow-up): which surfaces ask
// first, how the daemon's report is read, and the local pause state it sets.
// Tested in tests/emergency.test.mjs; the DOM side is actions/emergency.ts.
import type { ActionSource } from './registry.js';

/**
 * Whether a surface shows the confirm dialog. The TUI has no confirmation:
 * `<Space>X`, `:stopall` / `:stop-all` and Enter on the Settings row all run
 * `emergency_stop_all` at once (crates/rsi/src/action_handler/mod.rs:354). The
 * desktop's key path (chords and the `:` line, both source `key`) matches that;
 * menus, the palette, right-click and buttons have no chord to mistype, so they ask.
 */
export function stopNeedsConfirm(source: ActionSource): boolean {
  return source !== 'key';
}

export const STOP_CONFIRM_TITLE = 'Stop all AI work now?';
export const STOP_CONFIRM_BODY =
  'Running sessions stop and nothing new starts, not even new sessions, until you choose Resume AI work.';

/** What the daemon reports about an emergency stop (the fields the TUI reads). */
export interface StopReport {
  /** `cancelled_invocations.length`. */
  cancelled: number;
  /** `interrupted_sessions`: the sessions the stop interrupted. */
  interrupted: string[];
}

const isId = (v: unknown): v is string => typeof v === 'string' && v.length > 0;

/** Read `ModelControlPolicyUpdateReport`; null when it is not an object. */
export function parseStopReport(raw: unknown): StopReport | null {
  if (typeof raw !== 'object' || raw === null) return null;
  const r = raw as { cancelled_invocations?: unknown; interrupted_sessions?: unknown };
  const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
  return {
    cancelled: list(r.cancelled_invocations).length,
    interrupted: list(r.interrupted_sessions).filter(isId).map((id) => id.toLowerCase()),
  };
}

/** The TUI's toast: 'Emergency stop applied: N cancelled, M interrupted'. */
export function stopMessage(r: StopReport): string {
  return `Emergency stop applied: ${r.cancelled} cancelled, ${r.interrupted.length} interrupted`;
}

/** The toast after a stop: the TUI's count line, then the way back (the banner's button). */
export function stopToast(r: StopReport): string {
  return `${stopMessage(r)}. ${RESUME_HINT}`;
}

// ---------- the way back (local #224) ----------

/** The label of the banner button and of the way back named everywhere else. */
export const RESUME_LABEL = 'Resume AI work';
/** Where the same switch lives in Settings (the banner button sets the same mode). */
export const RESUME_SETTINGS_PATH = 'Settings › Safety & Spend › Model Control';
export const RESUME_HINT = `Choose ${RESUME_LABEL} in the banner at the top to start again.`;

/** What a model-control mode (the TUI slug) means to a person; null for normal and unknown modes. */
export function bannerText(slug: string | null): string | null {
  switch (slug) {
    case 'stop-all': return 'All AI work is stopped. Nothing new can start, not even new sessions or messages.';
    case 'pause-background': return 'Background AI work is paused. Work you start yourself still runs.';
    case 'deny-paid': return 'Paid AI work is blocked. Only free and local models can run.';
    case 'local-only': return 'Only local models can run. Hosted AI work is blocked.';
    default: return null;
  }
}

export const RESUME_CONFIRM_TITLE = 'Resume AI work?';
/** The one confirm before the banner button changes the mode back to normal. */
export function resumeConfirmBody(slug: string | null): string {
  const what = slug === 'stop-all' ? 'New sessions and messages can start again'
    : 'Every kind of AI work can run again';
  return `${what}. Sessions the stop interrupted stay stopped until you resume each one.`;
}

/** The toast after a successful resume. */
export function resumeMessage(): string {
  return 'AI work resumed. New sessions and messages can start again.';
}

/** The banner's tooltip for the row badge a stop leaves; keeps the TUI's HARD meaning. */
export const STOPPED_BADGE_LABEL = 'Stopped';
export const STOPPED_BADGE_TIP =
  'Stopped by Emergency stop (the TUI calls this a HARD pause). Choose Resume on the session, or Resume AI work in the banner.';

/** Operator pauses this app set locally (the TUI's `operator_pauses`); the stop only ever sets HARD. */
export type LocalPauses = ReadonlyMap<string, 'hard'>;

export const NO_PAUSES: LocalPauses = new Map();

/** Mark every interrupted session HARD, as the TUI does. */
export function applyStopReport(pauses: LocalPauses, r: StopReport): LocalPauses {
  if (r.interrupted.length === 0) return pauses;
  const next = new Map(pauses);
  for (const id of r.interrupted) next.set(id, 'hard');
  return next;
}

/** Forget one session's local pause (it was cleared or downgraded). */
export function dropPause(pauses: LocalPauses, id: string): LocalPauses {
  if (!pauses.has(id)) return pauses;
  const next = new Map(pauses);
  next.delete(id);
  return next;
}

/** Keep only the pauses of sessions still in `ids` (one that left the list drops its marker). */
export function prunePauses(pauses: LocalPauses, ids: Iterable<string>): LocalPauses {
  const live = new Set(ids);
  let kept = pauses;
  for (const id of pauses.keys()) if (!live.has(id)) kept = dropPause(kept, id);
  return kept;
}
