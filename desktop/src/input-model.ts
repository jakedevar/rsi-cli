// Gap sweep G1 (hub #3000), pure side: the TUI's global key intercepts
// (crates/rsi/src/key_tables.rs GLOBAL_KEY_INTERCEPTS and the session-list
// arrows), the session jumplist, overlay geometry, sidebar width and the
// transcript column, recent files (gf1..gf9), the daemon-config gate, slash
// command suggestions, the correction preview and the operator-message
// commands (:pending, :drain). No DOM; unit-tested under tests/input-model.test.mjs.
// The DOM side is actions/input.ts.

// ---------- global key routing ----------

/** Where the message box (the TUI's input bar) is. */
export type ComposerState =
  /** Not focused. */
  | 'none'
  /** Vim insert mode: every key types. */
  | 'insert'
  /** Vim normal mode. */
  | 'normal'
  /** Vim keys off (the TUI's Standard editing): always typing, but Ctrl-H/L/O/I still move. */
  | 'standard';

export interface GlobalEnv {
  /** A dialog (the TUI's overlay) is open. */
  overlay: boolean;
  /** A text field other than the message box has the keyboard. */
  typing: boolean;
  composer: ComposerState;
  /** Where the keys are: the session list or the session detail. */
  focus: 'list' | 'detail';
  sessionOpen: boolean;
  /** The physical Ctrl key is down (not Cmd): overlay geometry needs it. */
  physicalCtrl: boolean;
}

export type GeometryOp = 'up' | 'down' | 'left' | 'right' | 'shorter' | 'taller' | 'narrower' | 'wider' | 'reset';

export type GlobalAction =
  | { kind: 'jump'; dir: -1 | 1 }
  | { kind: 'zone'; dir: -1 | 1 }
  | { kind: 'pane'; dir: -1 | 1 }
  | { kind: 'geometry'; op: GeometryOp }
  | { kind: 'sidebar'; dir: -1 | 1 }
  | { kind: 'column'; dir: -1 | 1 }
  | { kind: 'step'; dir: -1 | 1; open: boolean };

const GEOMETRY_KEYS: Readonly<Record<string, GeometryOp>> = {
  'C-Up': 'up', 'C-Down': 'down', 'C-Left': 'left', 'C-Right': 'right',
  'C-S-Up': 'shorter', 'C-S-Down': 'taller', 'C-S-Left': 'narrower', 'C-S-Right': 'wider',
  'C-0': 'reset',
};

const STEP_KEYS: Readonly<Record<string, { dir: -1 | 1; open: boolean }>> = {
  Left: { dir: -1, open: false },
  Right: { dir: 1, open: false },
  'S-Left': { dir: -1, open: true },
  'S-Right': { dir: 1, open: true },
  // keybindings.md: Ctrl-Tab is the previous session, Ctrl-Shift-Tab the next.
  'C-Tab': { dir: -1, open: false },
  'C-S-Tab': { dir: 1, open: false },
};

/**
 * The global intercept a key token runs, or null. The order is the TUI's:
 * overlay geometry, then (with no overlay) the jumplist, Ctrl-H/L, the
 * sidebar, Ctrl-Left/Right, then the session-list arrows.
 */
export function routeGlobal(token: string, env: GlobalEnv): GlobalAction | null {
  if (env.overlay) {
    const op = GEOMETRY_KEYS[token];
    return op && env.physicalCtrl ? { kind: 'geometry', op } : null;
  }
  if (env.typing) return null;
  const moving = env.composer !== 'insert';
  const free = env.composer === 'none' || env.composer === 'normal';
  switch (token) {
    case 'C-o': return moving ? { kind: 'jump', dir: -1 } : null;
    case 'C-i': return moving ? { kind: 'jump', dir: 1 } : null;
    case 'C-h': case 'C-l': {
      if (!moving) return null;
      const dir = token === 'C-h' ? -1 : 1;
      if (env.focus === 'list' && env.composer === 'none') return { kind: 'zone', dir };
      return { kind: 'pane', dir };
    }
    case 'C-S-Left': case 'C-S-Right':
      return free ? { kind: 'sidebar', dir: token === 'C-S-Left' ? -1 : 1 } : null;
    case 'C-Left': case 'C-Right': {
      if (!free) return null;
      const dir = token === 'C-Left' ? -1 : 1;
      if (env.focus === 'detail' && env.sessionOpen) return { kind: 'column', dir };
      return { kind: 'pane', dir };
    }
    default: {
      const s = STEP_KEYS[token];
      return s && env.composer === 'none' ? { kind: 'step', ...s } : null;
    }
  }
}

