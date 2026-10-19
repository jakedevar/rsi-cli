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
  const retry = retryBadge(s);
  if (retry) out.push({ glyph: retry, title: 'Automatic retry attempt / budget (<Space>k cancels a pending retry)', cls: 'mark-retry' });
  if ((s.rotation_depth ?? 0) > 0) out.push({ glyph: `↺${s.rotation_depth}`, title: 'Context rotation depth', cls: 'mark-depth' });
  return out;
}

// ---------- landing (<Space>gl, :land) ----------

/** How long a shown commit stays armed for the confirming press (land.rs CONFIRM_WINDOW). */
export const LAND_CONFIRM_MS = 30_000;
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

export interface Armed {
  sessionId: string;
  commit: string;
  at: number;
}

const short = (sha: string): string => sha.slice(0, 9);

/** One line for the source and where it stands (RollingQueueSourceV1::summary). */
export function landSummary(src: LandSource): string {
  const name = src.branch || src.title || src.session_id || '?';
  let position: string;
  if (src.on_rolling) position = 'already on rolling';
  else if (src.ahead === 1) position = 'unmerged, 1 commit ahead of rolling';
  else if (typeof src.ahead === 'number') position = `unmerged, ${src.ahead} commits ahead of rolling`;
  else position = 'unmerged';
  const dirty = src.dirty ? ', sandbox has uncommitted changes the landing omits' : '';
  return `${name} @ ${short(src.commit)}: ${position}${dirty}`;
}

/** Why the source cannot land (RollingQueueSourceV1::blocker). */
export function landBlocker(src: LandSource): string | null {
  return src.on_rolling ? 'already on rolling; nothing to land' : null;
}

/** True when this arming confirms `sessionId` at `now` (land.rs Armed::confirms). */
export function confirmsLand(armed: Armed | null, sessionId: string, now: number): boolean {
  return !!armed && armed.sessionId === sessionId && now - armed.at < LAND_CONFIRM_MS;
}

/** The first press's prompt. */
export function landPrompt(src: LandSource): string {
  return `Land ${landSummary(src)}? Press <Space>gl (or :land) again within ${LAND_CONFIRM_MS / 1000}s to queue it.`;
}

/** The replay key of one confirmed landing (land.rs `operator-land-<uuid>`). */
export function landKey(uuid: string): string {
  return `operator-land-${uuid}`;
}

/** One line for a queue entry's state and outcome (RollingQueueEntryV1::status_line). */
export function statusLine(e: QueueEntry): string {
  const o = e.outcome;
  const detail = o?.landed_sha ? `landed ${short(o.landed_sha)}` : (o?.refusal ?? o?.detail ?? null);
  const head = `queue #${e.sequence} ${short(e.source_commit)}: ${e.state}`;
  return detail ? `${head} (${detail})` : head;
}

export const TERMINAL_QUEUE_STATES: ReadonlySet<string> = new Set(['published', 'refused', 'failed', 'superseded']);

/** The outcome announcement for an entry that settled (land.rs settled_message). */
export function settledMessage(label: string, e: QueueEntry): string {
  return `${e.state === 'published' ? 'landed' : 'NOT landed'}: ${label} - ${statusLine(e)}`;
}

export interface Watched {
  entryId: string;
  label: string;
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

/** The entry ids one queue read asks for. */
export function watchIds(watching: readonly Watched[]): string[] {
  return watching.slice(0, LAND_WATCH_MAX).map((w) => w.entryId);
}
