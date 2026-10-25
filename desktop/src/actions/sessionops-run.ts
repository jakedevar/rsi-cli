// The DOM side of the session operations (actions/sessionops.ts): each action
// forwards one rsid method through a src-tauri/src/sessionops.rs command and
// reports the TUI's toast. Landing asks in a confirm dialog (Land / Cancel, Enter
// or <Space>gl confirms) and then watches the queued entry until it settles, as
// crates/rsi/src/land.rs does. Daemon text is set with textContent only.
import { api, invoke } from '../api.js';
import type { Invocation, SliceRuntime } from '../commands.js';
import { confirmDialog } from '../approvals.js';
import { LAND_CHORD } from '../confirm-model.js';
import { toast } from '../dom.js';
import { isVimKeysOn } from '../vimkeys.js';
import { refreshSessions, renderSessionList } from '../sessions.js';
import {
  CI_COMMIT, LAND_WATCH_MS, type LandSource, type OpsSession, type QueueEntry, type ToggleKind, type Watched,
  TOGGLES, landBlocker, landConfirmCopy, landKey, landName, landQueuedMessage, retryCancelled, retryMessage, settle,
  settledMessage, stageNotes, toggleFailure, toggleMessage, toggledAt, watchIds,
} from '../sessionops-model.js';
import { currentSessions, isReadOnly, selected, state } from '../state.js';
import { SWEEP_CSS, injectCss } from '../sweep-css.js';
import { detailSnapshot } from './detail.js';
import { type OpsSnapshot, SLICE, type SessionOpsActionId } from './sessionops.js';

const errText = (e: unknown): string => (e instanceof Error ? e.message : String(e));

// ---------- targets ----------

function byId(id: string | null): OpsSession | null {
  if (!id) return null;
  return (currentSessions().find((s) => s.id === id) as OpsSession | undefined) ?? null;
}

/** The open session, else the list cursor's (like the lifecycle slice). */
function target(): OpsSession | null {
  return (selected() as OpsSession | undefined) ?? byId(state.cursorId);
}

/** `R`: the open session when the transcript has the keys, else the list cursor's (rotate_focused_session). */
function rotateTarget(): OpsSession | null {
  const open = selected() as OpsSession | undefined;
  if (open && detailSnapshot().focus === 'transcript') return open;
  return byId(state.cursorId) ?? open ?? null;
}

export function opsSnapshot(): OpsSnapshot {
  const t = target();
  return {
    target: t,
    rotateTarget: rotateTarget(),
    readOnly: t ? isReadOnly(t as Parameters<typeof isReadOnly>[0]) : false,
  };
}

// ---------- toggles, retry, rotate, commit ----------

/** Write the toggled field into the cached row so the marker shows before the next refresh. */
function patch(id: string, field: 'pinned_at' | 'testing_needed_at' | 'rotation_disabled_at', at: string | null): void {
  const row = currentSessions().find((s) => s.id === id) as (OpsSession & Record<string, unknown>) | undefined;
  if (row) row[field] = at;
}

async function toggle(kind: ToggleKind): Promise<void> {
  const t = target();
  if (!t) return;
  try {
    const answer = await invoke<unknown>(TOGGLES[kind].command, { sessionId: t.id });
    const at = toggledAt(kind, answer);
    patch(t.id, TOGGLES[kind].field, at);
    renderSessionList();
    toast(toggleMessage(kind, at), true);
    void refreshSessions();
  } catch (e) {
    toast(`${toggleFailure(kind)}: ${errText(e)}`);
  }
}

async function cancelRetry(): Promise<void> {
  const t = target();
  if (!t) return;
  try {
    const cancelled = retryCancelled(await invoke<unknown>('cancel_retry', { sessionId: t.id }));
    toast(retryMessage(cancelled), cancelled);
    void refreshSessions();
  } catch (e) {
    toast(`Cancel retry failed: ${errText(e)}`);
  }
}