/** Where pane focus goes: list ← detail; null at an edge or with no session open. */
export function paneStep(focus: 'list' | 'detail', dir: -1 | 1, sessionOpen: boolean): 'list' | 'detail' | null {
  if (dir < 0) return focus === 'detail' ? 'list' : null;
  return focus === 'list' && sessionOpen ? 'detail' : null;
}

/** Previous / next row, clamped; with no cursor the first or last row. */
export function stepRow(ids: readonly string[], cur: string | null, dir: -1 | 1): string | null {
  if (ids.length === 0) return null;
  const i = cur === null ? -1 : ids.indexOf(cur);
  if (i < 0) return (dir > 0 ? ids[0] : ids[ids.length - 1]) ?? null;
  return ids[Math.max(0, Math.min(ids.length - 1, i + dir))] ?? null;
}

// ---------- jumplist ----------

export const JUMPLIST_MAX = 100;

/** The session jumplist (Ctrl-O back, Ctrl-I forward), vim style. */
export class Jumplist {
  private entries: string[] = [];
  private at = -1;

  /** A session was opened: it becomes the newest entry and forward history drops. */
  visit(id: string): void {
    if (this.entries[this.at] === id) return;
    this.entries = this.entries.slice(0, this.at + 1).filter((e) => e !== id);
    this.entries.push(id);
    if (this.entries.length > JUMPLIST_MAX) this.entries.shift();
    this.at = this.entries.length - 1;
  }

  /** The session one step back (dir -1) or forward, skipping ids `exists` refuses; null at the end. */
  jump(dir: -1 | 1, exists: (id: string) => boolean): string | null {
    for (let i = this.at + dir; i >= 0 && i < this.entries.length; i += dir) {
      if (exists(this.entries[i]!)) {
        this.at = i;
        return this.entries[i]!;
      }
    }
    return null;
  }

  /** Whether jump(dir) would move, without moving. */
  canJump(dir: -1 | 1, exists: (id: string) => boolean): boolean {
    for (let i = this.at + dir; i >= 0 && i < this.entries.length; i += dir) if (exists(this.entries[i]!)) return true;
    return false;
  }

  list(): { entries: readonly string[]; at: number } {
    return { entries: [...this.entries], at: this.at };
  }
}

// ---------- overlay geometry ----------

export interface Size {
  width: number;
  height: number;
}

/** A dialog's saved geometry: offset from its centred place and an optional size. */
export interface Geometry {
  dx: number;
  dy: number;
  width: number | null;
  height: number | null;
}

export const GEOMETRY_STEP = 24;
export const MIN_OVERLAY: Size = { width: 240, height: 120 };
export const NO_GEOMETRY: Geometry = { dx: 0, dy: 0, width: null, height: null };

