import { ApiError, createApi, limits } from './api.js';
import { createPoller, createViewState, enterDetail, groupSessions, returnToList } from './state.js';

const api = createApi();
const state = createViewState();
let detailRequestGeneration = 0;
const MAX_DETAIL_EVENTS = 500;
let olderLoading = false;
let olderError = '';
const elements = {
  connection: document.querySelector('#connection'),
  connectionText: document.querySelector('#connection-text'),
  refresh: document.querySelector('#refresh'),
  listPane: document.querySelector('#list-pane'),
  detailPane: document.querySelector('#detail-pane'),
  sessionList: document.querySelector('#session-list'),
  sessionCount: document.querySelector('#session-count'),
  listState: document.querySelector('#list-state'),
  detailState: document.querySelector('#detail-state'),
  detailContent: document.querySelector('#detail-content'),
  search: document.querySelector('#search'),
};

const isVisible = () => document.visibilityState === 'visible';
const listPoller = createPoller({
  run: refreshSessions,
  intervalMs: 30000,
  maxIntervalMs: 120000,
  setTimer: (fn, ms) => window.setTimeout(fn, ms),
  clearTimer: (id) => window.clearTimeout(id),
  isVisible,
});
const detailPoller = createPoller({
  run: refreshDetail,
  intervalMs: 10000,
  maxIntervalMs: 120000,
  setTimer: (fn, ms) => window.setTimeout(fn, ms),
  clearTimer: (id) => window.clearTimeout(id),
  isVisible,
});

function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

// Older pages can overlap the freshly polled latest page, so events are keyed by
// id and re-sorted by sequence. A bounded window keeps a long session's detail
// view from growing without limit while it is open.
function mergeEvents(previous, fresh) {
  const byId = new Map();
  for (const event of [...previous, ...fresh]) byId.set(event.id, event);
  const events = [...byId.values()].sort((a, b) => a.sequence - b.sequence);
  return events.length > MAX_DETAIL_EVENTS ? events.slice(events.length - MAX_DETAIL_EVENTS) : events;
}

function eventElement(id) {
  const escaped = typeof CSS !== 'undefined' && CSS.escape ? CSS.escape(id) : id;
  return elements.detailContent.querySelector(`[data-event-id="${escaped}"]`);
}

function setConnection(kind, text) {
  elements.connection.dataset.state = kind;
  elements.connectionText.textContent = text;
}

function formatTime(iso) {
  const date = new Date(iso);
  return new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' }).format(date);
}

function formatClock(date = new Date()) {
  return new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' }).format(date);
}

function renderListPreservingScroll() {
  const scrollTop = elements.sessionList.scrollTop;
  renderList();
  elements.sessionList.scrollTop = scrollTop;
}

function renderList() {
  const query = elements.search.value.trim().toLocaleLowerCase();
  const sessions = state.sessions.filter((session) => `${session.title} ${session.provider} ${session.status}`.toLocaleLowerCase().includes(query));
  elements.sessionCount.textContent = query ? `${sessions.length} of ${state.sessions.length}` : `${state.sessions.length}`;
  if (state.sessions.length === limits.MAX_SESSIONS && !state.listError) {
    setListState(`Showing the first ${limits.MAX_SESSIONS} sessions. The list may be incomplete.`, { stale: true });
  }
  elements.sessionList.replaceChildren();
  for (const group of groupSessions(sessions)) {
    const section = node('section', 'session-group');
    const heading = node('h2', 'group-title', group.name);
    section.append(heading);
    for (const session of group.sessions) {
      const row = node('button', `session-row${session.id === state.selectedId ? ' is-selected' : ''}`);
      row.type = 'button';
      row.setAttribute('role', 'listitem');
      row.setAttribute('aria-label', `${session.title}, ${session.status}, ${session.provider}, updated ${formatTime(session.updatedAt)}`);
      row.setAttribute('aria-current', session.id === state.selectedId ? 'true' : 'false');
      const status = node('span', `status-mark status-${session.status.toLowerCase()}`);
      status.setAttribute('aria-hidden', 'true');
      const copy = node('span', 'session-copy');
      copy.append(node('span', 'session-title', session.title));
      copy.append(node('span', 'session-meta', `${session.provider} · ${session.status}`));
      if (session.needsYou) copy.append(node('span', 'needs-you-badge', 'Needs you'));
      row.append(status, copy, node('time', 'session-time', formatTime(session.updatedAt)));
      row.addEventListener('click', () => openSession(session.id, row));
      section.append(row);
    }
    elements.sessionList.append(section);
  }
  if (!sessions.length) {
    const empty = node('div', 'empty-state', query ? 'No sessions match your search.' : 'No sessions are available yet.');
    elements.sessionList.append(empty);
  }
}

