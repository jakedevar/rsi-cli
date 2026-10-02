const MAX_BODY_BYTES = 1_048_576;
const MAX_PROJECTS = 32;
const MAX_SESSIONS = 500;
const MAX_EVENTS = 50;
const MAX_SESSION_PAGES = 4;
const MAX_DECISIONS = 16;
const MAX_TEXT_BYTES = 64 * 1024;
const MAX_TOTAL_EVENT_BYTES = 512 * 1024;
const STATUSES = new Set(['Starting', 'Running', 'WaitingApproval', 'Completed', 'Failed', 'Interrupted', 'Archived', 'Deleted']);
const PROVIDERS = new Set(['Claude', 'Codex', 'Pioneer', 'OpenRouter', 'Bedrock', 'Local', 'Antigravity', 'CodexAppServer', 'Harness']);
const EVENT_KINDS = new Set(['user', 'assistant', 'tool', 'system', 'error', 'thinking']);
const DECISION_KINDS = new Set(['generic_questions', 'native_approval', 'legacy_approval']);
const DISPLAY_STATES = new Set(['complete', 'truncated', 'unavailable']);
const CURSOR_TOKEN = /^[A-Za-z0-9_-]{1,1024}$/;
const EVENT_ID = /^[1-9][0-9]*$/;

// Neutral display values used when the gateway reports an enum this UI does not
// know. The internal session shape must stay within the known sets above, so an
// unrecognized status or provider is shown as a quiet, existing value.
const NEUTRAL_STATUS = 'Archived';
const NEUTRAL_PROVIDER = 'Local';

export class ApiError extends Error {
  constructor(message, status = 0) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

function requireRecord(value, label) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new ApiError(`Invalid ${label} response.`);
  }
  return value;
}

function requireString(value, label, maxBytes = 4096) {
  if (typeof value !== 'string' || new TextEncoder().encode(value).byteLength > maxBytes) {
    throw new ApiError(`Invalid ${label} in response.`);
  }
  return value;
}

function statusError(status) {
  if (status === 401) return new ApiError('Your session has expired. Reload the page to sign in again.', 401);
  if (status === 403) return new ApiError('The gateway denied this request. Check that this phone is allowed and the gateway is ready, then reload.', 403);
  if (status === 502) return new ApiError('The RSI daemon is unavailable. Check that rsid is running on the host.', 502);
  return new ApiError(`The RSI host returned ${status}. Retry when it is available.`, status);
}

// A V1 list page carries `next_cursor`: either null (this is the last page) or
// `{kind, token}` for the same route. A missing value is treated as no cursor so
// responses that predate the continuation field still resolve, while a present
// but malformed cursor is refused instead of followed blindly.
function readCursor(value, kind, label) {
  if (value === null || value === undefined) return null;
  const cursor = requireRecord(value, `${label} cursor`);
  const cursorKind = requireString(cursor.kind, `${label} cursor kind`, 32);
  const token = requireString(cursor.token, `${label} cursor token`, 1024);
  if (cursorKind !== kind || !CURSOR_TOKEN.test(token)) throw new ApiError(`Invalid ${label} cursor in response.`);
  return token;
}

// V1 tagged enums are `{state:"known",value:...}` or `{state:"unknown",label:...}`.
function taggedValue(value, allowed, label, fallback) {
  const tagged = requireRecord(value, label);
  const text = tagged.state === 'known' ? requireString(tagged.value, `${label} value`, 128)
    : tagged.state === 'unknown' ? requireString(tagged.label, `${label} label`, 128)
      : null;
  if (text === null) throw new ApiError(`Invalid ${label} in response.`);
  return allowed.has(text) ? text : fallback;
}

function mapSummary(value) {
  const summary = requireRecord(value, 'session');
  const attention = requireRecord(summary.attention, 'session attention');
  if (typeof attention.requires_local_action !== 'boolean' || typeof attention.incomplete !== 'boolean') {
    throw new ApiError('Invalid session attention in response.');
  }
  const session = {
    id: requireString(summary.id, 'session id', 128),
    title: requireString(summary.own_title, 'session title', 512),
    status: taggedValue(summary.status, STATUSES, 'session status', NEUTRAL_STATUS),
    provider: taggedValue(summary.provider, PROVIDERS, 'session provider', NEUTRAL_PROVIDER),
    updatedAt: requireString(summary.updated_at, 'session updated time', 64),
    needsYou: attention.requires_local_action === true,
    attentionIncomplete: attention.incomplete === true,
  };
  if (!Number.isFinite(Date.parse(session.updatedAt))) throw new ApiError('Invalid session updated time in response.');
  return { session, projectId: requireString(summary.project_id, 'session project id', 128) };
}