/** One Ctrl(-Shift)-arrow or Ctrl-0 step, kept inside the window. */
export function geometryStep(g: Geometry, op: GeometryOp, current: Size, viewport: Size): Geometry {
  if (op === 'reset') return NO_GEOMETRY;
  const s = GEOMETRY_STEP;
  const clampW = (w: number) => Math.max(MIN_OVERLAY.width, Math.min(viewport.width, w));
  const clampH = (h: number) => Math.max(MIN_OVERLAY.height, Math.min(viewport.height, h));
  const out = { ...g };
  switch (op) {
    case 'up': out.dy -= s; break;
    case 'down': out.dy += s; break;
    case 'left': out.dx -= s; break;
    case 'right': out.dx += s; break;
    case 'shorter': out.height = clampH(current.height - s); break;
    case 'taller': out.height = clampH(current.height + s); break;
    case 'narrower': out.width = clampW(current.width - s); break;
    case 'wider': out.width = clampW(current.width + s); break;
  }
  const w = out.width ?? current.width;
  const h = out.height ?? current.height;
  const maxX = Math.max(0, (viewport.width - w) / 2);
  const maxY = Math.max(0, (viewport.height - h) / 2);
  out.dx = Math.max(-maxX, Math.min(maxX, out.dx));
  out.dy = Math.max(-maxY, Math.min(maxY, out.dy));
  return out;
}

/** A stored geometry back from JSON, or none. */
export function parseGeometry(raw: string | null): Geometry {
  if (!raw) return NO_GEOMETRY;
  try {
    const v = JSON.parse(raw) as Partial<Geometry>;
    const num = (x: unknown) => (typeof x === 'number' && Number.isFinite(x) ? x : null);
    return { dx: num(v.dx) ?? 0, dy: num(v.dy) ?? 0, width: num(v.width), height: num(v.height) };
  } catch {
    return NO_GEOMETRY;
  }
}

// ---------- sidebar and transcript column ----------

export const SIDEBAR_DEFAULT = 340;
export const SIDEBAR_MIN = 200;
export const SIDEBAR_STEP = 24;

/** The session-list width after one Ctrl-Shift-Left/Right, within the window. */
export function sidebarStep(width: number, dir: -1 | 1, windowWidth: number): number {
  const max = Math.max(SIDEBAR_MIN, Math.floor(windowWidth * 0.7));
  return Math.max(SIDEBAR_MIN, Math.min(max, width + dir * SIDEBAR_STEP));
}

export const COLUMN_STEP = 40;
export const COLUMN_MIN_WIDTH = 320;

/**
 * The transcript column's left offset after Ctrl-Left/Right. 0 is "Left
 * Aligned": Ctrl-Left snaps to it at the edge, Ctrl-Right leaves it.
 */
export function columnStep(offset: number, dir: -1 | 1, paneWidth: number): number {
  const max = Math.max(0, paneWidth - COLUMN_MIN_WIDTH);
  return Math.max(0, Math.min(max, offset + dir * COLUMN_STEP));
}

// ---------- recent files (gf1..gf9) ----------

const FILE_TOOLS = new Set(['Edit', 'MultiEdit', 'Write', 'Read']);
export const RECENT_FILES = 9;

export interface ToolEventLike {
  tool_name?: string | null;
  tool_input?: unknown;
}

/** The newest-first distinct `file_path`s of Edit / MultiEdit / Write / Read tool events, at most 9. */
export function recentFiles(events: readonly ToolEventLike[]): string[] {
  const out: string[] = [];
  for (let i = events.length - 1; i >= 0 && out.length < RECENT_FILES; i--) {
    const e = events[i]!;
    if (!e.tool_name || !FILE_TOOLS.has(e.tool_name)) continue;
    const input = e.tool_input;
    const path = input && typeof input === 'object' ? (input as { file_path?: unknown }).file_path : undefined;
    if (typeof path === 'string' && path.trim() !== '' && !out.includes(path)) out.push(path);
  }
  return out;
}

/** A path inside a root, as the root-relative `/` path the file viewer reads; null when outside. */
export function relativeTo(root: string, path: string): string | null {
  const norm = (p: string) => p.replace(/\\/g, '/').replace(/\/+$/, '');
  const r = norm(root);
  const p = norm(path);
  if (r === '' || !p.startsWith(`${r}/`)) return null;
  const rel = p.slice(r.length + 1);
  return rel.split('/').some((s) => s === '..' || s === '') ? null : rel;
}

// ---------- daemon config gate ----------

