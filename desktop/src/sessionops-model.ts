// Session operations (hub #3000 gap sweep G2), pure rules with no DOM: pin
// (P), testing-needed (<Space>t), auto-rotation off (<Space>r), cancel retry
// (<Space>k), rotate (R), commit and push (<Space>x) and landing a session
// branch through the merge queue (<Space>gl). Each mirrors the TUI handler
// named beside it (crates/rsi/src/app/session_actions.rs, land.rs,
// action_handler/session.rs). Unit-tested under tests/sessionops.test.mjs.

/** The session fields these rules read; the daemon sends them on every Session. */
export interface OpsSession {
  id: string;
  status: string;
  session_kind?: string;
  title?: string | null;
  git_branch?: string | null;
  pinned_at?: string | null;
  testing_needed_at?: string | null;
  rotation_disabled_at?: string | null;
  rotation_depth?: number | null;
  retry_attempt?: number | null;
  max_retries?: number | null;
  /** Marked to archive itself when it finishes (shown only while it is Starting, Running or WaitingApproval). */
  pending_archive?: boolean | null;
}

const CONTAINERS = new Set(['Group', 'Epic']);

export function isContainerKind(kind: string | null | undefined): boolean {
  return !!kind && CONTAINERS.has(kind);
}

// ---------- toggles ----------

export type ToggleKind = 'pin' | 'testing' | 'rotation';

/** The daemon method and the Session field its answer carries. */
export const TOGGLES: Readonly<Record<ToggleKind, { command: string; field: 'pinned_at' | 'testing_needed_at' | 'rotation_disabled_at' }>> = {
  pin: { command: 'toggle_pin', field: 'pinned_at' },
  testing: { command: 'toggle_testing_needed', field: 'testing_needed_at' },
  rotation: { command: 'toggle_rotation_disabled', field: 'rotation_disabled_at' },
};

/** The timestamp a toggle answer carries (`{"pinned_at": "…"}`), null when it cleared. */
export function toggledAt(kind: ToggleKind, answer: unknown): string | null {
  if (!answer || typeof answer !== 'object') return null;
  const v = (answer as Record<string, unknown>)[TOGGLES[kind].field];
  return typeof v === 'string' && v !== '' ? v : null;
}

/** The TUI's toast after a toggle (session_actions.rs). */
export function toggleMessage(kind: ToggleKind, at: string | null): string {
  switch (kind) {
    case 'pin': return at ? 'Session pinned' : 'Session unpinned';
    case 'testing': return at ? 'Marked for testing' : 'Testing mark removed';
    case 'rotation': return at ? 'Auto-rotation disabled' : 'Auto-rotation enabled';
  }
}

/** The TUI's failure prefix for a toggle. */
export function toggleFailure(kind: ToggleKind): string {
  switch (kind) {
    case 'pin': return 'Pin failed';
    case 'testing': return 'Toggle testing needed failed';
    case 'rotation': return 'Toggle rotation failed';
  }
}

/** Testing-needed marks a leaf: the TUI's action names "the selected leaf session". */
export function testingBlock(s: Pick<OpsSession, 'session_kind'>): string | null {
  return isContainerKind(s.session_kind) ? 'A Group or Epic has no testing marker; pick a leaf session' : null;
}

// ---------- retry ----------

/** `CancelRetry` answers `{"cancelled": bool}`; true only when a retry timer was armed. */
export function retryCancelled(answer: unknown): boolean {
  return !!answer && typeof answer === 'object' && (answer as { cancelled?: unknown }).cancelled === true;
}

export function retryMessage(cancelled: boolean): string {
  return cancelled ? 'Retry cancelled' : 'No pending retry to cancel';
}

/** `↻N/M` while the session has an automatic retry budget (the TUI's retry badge). */
export function retryBadge(s: Pick<OpsSession, 'retry_attempt' | 'max_retries'>): string | null {
  const max = s.max_retries ?? 0;
  if (max <= 0) return null;
  return `↻${s.retry_attempt ?? 0}/${max}`;
}

// ---------- rotate ----------

/** RotateSession accepts a running or stopped session (rotate_focused_session). */
export const ROTATABLE: ReadonlySet<string> = new Set(['Running', 'Completed', 'Interrupted', 'Failed']);

