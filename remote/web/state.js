export function createViewState() {
  return { sessions: [], selectedId: null, listScrollTop: 0, screen: 'list', stale: false, listError: '', detail: null, detailError: '', listLoading: false, detailLoading: false };
}

export function groupSessions(sessions, now = Date.now()) {
  const needsYou = sessions.filter((session) => session.needsYou === true || session.status === 'WaitingApproval' || session.status === 'Failed');
  const inFlight = sessions.filter((session) => !needsYou.includes(session) && ['Starting', 'Running'].includes(session.status));
  const recent = sessions.filter((session) => !needsYou.includes(session) && !inFlight.includes(session)
    && now - Date.parse(session.updatedAt) <= 10 * 24 * 60 * 60 * 1000);
  const grouped = new Set([...needsYou, ...inFlight, ...recent].map((session) => session.id));
  const quiet = sessions.filter((session) => !grouped.has(session.id));
  const sort = (a, b) => Date.parse(b.updatedAt) - Date.parse(a.updatedAt) || a.id.localeCompare(b.id);
  return [
    { name: 'NEEDS YOU', sessions: needsYou.sort(sort) },
    { name: 'IN FLIGHT', sessions: inFlight.sort(sort) },
    { name: 'RECENT', sessions: recent.sort(sort) },
    { name: 'QUIET', sessions: quiet.sort(sort) },
  ].filter((group) => group.sessions.length);
}

export function enterDetail(state, id, scrollTop) {
  return { ...state, screen: 'detail', selectedId: id, listScrollTop: scrollTop, detail: null, detailError: '', detailLoading: true };
}

export function returnToList(state) {
  return { ...state, screen: 'list', detailLoading: false };
}

// Visible-page polling with backoff. Timers are injected so tests can drive a
// fake clock; `run` is a promise-returning refresh that rejects on failure.
export function createPoller({ run, intervalMs, maxIntervalMs, setTimer, clearTimer, isVisible }) {
  let timer = null;
  let running = false;
  let stopped = true;
  let interval = intervalMs;

  function clearScheduled() {
    if (timer !== null) {
      clearTimer(timer);
      timer = null;
    }
  }

  function schedule(delay) {
    clearScheduled();
    if (stopped || !isVisible()) return;
    timer = setTimer(() => {
      timer = null;
      startRun();
    }, delay);
  }

  function settle(success) {
    running = false;
    interval = success ? intervalMs : Math.min(interval * 2, maxIntervalMs);
    schedule(interval);
  }

  function startRun() {
    if (stopped || running || !isVisible()) return;
    running = true;
    let result;
    try {
      result = run();
    } catch (error) {
      result = Promise.reject(error);
    }
    Promise.resolve(result).then(() => settle(true), () => settle(false));
  }

  function kick() {
    if (stopped || !isVisible()) return;
    clearScheduled();
    startRun();
  }

  function onVisibilityChange() {
    if (stopped) return;
    if (isVisible()) {
      clearScheduled();
      startRun();
    } else {
      clearScheduled();
    }
  }

  return {
    start() {
      stopped = false;
      kick();
    },
    stop() {
      stopped = true;
      clearScheduled();
    },
    kick,
    onVisibilityChange,
    currentInterval() {
      return interval;
    },
  };
}

// ---- Answer drafts (#990 A4) -------------------------------------------------
//
// A draft is the phone-side state of one answer to one waiting decision. It
// lives outside the rendered DOM, so the detail poll can rebuild the page
// without losing what was typed, which option was picked, or which idempotency
// key a possibly-delivered answer was sent under.

const UUID_V4_FALLBACK_BYTES = 16;

