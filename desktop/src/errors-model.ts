// One error presentation for the whole desktop (local #207, from the #205 audit).
//
// The backend hands every failed daemon call to the webview as one string
// (src-tauri/src/daemon.rs `impl Display for DaemonError`):
//   cannot reach rsid: <socket>: <os error>
//   daemon did not answer in time
//   daemon i/o error: <e> | daemon response too large | malformed daemon response: <e>
//   daemon error <json-rpc code>: <message>[ | next_action: <text>][ [<typed code>]]
// `plainError` turns that into one plain sentence plus a next step, and keeps
// the raw string as `detail` (copyable, for bug reports). `presentError` does
// the same for a composed message such as `Launch failed: daemon error ...`,
// which is what the ~150 call sites hand to toast/showError/inline panels; the
// DOM side (dom.ts) renders `text` and a collapsible Details section.
// Nothing is dropped: the raw text is always kept in `detail`. Credentials are
// redacted from both.

import { RESUME_LABEL, RESUME_SETTINGS_PATH } from './emergency-model.js';

export type ErrorClass =
  | 'unreachable' // rsid is not running or the connection broke
  | 'timeout' // rsid did not answer in time
  | 'restarting' // rsid is draining for a deploy or restart
  | 'outdated' // rsid does not know the method: the app and daemon versions differ
  | 'unreadable' // the reply was malformed or too large
  | 'stale' // someone else changed the row first (stale_version)
  | 'not_found' // the target does not exist (any more)
  | 'denied' // a permission or authority refusal
  | 'invalid' // the request was refused as invalid
  | 'refused' // any other typed refusal code
  | 'unknown' // a daemon error with no code we recognise
  | 'internal' // the app's own code threw while reading an answer (TypeError: Cannot read properties of null ...)
  | 'local'; // not a daemon error at all (a client-side message)

export interface DaemonErrorParts {
  /** The JSON-RPC error code (-32000, -32601, ...), when the daemon answered. */
  rpcCode: number | null;
  /** The daemon's message without the prefix, the next action or the code. */
  message: string;
  /** The typed refusal code (`stale_version`, `agent_issue_authority_denied`, ...). */
  code: string | null;
  /** The daemon's own next step (`data.next_action`), when it sent one. */
  nextAction: string | null;
}

export interface PlainError {
  kind: ErrorClass;
  /** One plain sentence: what happened. */
  text: string;
  /** What to do next ("" when there is nothing useful to say). */
  next: string;
  /** The raw error, credentials redacted: shown under Details and copyable. */
  detail: string;
  code: string | null;
  rpcCode: number | null;
}

/** Text a person reads: the sentence and its next step. */
export function errorSentence(p: Pick<PlainError, 'text' | 'next'>): string {
  return p.next ? `${p.text} ${p.next}` : p.text;
}

// ---------- credentials ----------

const SECRET_PATTERNS: readonly RegExp[] = [
  /\bsk-(?:ant-|proj-|or-)?[A-Za-z0-9_-]{12,}/g, // Anthropic / OpenAI / OpenRouter keys
  /\bgh[pousr]_[A-Za-z0-9]{20,}/g, // GitHub tokens
  /\bgithub_pat_[A-Za-z0-9_]{20,}/g,
  /\b(?:AKIA|ASIA)[0-9A-Z]{16}\b/g, // AWS access key ids
  /\bxox[abprs]-[A-Za-z0-9-]{10,}/g, // Slack
  /\bAIza[0-9A-Za-z_-]{30,}/g, // Google API keys
  /\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{5,}/g, // JWTs
];
/** `token=…`, `api_key: …`, `"password":"…"`, `Bearer …`, `RSI_SESSION_TOKEN=…`. */
const SECRET_ASSIGNMENT =
  /\b((?:[A-Za-z_]*_)?(?:token|api[_-]?key|secret|password|passwd|credential|authorization)s?["']?\s*[:=]\s*["']?)([^\s"',;)}\]]{4,})/gi;