export function rotateBlock(s: Pick<OpsSession, 'status' | 'session_kind'>): string | null {
  if (isContainerKind(s.session_kind)) return 'A Group or Epic has no context to rotate';
  return ROTATABLE.has(s.status) ? null : 'Can only rotate running or stopped sessions';
}

// ---------- commit and push ----------

/** `<Space>x` continues the session with this command (action_handler/session.rs). */
export const CI_COMMIT = '/ci_commit';

export function commitBlock(s: Pick<OpsSession, 'status' | 'session_kind'>): string | null {
  if (isContainerKind(s.session_kind)) return 'A Group or Epic has no work to commit';
  if (s.status === 'Starting') return 'Session is starting — wait for it to begin';
  if (s.status === 'Archived' || s.status === 'Deleted') return 'This session is read-only';
  return null;
}

// ---------- row markers ----------

/** ui/glyphs.rs: PIN, TESTING, ROTATION_OFF. */

export interface RowMark {
  glyph: string;
  title: string;
  cls: string;
}

/** The pin, testing, rotation and retry markers a session row shows (ui/glyphs.rs vocabulary). */
export function rowMarks(s: OpsSession): RowMark[] {
  const out: RowMark[] = [];
  if (s.pinned_at) out.push({ glyph: '◆', title: 'Pinned (P unpins)', cls: 'mark-pin' });
  if (s.testing_needed_at) out.push({ glyph: '☐', title: 'Manual testing needed (<Space>t clears)', cls: 'mark-testing' });
  if (s.rotation_disabled_at) out.push({ glyph: '⊘', title: 'Auto-rotation disabled (<Space>r enables)', cls: 'mark-rotation' });
  if (s.pending_archive === true && (s.status === 'Starting' || s.status === 'Running' || s.status === 'WaitingApproval')) out.push({ glyph: '⤓', title: 'Archives itself when it finishes (Archive cancels)', cls: 'mark-pending-archive' });
  const retry = retryBadge(s);
  if (retry) out.push({ glyph: retry, title: 'Automatic retry attempt / budget (<Space>k cancels a pending retry)', cls: 'mark-retry' });
  if ((s.rotation_depth ?? 0) > 0) out.push({ glyph: `↺${s.rotation_depth}`, title: 'Context rotation depth', cls: 'mark-depth' });
  return out;
}

// ---------- landing (<Space>gl, :land) ----------

/** How often queued landings are re-read (land.rs WATCH_INTERVAL). */
export const LAND_WATCH_MS = 5_000;
/** Entries one queue read may name (rsi_common ROLLING_QUEUE_WATCH_IDS_MAX). */
export const LAND_WATCH_MAX = 64;

/** `EnqueueRollingQueueSource`'s resolved source (rsi_common::rolling_queue). */
export interface LandSource {
  session_id?: string | null;
  branch?: string | null;
  commit: string;
  ahead?: number | null;
  on_rolling: boolean;
  dirty: boolean;
  title?: string | null;
}

export interface QueueOutcome {
  landed_sha?: string | null;
  refusal?: string | null;
  detail?: string | null;
}

export interface QueueEntry {
  id: string;
  sequence: number;
  source_commit: string;
  state: string;
  outcome?: QueueOutcome | null;
}

const short = (sha: string): string => sha.slice(0, 9);

/** Why the source cannot land (RollingQueueSourceV1::blocker), in plain words. */
export function landBlocker(src: LandSource): string | null {
  return src.on_rolling ? 'This session’s work is already on the shared main line, so there is nothing to land.' : null;
}

/** The session's name in plain copy: its title, else its branch, else the short id. */
export function landName(src: LandSource, session?: Pick<OpsSession, 'id' | 'title' | 'git_branch'> | null): string {
  const title = (session?.title ?? src.title ?? '').trim();
  if (title) return title;
  return src.branch || session?.git_branch || (session?.id ?? src.session_id ?? '').slice(0, 8) || 'this session';
}

export const LAND_CONFIRM_LABEL = 'Land';
export const LAND_CANCEL_LABEL = 'Cancel';
export const LAND_TOOLTIP = 'Add this session’s work to the shared main line after automatic checks';

export interface LandConfirmCopy {
  title: string;
  message: string;
  /** Secondary lines: the session and its branch. */
  detail: string[];
  confirmLabel: string;
  cancelLabel: string;
}