function eventKind(value, role) {
  const kind = requireRecord(value, 'event kind');
  if (kind.state === 'unknown') return 'system';
  if (kind.state !== 'known') throw new ApiError('Invalid event kind in response.');
  const name = requireString(kind.value, 'event kind value', 128);
  if (name === 'Message') {
    if (role === null || role === undefined) return 'system';
    const tagged = requireRecord(role, 'event role');
    if (tagged.state === 'known' && tagged.value === 'User') return 'user';
    if (tagged.state === 'known' && tagged.value === 'Assistant') return 'assistant';
    return 'system';
  }
  if (name === 'ToolUse' || name === 'ToolResult') return 'tool';
  if (name === 'System' || name === 'Compressed') return 'system';
  if (name === 'Thinking') return 'thinking';
  return 'system';
}

function mapEvent(value) {
  const event = requireRecord(value, 'event');
  const kind = eventKind(event.kind, event.role);
  let text = requireString(event.text, 'event text', MAX_TEXT_BYTES);
  let toolName;
  if (kind === 'tool' && event.tool_name !== null && event.tool_name !== undefined) {
    toolName = requireString(event.tool_name, 'tool name', 512);
    text = `${toolName}: ${text}`;
  }
  return {
    id: requireString(event.id, 'event id', 128),
    sequence: event.sequence,
    kind,
    createdAt: requireString(event.created_at, 'event time', 64),
    text,
    ...(toolName === undefined ? {} : { toolName }),
  };
}

function parseSession(value) {
  const session = requireRecord(value, 'session');
  const id = requireString(session.id, 'session id', 128);
  const title = requireString(session.title, 'session title', 512);
  const status = requireString(session.status, 'session status', 64);
  const provider = requireString(session.provider, 'session provider', 64);
  const updatedAt = requireString(session.updatedAt, 'session updated time', 64);
  const { needsYou, attentionIncomplete } = session;
  if (!STATUSES.has(status) || !PROVIDERS.has(provider) || !Number.isFinite(Date.parse(updatedAt))) {
    throw new ApiError('Invalid session status, provider, or updated time in response.');
  }
  if (typeof needsYou !== 'boolean' || typeof attentionIncomplete !== 'boolean') {
    throw new ApiError('Invalid session attention in response.');
  }
  return { id, title, status, provider, updatedAt, needsYou, attentionIncomplete };
}

function parseEvents(value) {
  if (!Array.isArray(value) || value.length > MAX_EVENTS) throw new ApiError('Invalid or oversized event list.');
  let totalBytes = 0;
  return value.map((item) => {
    const event = requireRecord(item, 'event');
    const id = requireString(event.id, 'event id', 128);
    const sequence = event.sequence;
    if (!Number.isInteger(sequence)) throw new ApiError('Invalid event sequence in response.');
    const kind = requireString(event.kind, 'event kind', 32);
    const createdAt = requireString(event.createdAt, 'event time', 64);
    const text = requireString(event.text, 'event text', MAX_TEXT_BYTES);
    if (!EVENT_KINDS.has(kind) || !Number.isFinite(Date.parse(createdAt))) throw new ApiError('Invalid event kind or time in response.');
    totalBytes += new TextEncoder().encode(text).byteLength;
    if (totalBytes > MAX_TOTAL_EVENT_BYTES) throw new ApiError('Event history exceeds the display limit.');
    const toolName = event.toolName === undefined ? undefined : requireString(event.toolName, 'tool name', 256);
    return { id, sequence, kind, createdAt, text, ...(toolName === undefined ? {} : { toolName }) };
  });
}

// A V1 display field is preview text plus how complete that preview is. A
// truncated preview is marked so the operator knows more exists; an unavailable
// field is shown as an inert placeholder instead of its (empty) text.
function mapDisplayText(value, label) {
  const field = requireRecord(value, label);
  const text = requireString(field.text, `${label} text`, MAX_TEXT_BYTES);
  const state = requireString(field.state, `${label} state`, 32);
  if (!DISPLAY_STATES.has(state)) throw new ApiError(`Invalid ${label} state in response.`);
  if (state === 'unavailable') return 'Unavailable';
  return state === 'truncated' ? `${text} …` : text;
}