const BEARER = /\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{8,}/g;

/** Replace anything that looks like a credential with `[redacted]`. */
export function redactCredentials(s: string): string {
  let out = s;
  for (const p of SECRET_PATTERNS) out = out.replace(p, '[redacted]');
  out = out.replace(BEARER, '$1 [redacted]');
  out = out.replace(SECRET_ASSIGNMENT, (_m, key: string, value: string) => (value === '[redacted]' ? `${key}${value}` : `${key}[redacted]`));
  return out;
}

// ---------- parsing ----------

const RPC_PREFIX = /^(?:Error:\s*)?daemon error (-?\d+):\s*/;
const TRAILING_CODE = /\s*\[([a-z][a-z0-9_]*)\]\s*$/;
const NEXT_ACTION = /\s*\|\s*next_action:\s*([\s\S]*)$/;
/** A leading `snake_case_code: message` (most rsid refusals name their code first). */
const LEADING_CODE = /^([a-z][a-z0-9]*(?:_[a-z0-9]+)+)(?::\s*|\s*$)/;

/** Where a daemon error starts inside a composed message, or -1. */
const MARKER =
  /(?:Error:\s*)?(?:daemon error -?\d+:|cannot reach rsid|daemon did not answer in time|daemon i\/o error|daemon response too large|malformed daemon response|rsid's socket transport is Unix-only)/;

/** A JavaScript runtime fault by name (`TypeError: ...`), as `String(e)` and the page's error events write it. */
const JS_FAULT_PREFIX = /(?:Uncaught\s+)?\b(?:TypeError|ReferenceError|RangeError|SyntaxError|EvalError|URIError):\s*/;
/** The same faults without the name (`e.message` of a rejection): V8 and WebKit wording. */
const JS_FAULT_BARE =
  /^(?:Cannot (?:read|set|destructure) propert(?:y|ies) of (?:null|undefined)|(?:undefined|null) is not an object|(?:undefined|null) is not iterable|[\w$.[\]'"]+ is not (?:a function|defined|iterable|a constructor)\b)/;

/** True for a message that is the app's own code failing, not a daemon answer or a person's input. */
export function isScriptFault(raw: string): boolean {
  const s = raw.trim().replace(/^Error:\s*/, '');
  return JS_FAULT_PREFIX.test(s) || JS_FAULT_BARE.test(s);
}

const INTERNAL: Omit<PlainError, 'detail' | 'code' | 'rpcCode'> = {
  kind: 'internal',
  text: "The app could not read rsid's answer.",
  next: 'Try again; if it keeps happening, copy the Details into a bug report.',
};

function rawOf(e: unknown): string {
  if (typeof e === 'string') return e;
  if (e instanceof Error) return e.message;
  if (e && typeof e === 'object' && typeof (e as { message?: unknown }).message === 'string') {
    return (e as { message: string }).message;
  }
  return String(e);
}

/** Split a `daemon error <n>: msg | next_action: x [code]` string; null when it is not one. */
export function parseDaemonError(raw: string): DaemonErrorParts | null {
  const s = raw.trim();
  const m = s.match(RPC_PREFIX);
  if (!m) return null;
  let rest = s.slice(m[0].length);
  let code: string | null = null;
  const c = rest.match(TRAILING_CODE);
  if (c) {
    code = c[1]!;
    rest = rest.slice(0, c.index);
  }
  let nextAction: string | null = null;
  const n = rest.match(NEXT_ACTION);
  if (n) {
    nextAction = n[1]!.trim() || null;
    rest = rest.slice(0, n.index);
  }
  const lead = rest.match(LEADING_CODE);
  if (lead) {
    code ??= lead[1]!;
    const tail = rest.slice(lead[0].length).trim();
    if (tail) rest = tail;
  }
  return { rpcCode: Number(m[1]), message: rest.trim(), code, nextAction };
}

// ---------- sentences ----------

function sentence(s: string): string {
  const t = s.trim();
  if (!t) return '';
  const capped = t[0]!.toUpperCase() + t.slice(1);
  return /[.!?]$/.test(capped) ? capped : `${capped}.`;
}

/** Quote a daemon message inside our sentence, without doubling its full stop. */
function clause(s: string): string {
  return s.trim().replace(/[.\s]+$/, '');
}

const UNREACHABLE: Omit<PlainError, 'detail' | 'code' | 'rpcCode'> = {
  kind: 'unreachable',
  text: "RSI's background service (rsid) is not running or cannot be reached.",
  next: 'Start rsid, or wait for it to restart; the app reconnects on its own.',
};

/** Known typed codes with their own sentence (the daemon's message goes to Details). */
const KNOWN_CODES: Readonly<Record<string, { kind: ErrorClass; text: string; next: string }>> = {
  stale_version: { kind: 'stale', text: 'Someone else changed this first.', next: 'Refresh and try again.' },
  deploy_draining: { kind: 'restarting', text: 'rsid is restarting for an update, so nothing was changed.', next: 'Try again in a minute.' },
  dependency_cycle: { kind: 'refused', text: 'That dependency would make Issues block each other in a loop.', next: 'Remove one of the dependencies first.' },
  idempotency_conflict: { kind: 'refused', text: 'This retry was already used for a different request.', next: 'Start the action again from the beginning.' },
  no_semantic_change: { kind: 'refused', text: 'Nothing would change, so rsid did not save anything.', next: 'Edit a value first.' },
  invalid_transition: { kind: 'refused', text: 'That status change is not allowed from the current status.', next: 'Refresh to see the current status.' },
  not_terminal: { kind: 'refused', text: 'Only a closed or cancelled Issue can be archived.', next: 'Close or cancel it first.' },
  not_archived: { kind: 'refused', text: 'This Issue is not archived.', next: 'Refresh to see its current state.' },
};

/** rsid's refusal in model-control mode stop_all: "all model work is stopped for {purpose}" (model_control.rs). */
const MODEL_WORK_STOPPED = /\ball model work is stopped\b/i;
/** The step back out of an Emergency stop (the banner button; the same switch as the Settings row). */
export const MODEL_WORK_STOPPED_NEXT = `Choose ${RESUME_LABEL} in the banner (or ${RESUME_SETTINGS_PATH}).`;

function classify(p: DaemonErrorParts): { kind: ErrorClass; text: string; next: string } {
  const code = p.code ?? '';
  const msg = clause(p.message) || 'no reason given';
  const known = KNOWN_CODES[code];
  if (known) return known;
  if (MODEL_WORK_STOPPED.test(p.message)) {
    return { kind: 'refused', text: `AI work is stopped, so rsid refused this: ${msg}.`, next: MODEL_WORK_STOPPED_NEXT };
  }
  if (/_stale$|stale_/.test(code) || /\b(row changed|changed since|stale version)\b/i.test(p.message)) {
    return KNOWN_CODES.stale_version!;
  }
  if (/restart drain|deploy drain|draining/i.test(p.message)) return KNOWN_CODES.deploy_draining!;
  if (p.rpcCode === -32601 || /\b(method not found|unknown method)\b/i.test(p.message)) {
    return { kind: 'outdated', text: 'This version of rsid does not support that action.', next: 'Update rsid and restart it so it matches the app.' };
  }
  if (/denied|forbidden|unauthori[sz]ed|authority|not_permitted|permission/.test(code)
    || /\b(permission denied|not permitted|not authori[sz]ed|forbidden|no authority|lacks? (the )?authority)\b/i.test(p.message)) {
    return { kind: 'denied', text: `rsid refused this action: ${msg}.`, next: 'You do not have permission for it; ask the operator or the manager who owns it.' };
  }
  if (/not_found|^unknown_|_unknown$|missing$/.test(code)
    || /\b(not found|no such|does not exist|unknown (session|issue|project|epic|graph|job|worker|group|manager|todo|prompt|workflow))\b/i.test(p.message)) {
    return { kind: 'not_found', text: `rsid could not find that: ${msg}.`, next: 'Refresh the list; it may have been removed or archived.' };
  }
  if (p.rpcCode === -32602 || p.rpcCode === -32600 || /^invalid_/.test(code)) {
    return { kind: 'invalid', text: `rsid refused the request as invalid: ${msg}.`, next: 'Check the values and try again.' };
  }
  if (code) return { kind: 'refused', text: `rsid refused this action: ${msg}.`, next: 'See Details for the reason.' };
  return { kind: 'unknown', text: `rsid could not do this: ${msg}.`, next: 'If it keeps happening, copy the Details into a bug report.' };
}

/** Any thrown value or error string as a plain sentence, a next step and the raw detail. */
export function plainError(e: unknown): PlainError {
  const raw = rawOf(e).trim();
  const detail = redactCredentials(raw);
  const base = { detail, code: null as string | null, rpcCode: null as number | null };
  const s = raw.replace(/^Error:\s*/, '');
  if (/^cannot reach rsid\b|connection closed before a response|^daemon i\/o error\b/i.test(s)) return { ...UNREACHABLE, ...base };
  if (/^rsid's socket transport is Unix-only/.test(s)) {
    return { kind: 'unreachable', text: 'The app cannot reach rsid on this platform yet.', next: 'Use the rsi TUI on this machine for now.', ...base };
  }
  if (/^daemon did not answer in time\b|\btimed out\b/i.test(s)) {
    return { kind: 'timeout', text: 'rsid did not answer in time.', next: 'It may be busy; try again in a moment.', ...base };
  }
  if (/^(daemon response too large|malformed daemon response)\b/.test(s)) {
    return { kind: 'unreadable', text: 'rsid sent a reply the app could not read.', next: 'Make sure the app and rsid are the same version.', ...base };
  }
  const parts = parseDaemonError(s);
  if (!parts) {
    if (isScriptFault(s)) return { ...INTERNAL, ...base };
    return { kind: 'local', text: redactCredentials(s), next: '', ...base };
  }
  const c = classify(parts);
  return {
    kind: c.kind,
    text: redactCredentials(c.text),
    // The way out of a stop is ours to name: the daemon's own next step does not know about the banner.
    next: parts.nextAction && c.next !== MODEL_WORK_STOPPED_NEXT ? redactCredentials(sentence(parts.nextAction)) : c.next,
    detail,
    code: parts.code,
    rpcCode: parts.rpcCode,
  };
}

export interface PresentedError {
  /** What the surface shows: the caller's own words, then the plain sentence and next step. */
  text: string;
  /** The raw daemon error for the Details section; null when the message held none. */
  detail: string | null;
  kind: ErrorClass;
}

/**
 * A composed display message (`Launch failed: daemon error -32000: ...`) with
 * its daemon error turned into plain words. The caller's prefix is kept. A
 * message holding no daemon error is shown as it is (credentials redacted).
 */
export function presentError(message: string): PresentedError {
  const m = message.match(MARKER);
  if (!m || m.index === undefined) {
    // The app's own code threw ("Satellite registry load failed: TypeError: Cannot read ..."): plain words, the raw text under Details.
    const f = message.match(JS_FAULT_PREFIX);
    const at = f?.index ?? (isScriptFault(message) ? 0 : -1);
    if (at >= 0) {
      const p = plainError(message.slice(at));
      return { text: `${redactCredentials(message.slice(0, at))}${errorSentence(p)}`, detail: p.detail, kind: p.kind };
    }
    return { text: redactCredentials(message), detail: null, kind: 'local' };
  }
  const prefix = message.slice(0, m.index);
  const p = plainError(message.slice(m.index));
  return { text: `${redactCredentials(prefix)}${errorSentence(p)}`, detail: p.detail, kind: p.kind };
}
