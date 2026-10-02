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