function mapDecision(value) {
  const decision = requireRecord(value, 'decision');
  const id = requireString(decision.id, 'decision id', 160);
  const kind = requireString(decision.kind, 'decision kind', 32);
  const closure = requireString(decision.closure_state, 'decision closure state', 32);
  const display = requireRecord(decision.display, 'decision display');
  if (!DECISION_KINDS.has(kind) || display.kind !== kind) {
    throw new ApiError('Invalid decision kind or display in response.');
  }
  const open = closure !== 'closed';
  if (kind === 'generic_questions') {
    return { id, kind: 'question', title: 'A question is waiting', detail: '', open };
  }
  if (kind === 'native_approval') {
    return { id, kind: 'approval', title: mapDisplayText(display.method, 'decision method'), detail: mapDisplayText(display.description, 'decision description'), open };
  }
  return { id, kind: 'approval', title: mapDisplayText(display.tool_name, 'decision tool name'), detail: '', open };
}

function parseDecisions(value) {
  const document = requireRecord(value, 'decisions');
  if (!Array.isArray(document.items)) throw new ApiError('Invalid or oversized decision list.');
  const items = document.items.length > MAX_DECISIONS ? document.items.slice(0, MAX_DECISIONS) : document.items;
  return items.map(mapDecision);
}

async function readBody(response) {
  const declaredLength = Number(response.headers.get('Content-Length') || 0);
  if (declaredLength > MAX_BODY_BYTES) throw new ApiError('Response is too large to display.');
  const contentType = response.headers.get('Content-Type') || '';
  if (contentType && !/json/i.test(contentType)) throw new ApiError('The RSI host sent an unreadable response.');
  let body;
  try {
    if (response.body?.getReader) {
      const reader = response.body.getReader();
      const decoder = new TextDecoder('utf-8', { fatal: true });
      let size = 0;
      let decoded = '';
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        size += value.byteLength;
        if (size > MAX_BODY_BYTES) {
          await reader.cancel();
          throw new ApiError('Response is too large to display.');
        }
        decoded += decoder.decode(value, { stream: true });
      }
      body = decoded + decoder.decode();
    } else {
      body = await response.text();
      if (new TextEncoder().encode(body).byteLength > MAX_BODY_BYTES) throw new ApiError('Response is too large to display.');
    }
  } catch (error) {
    if (error instanceof ApiError) throw error;
    throw new ApiError('The RSI host sent an unreadable response.');
  }
  try {
    return JSON.parse(body);
  } catch {
    throw new ApiError('The RSI host sent invalid JSON.');
  }
}

async function send(fetchImpl, path, init) {
  try {
    return await fetchImpl(path, init);
  } catch {
    throw new ApiError('Could not reach the RSI host. Check the connection and retry.');
  }
}

const GET_INIT = Object.freeze({ method: 'GET', headers: Object.freeze({ Accept: 'application/json' }), cache: 'no-store' });