export type ConfigState = { kind: 'loading' } | { kind: 'ready' } | { kind: 'error'; error: string };

/** Null when submissions may go, else the TUI's notice (bootstrap.rs `daemon_config_unavailable_reason`). */
export function configGate(s: ConfigState): string | null {
  switch (s.kind) {
    case 'ready': return null;
    case 'loading': return 'Daemon configuration is still loading; draft preserved';
    case 'error': return `Daemon configuration unavailable: ${s.error}; draft preserved`;
  }
}

// ---------- the message box ----------

export interface EnterLike {
  key: string;
  shiftKey?: boolean;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  isComposing?: boolean;
}

/**
 * Enter in a submit-capable box: Ctrl/Cmd-Enter always sends; with Submit on
 * Enter (the TUI default) plain Enter sends and Shift-Enter types a newline.
 */
export function submitKey(ev: EnterLike, submitOnEnter: boolean): boolean {
  if (ev.key !== 'Enter' || ev.isComposing || ev.altKey) return false;
  if (ev.ctrlKey || ev.metaKey) return true;
  return submitOnEnter && !ev.shiftKey;
}

/** The file extension a pasted image is saved with, or null for a type the TUI does not paste. */
export function imageExtension(mime: string): string | null {
  switch (mime.toLowerCase()) {
    case 'image/png': return 'png';
    case 'image/jpeg': case 'image/jpg': return 'jpg';
    case 'image/gif': return 'gif';
    case 'image/webp': return 'webp';
    default: return null;
  }
}

/** The MIME type rsid's `UploadPasteImage` accepts for a clipboard type (hub #3044), or null. */
export function pasteImageMime(mime: string): string | null {
  switch (imageExtension(mime)) {
    case 'png': return 'image/png';
    case 'jpg': return 'image/jpeg';
    case 'gif': return 'image/gif';
    case 'webp': return 'image/webp';
    default: return null;
  }
}

/** The base64 payload of a `data:<type>;base64,<payload>` URL (FileReader.readAsDataURL), or null. */
export function dataUrlBase64(url: string): string | null {
  const m = /^data:[^,]*;base64,(.*)$/su.exec(url);
  const payload = m?.[1];
  return payload ? payload : null;
}

/** Text to insert for a saved image (the TUI inserts an `@path` reference). */
export const imageReference = (path: string): string => `@${path} `;

// ---------- slash command suggestions ----------

export interface SlashCommand {
  name: string;
  description: string;
}

/** The root rsid lists slash commands for (`ListSlashCommands`, hub #3044). */
export type SlashRoot = { project_id: string } | { session_id: string };

/**
 * In the launch dialog, the chosen project when no working dir is typed, or
 * the project whose path is the typed one (rsid lists only a project or
 * session root, never an arbitrary path); elsewhere the selected session.
 */
export function slashRoot(
  launch: { workingDir: string; projectId: string } | null,
  projects: readonly { id: string; path?: string | null | undefined }[],
  sessionId: string | null,
): SlashRoot | null {
  if (launch) {
    const wd = launch.workingDir.trim();
    const project = wd
      ? projects.find((p) => (p.path ?? '').replace(/\/+$/u, '') === wd.replace(/\/+$/u, ''))
      : projects.find((p) => p.id === launch.projectId);
    return project ? { project_id: project.id } : null;
  }
  return sessionId ? { session_id: sessionId } : null;
}

/** A cache key for a slash root. */
export const slashRootKey = (root: SlashRoot): string =>
  'project_id' in root ? `p:${root.project_id}` : `s:${root.session_id}`;

/** The text after `/` up to the cursor when the cursor's line starts with `/` (suggestions.rs). */
export function slashQuery(text: string, cursor: number): string | null {
  const start = text.lastIndexOf('\n', cursor - 1) + 1;
  if (text[start] !== '/') return null;
  const upto = text.slice(start + 1, cursor);
  if (cursor <= start || /\s/u.test(upto)) return null;
  return upto;
}