// Canonical lowercase UUID v4. `randomUUID` needs a secure context; the
// fallback uses `getRandomValues`, which does not.
export function randomUuid(cryptoImpl = globalThis.crypto) {
  if (cryptoImpl && typeof cryptoImpl.randomUUID === 'function') return cryptoImpl.randomUUID().toLowerCase();
  const bytes = new Uint8Array(UUID_V4_FALLBACK_BYTES);
  cryptoImpl.getRandomValues(bytes);
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

export const MAX_ANSWER_BYTES = 2048;

export function answerProblem(text) {
  if (typeof text !== 'string' || text.trim() === '') return 'Write or choose an answer first.';
  if (text.includes('\0')) return 'The answer cannot contain a NUL character.';
  if (new TextEncoder().encode(text).byteLength > MAX_ANSWER_BYTES) return `The answer is longer than ${MAX_ANSWER_BYTES} bytes.`;
  return '';
}

const draftKey = (sessionId, decisionKey) => `${sessionId}\u0000${decisionKey}`;

export function createDraftStore({ newKey = randomUuid } = {}) {
  const drafts = new Map();
  // A discarded draft leaves one notice for the next draft of that decision.
  const notices = new Map();

  function fresh(sessionId, target) {
    const key = draftKey(sessionId, target.decisionKey);
    const draft = {
      sessionId,
      decisionKey: target.decisionKey,
      pending: target.pending === true,
      rowVersion: target.rowVersion,
      digest: target.digest,
      kind: target.kind,
      title: target.title,
      detail: target.detail,
      options: target.options,
      text: '',
      idempotencyKey: newKey(),
      status: 'editing',
      error: null,
      notice: notices.get(key) ?? '',
      sentRequest: null,
      sentText: null,
      receipt: null,
    };
    notices.delete(key);
    drafts.set(key, draft);
    return draft;
  }

  const inFlight = (draft) => draft.status === 'sending' || draft.status === 'unknown' || draft.status === 'receipt';

  return {
    get: (sessionId, decisionKey) => drafts.get(draftKey(sessionId, decisionKey)) ?? null,
    forSession: (sessionId) => [...drafts.values()].filter((draft) => draft.sessionId === sessionId),
    size: () => drafts.size,

    // The draft for a target. A draft that was started against another row
    // version or digest is discarded: the decision changed under it, so the
    // typed text no longer answers what is being asked. A sent answer keeps its
    // draft so its outcome stays visible.
    ensure(sessionId, target) {
      const existing = drafts.get(draftKey(sessionId, target.decisionKey));
      if (existing && (inFlight(existing) || (existing.rowVersion === target.rowVersion && existing.digest === target.digest))) {
        if (!inFlight(existing)) Object.assign(existing, { title: target.title, detail: target.detail, options: target.options });
        return existing;
      }
      if (existing) notices.set(draftKey(sessionId, target.decisionKey), 'This decision changed while you were looking at it. Review it and answer again; your earlier draft was discarded.');
      return fresh(sessionId, target);
    },

    // Drop idle drafts whose decision is no longer waiting. `items` is a fresh,
    // successful targets read; never call this with a failed read.
    sync(sessionId, items) {
      const present = new Set(items.map((item) => item.decisionKey));
      for (const draft of this.forSession(sessionId)) {
        if (!present.has(draft.decisionKey) && !inFlight(draft)) drafts.delete(draftKey(sessionId, draft.decisionKey));
      }
      for (const item of items) this.ensure(sessionId, item);
    },

    setText(draft, text) {
      if (inFlight(draft)) return;
      draft.text = text;
      // The daemon replays an idempotency key only for the identical answer, so
      // a different text never reuses a key that already went out.
      if (draft.sentText !== null && text !== draft.sentText) {
        draft.idempotencyKey = newKey();
        draft.sentRequest = null;
        draft.sentText = null;
      }
    },

    // Freeze the request for this attempt. A retry of an answer whose outcome
    // is unknown resends the exact frozen request, fence included.
    begin(draft, fence) {
      const problem = answerProblem(draft.text);
      if (draft.status === 'sending') return { problem: 'Already sending.' };
      if (draft.status === 'unknown' && draft.sentRequest) {
        draft.status = 'sending';
        draft.error = null;
        return { request: draft.sentRequest };
      }
      if (problem) return { problem };
      if (!draft.pending && !fence) return { problem: 'The answer fence is not available. Refresh and try again.' };
      draft.sentRequest = {
        decisionKey: draft.decisionKey,
        ...(draft.pending ? { pending: true } : {}),
        rowVersion: draft.rowVersion,
        digest: draft.digest,
        fence: fence ? { policyVersion: fence.policyVersion, scopeVersion: fence.scopeVersion } : null,
        answer: draft.text,
        idempotencyKey: draft.idempotencyKey,
      };
      draft.sentText = draft.text;
      draft.status = 'sending';
      draft.error = null;
      return { request: draft.sentRequest };
    },

    settleOk(draft, receipt) {
      draft.status = 'receipt';
      draft.receipt = receipt;
      draft.error = null;
    },

    settleError(draft, error) {
      const kind = error?.kind ?? 'unexpected';
      if (kind === 'stale') {
        // The decision changed: discard the draft; the next targets read makes
        // a new one that carries this notice.
        notices.set(draftKey(draft.sessionId, draft.decisionKey), 'This decision changed. Refresh shows its current state; your draft was discarded.');
        draft.status = 'discarded';
        drafts.delete(draftKey(draft.sessionId, draft.decisionKey));
        return;
      }
      draft.error = { kind, message: error?.message ?? 'The answer could not be sent.', retryAfter: error?.retryAfter ?? null };
      draft.status = kind === 'unknown' ? 'unknown' : 'editing';
    },

    // Done with a delivered answer; the poll shows what happened next.
    dismiss(draft) {
      drafts.delete(draftKey(draft.sessionId, draft.decisionKey));
    },

    clearSession(sessionId) {
      for (const draft of this.forSession(sessionId)) if (!inFlight(draft)) drafts.delete(draftKey(sessionId, draft.decisionKey));
    },
  };
}

// Send one draft through `api.answerDecision` and record the outcome. Resolves
// to the draft's resulting status; never throws for an expected refusal.
export async function submitDraft({ api, store, draft, fence }) {
  const begun = store.begin(draft, fence);
  if (begun.problem) {
    draft.error = { kind: 'invalid', message: begun.problem, retryAfter: null };
    return draft.status;
  }
  try {
    const receipt = await api.answerDecision(draft.sessionId, begun.request);
    store.settleOk(draft, receipt);
  } catch (error) {
    store.settleError(draft, error);
  }
  return draft.status;
}

// What to show for a draft: a status line, its tone, and which controls apply.
// Receipt copy never claims the decision is answered; the poll shows delivery.
export function describeDraft(draft) {
  if (draft.status === 'sending') {
    return { tone: 'busy', message: 'Sending…', canEdit: false, canSend: false, canRetry: false, canDismiss: false };
  }
  if (draft.status === 'receipt') {
    const receipt = draft.receipt ?? {};
    if (receipt.state) return describePendingReceipt(receipt);
    const key = receipt.key ? ` Receipt ${receipt.key}.` : '';
    const again = receipt.deduplicated ? ' This answer had already been recorded.' : '';
    return {
      tone: 'ok',
      message: `Sent. Your RSI host accepted the answer.${key}${again} This page does not mark it answered; the decision leaves this list once the host moves on.`,
      canEdit: false, canSend: false, canRetry: false, canDismiss: true,
    };
  }
  if (draft.status === 'unknown') {
    return {
      tone: 'warn',
      message: `${draft.error?.message ?? 'The outcome is unknown.'} Your answer is locked. Retry sends the same answer under the same key and cannot be applied twice.`,
      canEdit: false, canSend: false, canRetry: true, canDismiss: false,
    };
  }
  if (draft.error) return { tone: 'error', message: draft.error.message, canEdit: true, canSend: true, canRetry: false, canDismiss: false };
  if (draft.notice) return { tone: 'warn', message: draft.notice, canEdit: true, canSend: true, canRetry: false, canDismiss: false };
  return { tone: 'idle', message: '', canEdit: true, canSend: true, canRetry: false, canDismiss: false };
}

export function describePendingReceipt(receipt) {
  const state = receipt.state;
  const message = state === 'uncertain' ? 'The answer may have been consumed. The host will not send it again. Inspect the session on your RSI host.'
    : state === 'succeeded' ? 'The host established delivery. Native approval consumption still waits for provider confirmation.'
    : state === 'refused' || state === 'failed' ? 'The host refused or failed this answer. Refresh the session on your RSI host.'
    : 'The host recorded the answer and is waiting to deliver it.';
  return { tone: state === 'uncertain' ? 'warn' : state === 'refused' || state === 'failed' ? 'error' : 'ok',
    message: `${message} Receipt ${receipt.key}.`, canEdit: false, canSend: false, canRetry: false, canDismiss: true };
}