export function createApi(fetchImpl = globalThis.fetch.bind(globalThis)) {
  // CSRF stays in memory for the lifetime of this closure; never persisted.
  let csrf = null;
  const projectBySession = new Map();

  async function establishSession() {
    const bootstrapResponse = await send(fetchImpl, '/auth/bootstrap', { ...GET_INIT });
    if (!bootstrapResponse.ok) throw statusError(bootstrapResponse.status);
    const bootstrap = requireRecord(await readBody(bootstrapResponse), 'bootstrap');
    const nonce = requireString(bootstrap.nonce, 'nonce', 256);
    const sessionResponse = await send(fetchImpl, '/auth/session', {
      method: 'POST',
      headers: { Accept: 'application/json', 'content-type': 'application/json' },
      cache: 'no-store',
      body: JSON.stringify({ nonce }),
    });
    if (!sessionResponse.ok) throw statusError(sessionResponse.status);
    const session = requireRecord(await readBody(sessionResponse), 'session');
    csrf = requireString(session.csrf, 'csrf', 256);
  }

  async function dataGet(path) {
    if (csrf === null) await establishSession();
    let response = await send(fetchImpl, path, { ...GET_INIT });
    if (response.status === 401) {
      await establishSession();
      response = await send(fetchImpl, path, { ...GET_INIT });
      if (response.status === 401) throw statusError(401);
    }
    if (!response.ok) throw statusError(response.status);
    return readBody(response);
  }

  return {
    async listSessions() {
      const document = requireRecord(await dataGet('/api/v1/projects'), 'project list');
      if (!Array.isArray(document.items) || document.items.length > MAX_PROJECTS) {
        throw new ApiError('Invalid or oversized project list.');
      }
      projectBySession.clear();
      const sessions = [];
      const ids = new Set();
      const absorb = (items) => {
        for (const raw of items) {
          const { session, projectId: owner } = mapSummary(raw);
          if (ids.has(session.id)) throw new ApiError('Session list contains a duplicate identity.');
          if (sessions.length >= MAX_SESSIONS) throw new ApiError('Invalid or oversized session list.');
          ids.add(session.id);
          sessions.push(parseSession(session));
          projectBySession.set(session.id, owner);
        }
      };
      for (const rawProject of document.items) {
        const project = requireRecord(rawProject, 'project');
        const projectId = requireString(project.id, 'project id', 128);
        const base = `/api/v1/projects/${encodeURIComponent(projectId)}/sessions`;
        let listing = requireRecord(await dataGet(base), 'session list');
        for (let page = 0; ; page += 1) {
          if (!Array.isArray(listing.items)) throw new ApiError('Invalid or oversized session list.');
          absorb(listing.items);
          const token = readCursor(listing.next_cursor, 'sessions', 'session list');
          if (token === null || page + 1 >= MAX_SESSION_PAGES) break;
          listing = requireRecord(await dataGet(`${base}?cursor=${encodeURIComponent(token)}`), 'session list');
        }
      }
      return sessions;
    },
    async getSession(id) {
      const projectId = projectBySession.get(id);
      if (projectId === undefined) throw new ApiError('Refresh the session list before opening a session.');
      const base = `/api/v1/projects/${encodeURIComponent(projectId)}/sessions/${encodeURIComponent(id)}`;
      const detail = requireRecord(await dataGet(base), 'session detail');
      const { session } = mapSummary(requireRecord(detail.item, 'session detail').summary);
      if (session.id !== id) throw new ApiError('The host returned a different session.');
      const history = requireRecord(await dataGet(`${base}/history`), 'session history');
      if (!Array.isArray(history.items)) throw new ApiError('Invalid or oversized event list.');
      const window = history.items.length > MAX_EVENTS ? history.items.slice(history.items.length - MAX_EVENTS) : history.items;
      // Waiting decisions are a read-only projection: a failure here degrades
      // the detail view with a quiet inline message instead of hiding activity.
      let decisions = null;
      let decisionsError = '';
      try {
        decisions = parseDecisions(await dataGet(`${base}/decisions`));
      } catch (error) {
        decisions = null;
        decisionsError = error instanceof ApiError ? error.message : 'Could not load waiting decisions.';
      }
      return { session: parseSession(session), events: parseEvents(window.map(mapEvent)), decisions, decisionsError, hasOlder: history.items.length === 25 };
    },
    async getOlderHistory(id, before) {
      const projectId = projectBySession.get(id);
      if (projectId === undefined) throw new ApiError('Refresh the session list before opening a session.');
      if (before === null || typeof before !== 'object' || Array.isArray(before)) throw new ApiError('Invalid history anchor.');
      if (!Number.isInteger(before.sequence) || typeof before.id !== 'string' || !EVENT_ID.test(before.id)) {
        throw new ApiError('Invalid history anchor.');
      }
      const base = `/api/v1/projects/${encodeURIComponent(projectId)}/sessions/${encodeURIComponent(id)}`;
      const history = requireRecord(await dataGet(`${base}/history?before=${before.sequence}:${before.id}`), 'session history');
      if (!Array.isArray(history.items)) throw new ApiError('Invalid or oversized event list.');
      return { events: parseEvents(history.items.map(mapEvent)), hasOlder: history.items.length === 25 };
    },
    async logout() {
      if (csrf === null) return;
      const response = await send(fetchImpl, '/auth/logout', {
        method: 'POST',
        headers: { Accept: 'application/json', 'x-rsi-csrf': csrf },
        cache: 'no-store',
      });
      if (!response.ok) throw statusError(response.status);
      csrf = null;
    },
  };
}

export const limits = Object.freeze({ MAX_BODY_BYTES, MAX_PROJECTS, MAX_SESSIONS, MAX_EVENTS, MAX_DECISIONS, MAX_TEXT_BYTES, MAX_TOTAL_EVENT_BYTES });