/** The confirm dialog's words: what landing does, no queue or git jargon. */
export function landConfirmCopy(src: LandSource, session?: Pick<OpsSession, 'id' | 'title' | 'git_branch'> | null): LandConfirmCopy {
  const changes = typeof src.ahead === 'number' && src.ahead > 0
    ? ` ${src.ahead === 1 ? '1 change is' : `${src.ahead} changes are`} ready to land.`
    : '';
  const unsaved = src.dirty ? ' Work that is not saved yet in its workspace is left out.' : '';
  const detail = [`Session: ${landName(src, session)}`];
  const branch = src.branch || session?.git_branch;
  if (branch) detail.push(`Branch: ${branch}`);
  return {
    title: 'Land this session?',
    message: `This adds this session’s work to the shared main line after automatic checks.${changes}${unsaved}`,
    detail,
    confirmLabel: LAND_CONFIRM_LABEL,
    cancelLabel: LAND_CANCEL_LABEL,
  };
}

/** The replay key of one confirmed landing (land.rs `operator-land-<uuid>`). */
export function landKey(uuid: string): string {
  return `operator-land-${uuid}`;
}

export const TERMINAL_QUEUE_STATES: ReadonlySet<string> = new Set(['published', 'refused', 'failed', 'superseded']);

/** Where a landing stands, in the four words the user sees. */
export type LandStage = 'queued' | 'checking' | 'landed' | 'refused';

export function landStage(state: string): LandStage {
  switch (state) {
    case 'published': return 'landed';
    case 'refused': case 'failed': case 'superseded': return 'refused';
    case 'admitted': case 'gating': return 'checking';
    default: return 'queued';
  }
}

/** The message right after the user confirms. */
export function landQueuedMessage(label: string): string {
  return `Queued “${label}” to land. Automatic checks run next.`;
}

/** The progress message when a landing moves to a new non-final stage. */
export function landProgressMessage(label: string, stage: LandStage): string {
  return stage === 'checking' ? `Checking “${label}” before it lands…` : `“${label}” is waiting its turn to land…`;
}

/** The refusal reason as the daemon sent it, else a plain fallback for the state. */
export function landReason(e: QueueEntry): string {
  const o = e.outcome;
  const text = (o?.refusal ?? o?.detail ?? '').trim();
  if (text) return text;
  if (e.state === 'superseded') return 'A newer version of this work replaced it.';
  return 'The automatic checks could not finish.';
}

/** The outcome announcement for an entry that settled. */
export function settledMessage(label: string, e: QueueEntry): string {
  if (landStage(e.state) === 'landed') return `Landed “${label}”. Its work is now on the shared main line.`;
  return `Not landed: “${label}”. ${landReason(e)}`;
}

export interface Watched {
  entryId: string;
  label: string;
  /** The last stage announced, so a stage is shown once. */
  stage?: LandStage;
}

/** Split the watched landings into the ones that settled (with their entry) and the rest. */
export function settle(watching: readonly Watched[], entries: readonly QueueEntry[]): {
  settled: Array<{ label: string; entry: QueueEntry }>;
  rest: Watched[];
} {
  const settled: Array<{ label: string; entry: QueueEntry }> = [];
  const rest: Watched[] = [];
  for (const w of watching) {
    const e = entries.find((x) => x.id === w.entryId);
    if (e && TERMINAL_QUEUE_STATES.has(e.state)) settled.push({ label: w.label, entry: e });
    else rest.push(w);
  }
  return { settled, rest };
}

/** Progress messages for watched landings that moved to a new stage; returns the updated watch list. */
export function stageNotes(watching: readonly Watched[], entries: readonly QueueEntry[]): {
  notes: string[];
  watching: Watched[];
} {
  const notes: string[] = [];
  const next = watching.map((w) => {
    const e = entries.find((x) => x.id === w.entryId);
    if (!e || TERMINAL_QUEUE_STATES.has(e.state)) return w;
    const stage = landStage(e.state);
    if (stage === (w.stage ?? 'queued')) return w;
    notes.push(landProgressMessage(w.label, stage));
    return { ...w, stage };
  });
  return { notes, watching: next };
}

/** The entry ids one queue read asks for. */
export function watchIds(watching: readonly Watched[]): string[] {
  return watching.slice(0, LAND_WATCH_MAX).map((w) => w.entryId);
}