async function rotate(): Promise<void> {
  const t = rotateTarget();
  if (!t) return;
  toast('Rotating session context…', true);
  try {
    await invoke<unknown>('rotate_session', { sessionId: t.id });
    toast('Rotation started: the continuation appears in the list when it launches', true);
    void refreshSessions();
  } catch (e) {
    toast(`Rotate failed: ${errText(e)}`);
  }
}

async function commitPush(): Promise<void> {
  const s = selected();
  if (!s) return;
  toast('Committing and pushing...', true);
  try {
    await api.sendMessage(s.id, CI_COMMIT);
    void refreshSessions();
  } catch (e) {
    toast(`Send failed: ${errText(e)}`);
  }
}

// ---------- landing ----------

let watching: Watched[] = [];
let watchTimer: number | null = null;
let watchBusy = false;
let asking = false;

interface EnqueueAnswer {
  source: LandSource;
  entry?: QueueEntry | null;
}

function newKey(): string {
  return landKey(crypto.randomUUID());
}

/** Land opens a confirm dialog from every path (menu, button, palette, key chord); Land sends the RPC. */
async function land(): Promise<void> {
  const t = target();
  if (!t || asking) return;
  asking = true;
  try {
    const r = await invoke<EnqueueAnswer>('land_session', { sessionId: t.id, commit: null, idempotencyKey: null, dryRun: true });
    const blocker = landBlocker(r.source);
    if (blocker) { toast(blocker); return; }
    const copy = landConfirmCopy(r.source, t);
    const ok = await confirmDialog(copy.message, copy.confirmLabel, {
      title: copy.title, detail: copy.detail, cancelLabel: copy.cancelLabel, primary: true,
      chord: LAND_CHORD, vimKeysOn: isVimKeysOn,
    });
    if (ok) await confirmLand(t.id, r.source.commit, landName(r.source, t));
  } catch (e) {
    toast(`Land: ${errText(e)}`);
  } finally {
    asking = false;
  }
}

async function confirmLand(sessionId: string, commit: string, label: string): Promise<void> {
  try {
    const r = await invoke<EnqueueAnswer>('land_session', {
      sessionId, commit, idempotencyKey: newKey(), dryRun: false,
    });
    if (r.entry) {
      toast(landQueuedMessage(label), true);
      watching.push({ entryId: r.entry.id, label });
      startWatch();
    }
  } catch (e) {
    toast(`Land: ${errText(e)}`);
  }
}

function startWatch(): void {
  if (watchTimer !== null) return;
  watchTimer = window.setInterval(() => { void pollWatch(); }, LAND_WATCH_MS);
}

async function pollWatch(): Promise<void> {
  if (watchBusy) return;
  if (watching.length === 0) {
    if (watchTimer !== null) window.clearInterval(watchTimer);
    watchTimer = null;
    return;
  }
  watchBusy = true;
  try {
    const r = await invoke<{ entries?: QueueEntry[] }>('rolling_queue_entries', { entryIds: watchIds(watching) });
    const entries = r.entries ?? [];
    const progress = stageNotes(watching, entries);
    for (const note of progress.notes) toast(note, true);
    const { settled, rest } = settle(progress.watching, entries);
    watching = rest;
    for (const { label, entry } of settled) toast(settledMessage(label, entry), entry.state === 'published');
  } catch {
    // A failed read is retried on the next tick, as the TUI's watch does.
  } finally {
    watchBusy = false;
  }
}

// ---------- dispatch ----------

export function runSessionOps(id: SessionOpsActionId, _inv: Invocation): void {
  switch (id) {
    case 'toggle-pin': void toggle('pin'); return;
    case 'toggle-testing': void toggle('testing'); return;
    case 'toggle-rotation': void toggle('rotation'); return;
    case 'cancel-retry': void cancelRetry(); return;
    case 'rotate-session': void rotate(); return;
    case 'commit-push': void commitPush(); return;
    case 'land-session': void land(); return;
  }
}

/** The session-ops slice's DOM side, collected by actions/runtime.generated.ts. */
export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runSessionOps(id as SessionOpsActionId, inv),
  snapshot: () => ({ ops: opsSnapshot() }),
  init: () => injectCss('sweep-css', SWEEP_CSS),
};