function setListState(message, { stale = false, error = false } = {}) {
  elements.listState.replaceChildren();
  elements.listState.hidden = !message;
  elements.listState.className = `inline-state${stale ? ' is-stale' : ''}${error ? ' is-error' : ''}`;
  if (!message) return;
  elements.listState.append(node('span', 'state-copy', message));
  if (error) {
    const retry = node('button', 'text-button', 'Retry');
    retry.type = 'button';
    retry.addEventListener('click', () => listPoller.kick());
    elements.listState.append(retry);
  }
}

async function refreshSessions() {
  if (state.listLoading) return;
  state.listLoading = true;
  const hadSessions = state.sessions.length > 0;
  try {
    state.sessions = await api.listSessions();
    state.stale = false;
    state.listError = '';
    setConnection('connected', `Updated ${formatClock()}`);
    setListState('');
    renderListPreservingScroll();
  } catch (error) {
    const message = error instanceof ApiError ? error.message : 'Could not load sessions. Check the connection and retry.';
    state.listError = message;
    state.stale = hadSessions;
    setConnection('error', hadSessions ? `Showing stale data · ${message}` : message);
    if (hadSessions) setListState('');
    else setListState(message, { error: true });
    throw error;
  } finally {
    state.listLoading = false;
  }
}

function renderWaiting({ session, decisions, decisionsError }) {
  const open = Array.isArray(decisions) ? decisions.filter((decision) => decision.open) : [];
  if (!decisionsError && !session.attentionIncomplete && open.length === 0) return;
  const section = node('section', 'waiting-section');
  section.append(node('h2', 'waiting-title', 'Waiting on your RSI host'));
  if (decisionsError) {
    section.append(node('p', 'waiting-error', decisionsError));
  } else if (open.length) {
    const list = node('ul', 'decision-list');
    list.setAttribute('aria-label', 'Decisions waiting on the RSI host');
    for (const decision of open) {
      const item = node('li', `decision decision-${decision.kind}`);
      const head = node('div', 'decision-head');
      head.append(node('span', `decision-kind decision-kind-${decision.kind}`, decision.kind === 'question' ? 'Question' : 'Approval'));
      head.append(node('span', 'decision-title', decision.title));
      item.append(head);
      if (decision.detail) item.append(node('p', 'decision-detail', decision.detail));
      list.append(item);
    }
    section.append(list);
  }
  if (open.length) section.append(node('p', 'waiting-note', 'Answer these on your RSI host; this view is read-only.'));
  if (session.attentionIncomplete) {
    section.append(node('p', 'waiting-incomplete', 'Some sources could not be checked; the list may be incomplete.'));
  }
  elements.detailContent.append(section);
}

