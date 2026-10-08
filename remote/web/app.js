import { ApiError, createApi, limits } from './api.js';
import { createDraftStore, createPoller, createViewState, describeDraft, describePendingReceipt, enterDetail, groupSessions, returnToList, submitDraft } from './state.js';

const api = createApi();
const state = createViewState();
const drafts = createDraftStore();
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
  announce: document.querySelector('#announce'),
  footnoteMode: document.querySelector('#footnote-mode'),
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

function announce(message) {
  elements.announce.textContent = '';
  elements.announce.textContent = message;
}

const isAnswerable = (target) => target.kind === 'question' || target.options.length > 0;

function renderAnswerCard(target, draft, fence) {
  const item = node('li', `decision decision-${target.kind} answer-card`);
  const head = node('div', 'decision-head');
  head.append(node('span', `decision-kind decision-kind-${target.kind}`, target.kind === 'question' ? 'Question' : 'Approval'));
  head.append(node('span', 'decision-title', target.title));
  item.append(head);
  if (target.detail) item.append(node('p', 'decision-detail', target.detail));
  if (target.receipt) {
    item.append(node('p', 'answer-note', describePendingReceipt(target.receipt).message));
    return item;
  }
  if (!draft) {
    item.append(node('p', 'answer-note', 'No answer choices were offered for this approval. Answer it on your RSI host.'));
    return item;
  }
  const view = describeDraft(draft);
  const mark = (element, field) => {
    element.dataset.draftKey = draft.decisionKey;
    element.dataset.draftField = field;
    return element;
  };
  const rerender = () => renderDetailPreservingScroll();
  const group = node('div', 'answer-form');
  group.setAttribute('role', 'group');
  group.setAttribute('aria-label', `Answer: ${target.title}`);
  if (target.options.length) {
    const choices = node('div', 'answer-options');
    target.options.forEach((option, index) => {
      const button = mark(node('button', `option-button${draft.text === option ? ' is-picked' : ''}`, option), `option-${index}`);
      button.type = 'button';
      button.disabled = !view.canEdit;
      button.setAttribute('aria-pressed', draft.text === option ? 'true' : 'false');
      button.addEventListener('click', () => {
        drafts.setText(draft, option);
        rerender();
      });
      choices.append(button);
    });
    group.append(choices);
  }
  if (target.kind === 'question') {
    const label = node('label', 'answer-label');
    label.append(node('span', 'answer-label-text', target.options.length ? 'Or write your own answer' : 'Your answer'));
    const field = mark(node('textarea', 'answer-text'), 'text');
    field.rows = 3;
    field.maxLength = 2048;
    field.value = draft.text;
    field.readOnly = !view.canEdit;
    field.autocomplete = 'off';
    field.addEventListener('input', () => drafts.setText(draft, field.value));
    label.append(field);
    group.append(label);
  }
  const actions = node('div', 'answer-actions');
  if (view.canRetry) {
    const retry = mark(node('button', 'primary-button', 'Retry same answer'), 'retry');
    retry.type = 'button';
    retry.addEventListener('click', () => sendDraft(draft));
    const refresh = mark(node('button', 'text-button', 'Refresh'), 'refresh');
    refresh.type = 'button';
    refresh.addEventListener('click', () => detailPoller.kick());
    actions.append(retry, refresh);
  } else if (view.canDismiss) {
    const done = mark(node('button', 'text-button', 'Done'), 'dismiss');
    done.type = 'button';
    done.addEventListener('click', () => {
      drafts.dismiss(draft);
      rerender();
    });
    actions.append(done);
  } else {
    const send = mark(node('button', 'primary-button', draft.status === 'sending' ? 'Sending…' : 'Send answer'), 'send');
    send.type = 'button';
    send.disabled = !view.canSend || !fence;
    send.setAttribute('aria-busy', draft.status === 'sending' ? 'true' : 'false');
    send.addEventListener('click', () => sendDraft(draft));
    actions.append(send);
  }
  group.append(actions);
  if (view.message) {
    const status = node('p', `answer-status answer-status-${view.tone}`, view.message);
    status.setAttribute('role', 'status');
    group.append(status);
  }
  item.append(group);
  return item;
}