/** A skim-like fuzzy score (higher is better), null when the letters are not all there in order. */
export function fuzzyScore(name: string, query: string): number | null {
  if (query === '') return 0;
  const n = name.toLowerCase();
  const q = query.toLowerCase();
  let score = 0;
  let from = 0;
  let prev = -2;
  for (const c of q) {
    const i = n.indexOf(c, from);
    if (i < 0) return null;
    score += 10;
    if (i === prev + 1) score += 15;
    if (i === 0 || /[-_ .]/.test(n[i - 1]!)) score += 10;
    score -= Math.min(i - from, 5);
    prev = i;
    from = i + 1;
  }
  return score;
}

/** Commands matching the query, best first; an empty query lists them all in order. */
export function filterSlash(commands: readonly SlashCommand[], query: string): SlashCommand[] {
  if (query === '') return [...commands];
  return commands
    .map((c, i) => ({ c, i, s: fuzzyScore(c.name, query) }))
    .filter((x) => x.s !== null)
    .sort((a, b) => b.s! - a.s! || a.i - b.i)
    .map((x) => x.c);
}

/** Tab: replace the line's `/query` (to the cursor) with `/name `. */
export function acceptSlash(text: string, cursor: number, name: string): { text: string; cursor: number } {
  const start = text.lastIndexOf('\n', cursor - 1) + 1;
  const insert = `/${name} `;
  return { text: text.slice(0, start) + insert + text.slice(cursor), cursor: start + insert.length };
}

/** Move the suggestion highlight, wrapping. */
export function moveSuggestion(count: number, at: number, dir: -1 | 1): number {
  if (count <= 0) return -1;
  return (((at + dir) % count) + count) % count;
}

// ---------- correction preview (Ctrl-Y / Ctrl-Shift-G) ----------

export interface Correction {
  text: string;
  /** The compiler asked a question: there is nothing to accept. */
  clarify: boolean;
  kind: 'compile' | 'grammar';
}

export type CompileOutcome = { preview: Correction | null; notice: string | null };

/** A CompilePrompt result (rsi_common::prompt_compile::CompileResult) to a preview (event.rs). */
export function compileOutcome(result: unknown): CompileOutcome {
  const r = (result ?? {}) as { compiled?: unknown; contract?: { status?: unknown; kind?: unknown; message?: unknown }; layer_validation?: Record<string, unknown> };
  const lv = r.layer_validation ?? {};
  const missing = ['semantic', 'syntactic', 'deictic', 'discourse', 'pragmatic'].filter((k) => lv[k] === false);
  const weak = missing.length > 0 ? `Compiler: weak layers [${missing.join(', ')}]` : null;
  const status = r.contract?.status;
  if (status === 'error') {
    return { preview: null, notice: `Compile ERROR [${String(r.contract?.kind ?? '?')}]: ${String(r.contract?.message ?? '')}` };
  }
  if ((status === 'complete' || status === 'incomplete') && typeof r.compiled === 'string') {
    return { preview: { text: r.compiled, clarify: false, kind: 'compile' }, notice: weak };
  }
  return { preview: null, notice: 'Compile result: unknown contract variant' };
}

/** A failed compile: an ambiguous intent becomes a CLARIFY preview, anything else a notice. */
export function compileFailure(message: string): CompileOutcome {
  const m = message.match(/Ambiguous intent: (.*)$/s);
  if (m) return { preview: { text: `CLARIFY:\n- ${m[1]}`, clarify: true, kind: 'compile' }, notice: null };
  return { preview: null, notice: `Prompt compilation failed: ${message}` };
}

export type PreviewKey = 'accept' | 'discard' | 'send-original' | 'compile' | 'grammar';

/** The correction preview's keys (keybindings.md "Correction Preview"). */
export function previewKey(token: string, preview: Correction): PreviewKey | null {
  switch (token) {
    case 'a': return preview.clarify ? null : 'accept';
    case 'd': case 'Esc': return 'discard';
    case 'C-Enter': return 'send-original';
    case 'C-y': return 'compile';
    case 'C-S-g': return 'grammar';
    default: return null;
  }
}