function renderDetail() {
  elements.detailContent.replaceChildren();
  if (!state.detail) return;
  const { session, events, decisions, decisionsError, hasOlder } = state.detail;
  const header = node('header', 'detail-header');
  const back = node('button', 'back-button');
  back.type = 'button';
  back.setAttribute('aria-label', 'Back to sessions');
  back.append(node('span', 'back-chevron', '←'), node('span', '', 'Sessions'));
  back.addEventListener('click', () => closeSession(true));
  const headerLine = node('div', 'detail-title-line');
  headerLine.append(node('span', `status-mark status-${session.status.toLowerCase()}`), node('span', 'detail-status', session.status));
  header.append(back, headerLine, node('h1', 'detail-title', session.title));
  const metadata = node('div', 'detail-metadata');
  metadata.append(node('span', '', session.provider), node('span', 'metadata-dot', '·'), node('time', '', `Updated ${formatTime(session.updatedAt)}`));
  header.append(metadata);
  elements.detailContent.append(header);

  renderWaiting({ session, decisions, decisionsError });

  const eventHeading = node('div', 'events-heading');
  const eventCount = events.length === limits.MAX_EVENTS ? `Latest ${limits.MAX_EVENTS} · limited` : `${events.length} events`;
  eventHeading.append(node('h2', '', 'Recent activity'), node('span', 'event-count', eventCount));
  elements.detailContent.append(eventHeading);
  if (hasOlder || olderError) {
    const controls = node('div', 'older-controls');
    if (hasOlder) {
      const olderButton = node('button', 'older-button', olderLoading ? 'Loading older…' : 'Load older');
      olderButton.type = 'button';
      olderButton.disabled = olderLoading;
      olderButton.setAttribute('aria-busy', olderLoading ? 'true' : 'false');
      olderButton.addEventListener('click', loadOlderEvents);
      controls.append(olderButton);
    }
    if (olderError) controls.append(node('p', 'older-error', olderError));
    elements.detailContent.append(controls);
  }
  if (events.length === 0) {
    elements.detailContent.append(node('p', 'empty-state detail-empty', 'No saved activity for this session yet.'));
    return;
  }
  const list = node('ol', 'event-list');
  list.setAttribute('aria-label', 'Recent session activity');
  for (const event of events) {
    const item = node('li', `event event-${event.kind}`);
    item.dataset.eventId = event.id;
    const meta = node('div', 'event-meta');
    meta.append(node('span', 'event-kind', event.kind === 'tool' ? (event.toolName || 'Tool') : event.kind));
    meta.append(node('time', '', formatTime(event.createdAt)));
    const content = node('pre', 'event-text', event.text);
    item.append(meta, content);
    list.append(item);
  }
  elements.detailContent.append(list);
}

function renderDetailPreservingScroll() {
  const scrollY = window.scrollY;
  renderDetail();
  window.scrollTo(0, scrollY);
}

async function loadOlderEvents() {
  const detail = state.detail;
  if (!detail || !detail.hasOlder || olderLoading) return;
  const oldest = detail.events[0];
  if (!oldest) return;
  const requestGeneration = detailRequestGeneration;
  const sessionId = state.selectedId;
  const anchorTop = eventElement(oldest.id)?.getBoundingClientRect().top ?? null;
  olderLoading = true;
  olderError = '';
  renderDetailPreservingScroll();
  try {
    const page = await api.getOlderHistory(sessionId, { sequence: oldest.sequence, id: oldest.id });
    if (requestGeneration !== detailRequestGeneration || state.screen !== 'detail' || state.selectedId !== sessionId || !state.detail) return;
    state.detail.events = mergeEvents(page.events, state.detail.events);
    state.detail.hasOlder = page.hasOlder;
    olderLoading = false;
    renderDetail();
    if (anchorTop !== null) {
      const anchor = eventElement(oldest.id);
      if (anchor) window.scrollBy(0, anchor.getBoundingClientRect().top - anchorTop);
    }
  } catch (error) {
    if (requestGeneration !== detailRequestGeneration || state.screen !== 'detail' || state.selectedId !== sessionId) return;
    olderLoading = false;
    olderError = error instanceof ApiError ? error.message : 'Could not load older activity. Retry.';
    renderDetailPreservingScroll();
  }
}

async function refreshDetail() {
  const id = state.selectedId;
  if (state.screen !== 'detail' || !id || state.detailLoading) return;
  const requestGeneration = detailRequestGeneration;
  try {
    const detail = await api.getSession(id);
    if (requestGeneration !== detailRequestGeneration || state.screen !== 'detail' || state.selectedId !== id) return;
    const previous = state.detail;
    state.detail = previous && previous.session.id === detail.session.id
      ? { ...detail, events: mergeEvents(previous.events, detail.events), hasOlder: previous.hasOlder && detail.hasOlder }
      : detail;
    state.detailError = '';
    renderDetailPreservingScroll();
    setConnection('connected', `Updated ${formatClock()}`);
  } catch (error) {
    if (requestGeneration !== detailRequestGeneration || state.screen !== 'detail' || state.selectedId !== id) return;
    const message = error instanceof ApiError ? error.message : 'Could not refresh session detail.';
    setConnection('error', message);
    throw error;
  }
}