async function sendDraft(draft) {
  const fence = state.detail?.targets?.fence ?? null;
  const sending = submitDraft({ api, store: drafts, draft, fence });
  renderDetailPreservingScroll();
  const status = await sending;
  const failed = draft.error;
  announce(status === 'receipt' ? 'Answer sent to your RSI host.' : failed ? failed.message : 'This decision changed. Refreshing.');
  if (state.screen === 'detail' && state.selectedId === draft.sessionId) {
    renderDetailPreservingScroll();
    // A receipt, an unknown outcome or a changed decision are all settled by
    // asking the host again; no optimistic "answered" state is shown.
    if (status === 'receipt' || status === 'unknown' || !failed) detailPoller.kick();
  }
}

function renderWaiting({ session, decisions, decisionsError, targets }) {
  const open = Array.isArray(decisions) ? decisions.filter((decision) => decision.open) : [];
  const items = targets?.ready ? targets.items : [];
  const sentDrafts = drafts.forSession(session.id).filter((draft) => draft.status !== 'editing' && !items.some((item) => item.decisionKey === draft.decisionKey));
  if (!decisionsError && !session.attentionIncomplete && open.length === 0 && items.length === 0 && sentDrafts.length === 0) return;
  const section = node('section', 'waiting-section');
  section.append(node('h2', 'waiting-title', 'Waiting on your RSI host'));
  if (decisionsError) section.append(node('p', 'waiting-error', decisionsError));
  const answerable = new Set(items.map((item) => item.decisionKey));
  const readOnly = open.filter((decision) => !answerable.has(decision.id));
  if (items.length || sentDrafts.length || readOnly.length) {
    const list = node('ul', 'decision-list');
    list.setAttribute('aria-label', 'Decisions waiting on the RSI host');
    for (const target of items) list.append(renderAnswerCard(target, isAnswerable(target) ? drafts.ensure(session.id, target) : null, targets.fence));
    // An answer already sent stays visible after the decision stops waiting,
    // so its receipt or unknown outcome is not lost to a poll.
    for (const draft of sentDrafts) list.append(renderAnswerCard(draft, draft, targets?.fence ?? null));
    for (const decision of readOnly) {
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
  if (readOnly.length) section.append(node('p', 'waiting-note', items.length ? 'Decisions without an answer form must be answered on your RSI host.' : 'Answer these on your RSI host; this view is read-only.'));
  if (targets?.state === 'error') section.append(node('p', 'waiting-incomplete', 'Could not refresh the answer forms; showing the last known ones.'));
  if (session.attentionIncomplete) {
    section.append(node('p', 'waiting-incomplete', 'Some sources could not be checked; the list may be incomplete.'));
  }
  elements.detailContent.append(section);
}

// The poll rebuilds the page, so the focused draft control and the caret are
// put back afterwards; the draft text itself lives in the draft store.
function renderDetail() {
  const active = document.activeElement;
  const focus = active?.dataset?.draftField
    ? { key: active.dataset.draftKey, field: active.dataset.draftField, start: active.selectionStart, end: active.selectionEnd }
    : null;
  renderDetailBody();
  elements.footnoteMode.textContent = state.detail?.targets?.ready ? 'You can answer waiting decisions' : 'Read only';
  if (!focus) return;
  const again = [...elements.detailContent.querySelectorAll('[data-draft-field]')]
    .find((element) => element.dataset.draftKey === focus.key && element.dataset.draftField === focus.field);
  if (!again || again.disabled) return;
  again.focus({ preventScroll: true });
  if (focus.field === 'text' && focus.start !== null) again.setSelectionRange(focus.start, focus.end);
}

function renderDetailBody() {
  elements.detailContent.replaceChildren();
  if (!state.detail) return;
  const { session, events, decisions, decisionsError, targets, hasOlder } = state.detail;
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

  renderWaiting({ session, decisions, decisionsError, targets });

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
    const sameSession = previous && previous.session.id === detail.session.id;
    // A failed targets read keeps the last good forms so a draft is not pulled
    // out from under the operator by one bad poll.
    const targets = detail.targets.state === 'error' && sameSession && previous.targets?.ready
      ? { ...previous.targets, state: 'error' }
      : detail.targets;
    if (targets.state === 'ok') drafts.sync(id, targets.items);
    state.detail = sameSession
      ? { ...detail, targets, events: mergeEvents(previous.events, detail.events), hasOlder: previous.hasOlder && detail.hasOlder }
      : { ...detail, targets };
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
    if (detail.targets.state === 'ok') drafts.sync(id, detail.targets.items);
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
  elements.footnoteMode.textContent = 'Read only';
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