// ---------- operator messages and drain ----------

/** OperatorMessageView states still in flight (crates/rsi/src/client.rs `is_pending`). */
const PENDING_STATES = new Set(['queued', 'dispatching', 'effect_possible', 'uncertain']);

export interface OperatorMessage {
  id: string;
  session_id: string;
  content: string;
  state: string;
  created_at?: string;
}

export const isPending = (m: Pick<OperatorMessage, 'state'>): boolean => PENDING_STATES.has(m.state);

export function pendingMessages(list: unknown): OperatorMessage[] {
  if (!Array.isArray(list)) return [];
  return (list as OperatorMessage[]).filter((m) => m && typeof m.id === 'string' && typeof m.state === 'string' && isPending(m));
}

/** `:pending` previews: the first 100 characters on one line. */
export const messagePreview = (content: string): string => [...content.replace(/\s+/g, ' ')].slice(0, 100).join('');

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export type PendingCommand =
  | { kind: 'list' }
  | { kind: 'edit'; id: string; text: string }
  | { kind: 'withdraw'; id: string }
  | { kind: 'drain' }
  | { kind: 'drain-status' }
  | { kind: 'error'; message: string };

/** `:pending …` / `:drain …` arguments (commands.rs parses them ahead of its table). */
export function parseOperatorCommand(name: 'pending' | 'drain', arg: string): PendingCommand {
  const t = arg.trim();
  if (name === 'drain') {
    if (t === '') return { kind: 'drain' };
    if (t === 'status') return { kind: 'drain-status' };
    return { kind: 'error', message: 'Use :drain or :drain status' };
  }
  if (t === '') return { kind: 'list' };
  const edit = t.match(/^edit\s+(\S+)\s+([\s\S]+)$/);
  if (edit) {
    if (!UUID.test(edit[1]!)) return { kind: 'error', message: 'Invalid pending message ID' };
    return { kind: 'edit', id: edit[1]!.toLowerCase(), text: edit[2]! };
  }
  if (/^edit\b/.test(t)) return { kind: 'error', message: 'Use :pending edit <message-id> <new text>' };
  const wd = t.match(/^withdraw\s+(\S+)$/);
  if (wd) {
    if (!UUID.test(wd[1]!)) return { kind: 'error', message: 'Invalid pending message ID' };
    return { kind: 'withdraw', id: wd[1]!.toLowerCase() };
  }
  return { kind: 'error', message: 'Use :pending, :pending edit <id> <text> or :pending withdraw <id>' };
}

/** A typed `/pending …` or `/drain …` message runs the command instead of being sent (session_actions.rs). */
export function operatorSlash(text: string): PendingCommand | null {
  const t = text.trim();
  if (t === '/drain' || t === '/drain status') return parseOperatorCommand('drain', t.slice(6));
  if (t === '/pending' || t.startsWith('/pending ')) return parseOperatorCommand('pending', t.slice(8));
  return null;
}

/** Statuses whose follow-ups queue for the next safe turn boundary instead of starting a turn. */
const BUSY = new Set(['Starting', 'Running', 'WaitingApproval']);
export const queuesFollowUp = (status: string): boolean => BUSY.has(status);

// ---------- modal toggles ----------

/** Keys that close the dialog they opened when pressed again (keybindings.md "Modal Toggle Behavior"). */
export const TOGGLE_SEQUENCES: readonly (readonly string[])[] = [['C-m'], ['T'], ['Space', 'p'], ['Space', 's']];

/** The toggle sequence the latest keys end with, or null. */
export function toggleSequence(recent: readonly string[]): readonly string[] | null {
  for (const seq of TOGGLE_SEQUENCES) {
    if (recent.length >= seq.length && seq.every((k, i) => recent[recent.length - seq.length + i] === k)) return seq;
  }
  return null;
}