async function openSession(id, row, pushRoute = true) {
  if (state.detailLoading) return;
  const requestGeneration = ++detailRequestGeneration;
  state.selectedId = id;
  state.listScrollTop = elements.sessionList.scrollTop;
  state.screen = 'detail';
  state.detail = null;
  state.detailError = '';
  state.detailLoading = true;
  olderLoading = false;
  olderError = '';
  if (row) row.focus({ preventScroll: true });
  if (pushRoute) history.pushState({ rsiRemoteDetail: id }, '', `#session/${encodeURIComponent(id)}`);
  showScreen();
  detailPoller.stop();
  detailPoller.start();
  elements.detailState.hidden = false;
  elements.detailState.className = 'detail-state';
  elements.detailState.textContent = 'Loading recent activity…';
  try {
    const detail = await api.getSession(id);
    if (requestGeneration !== detailRequestGeneration || state.screen !== 'detail' || state.selectedId !== id) return;
    state.detail = detail;
    state.detailError = '';
  } catch (error) {
    if (requestGeneration !== detailRequestGeneration || state.screen !== 'detail' || state.selectedId !== id) return;
    state.detailError = error instanceof ApiError ? error.message : 'Could not load session detail. Retry.';
  } finally {
    if (requestGeneration !== detailRequestGeneration) return;
    state.detailLoading = false;
    renderDetailState();
    renderDetail();
  }
}

function renderDetailState() {
  elements.detailState.replaceChildren();
  if (state.detail) {
    elements.detailState.hidden = true;
    return;
  }
  elements.detailState.hidden = false;
  elements.detailState.className = `detail-state${state.detailError ? ' is-error' : ''}`;
  elements.detailState.append(node('p', '', state.detailError || 'Loading recent activity…'));
  if (state.detailError) {
    const retry = node('button', 'text-button', 'Retry');
    retry.type = 'button';
    retry.addEventListener('click', () => openSession(state.selectedId, null, false));
    elements.detailState.append(retry);
  }
}

function showScreen() {
  const detail = state.screen === 'detail';
  elements.listPane.hidden = detail;
  elements.detailPane.hidden = !detail;
  elements.listPane.classList.toggle('is-active', !detail);
  elements.detailPane.classList.toggle('is-active', detail);
  if (!detail) renderList();
}

function closeSession(pushHistory) {
  if (state.screen !== 'detail') return;
  detailPoller.stop();
  detailRequestGeneration += 1;
  if (pushHistory && location.hash.startsWith('#session/')) history.back();
  state.screen = 'list';
  state.detailLoading = false;
  olderLoading = false;
  olderError = '';
  state.detail = null;
  showScreen();
  requestAnimationFrame(() => {
    elements.sessionList.scrollTop = state.listScrollTop;
    const selected = [...elements.sessionList.querySelectorAll('.session-row')].find((element) => element.getAttribute('aria-current') === 'true');
    selected?.focus({ preventScroll: true });
  });
}

window.addEventListener('popstate', () => {
  if (state.screen === 'detail') closeSession(false);
});
document.addEventListener('visibilitychange', () => {
  listPoller.onVisibilityChange();
  detailPoller.onVisibilityChange();
});
elements.refresh.addEventListener('click', () => {
  listPoller.kick();
  detailPoller.kick();
});
elements.search.addEventListener('input', renderList);
elements.search.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') elements.search.blur();
});
document.addEventListener('keydown', (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k' && !event.defaultPrevented) {
    event.preventDefault();
    elements.search.focus();
  }
  if (event.key === 'Escape' && state.screen === 'detail') closeSession(true);
});

showScreen();
setListState('Loading sessions…');
listPoller.start();
