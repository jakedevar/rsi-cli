// Pane, zone and window navigation (hub #3000 gap sweep G2), pure rules with
// no DOM: the Jobs zone, Backspace, close pane, the project "tabs", the
// default launch model (Ctrl-M), and the window and scroll keys the TUI
// inherits from modalkit. Each names the TUI code it mirrors. Unit-tested
// under tests/nav.test.mjs.

// ---------- zones (gs / gj / ga) ----------

export type Zone = 'main' | 'jobs';
export type ListZone = Zone | 'archived';

/** A root-level scheduled-job launch (app/sessions.rs is_jobs_zone_session). */
export function isJobSession(s: { scheduled_job_id?: string | null; parent_id?: string | null }): boolean {
  return !!s.scheduled_job_id && !s.parent_id;
}

/** Ctrl-h / Ctrl-l on the list: the previous / next session-list zone. */
export function stepZone(cur: ListZone, dir: 1 | -1): ListZone {
  const order: readonly ListZone[] = ['main', 'jobs', 'archived'];
  const i = order.indexOf(cur);
  return order[(i + dir + order.length) % order.length]!;
}

export function zoneLabel(z: ListZone): string {
  return z === 'main' ? 'Active sessions' : z === 'jobs' ? 'Jobs: scheduled-job sessions' : 'Archived sessions';
}

// ---------- project "tabs" (> / <) ----------

/** The next / previous project filter, "All projects" ('') included, wrapping. */
export function stepProject(ids: readonly string[], cur: string, dir: 1 | -1): string {
  const ring = ['', ...ids];
  const i = Math.max(0, ring.indexOf(cur));
  return ring[(i + dir + ring.length) % ring.length]!;
}

// ---------- Backspace (AscendOrBack) ----------

export type BackStep = 'to-list' | 'ascend' | null;

/** The transcript hands the keys back to the list; in the list it ascends one container; at the root it does nothing. */
export function backStep(focus: 'list' | 'transcript', cursorHasParent: boolean, sessionOpen: boolean): BackStep {
  if (focus === 'transcript' && sessionOpen) return 'to-list';
  return cursorHasParent ? 'ascend' : null;
}

// ---------- close pane (<Space>q, :close) ----------

/** What the snapshot says is on screen (the fields close-pane reads). */
export interface PaneView {
  open: unknown;
  terminal?: { visible: boolean } | undefined;
  files?: { viewer: boolean; explorer: boolean } | undefined;
  graphs?: { pane: 'graph' | 'dag' | 'esp' | null } | undefined;
  managers?: { view: unknown } | undefined;
  issues?: { active: boolean } | undefined;
}

export type CloseTarget =
  | 'toggle-terminal' | 'files-close-viewer' | 'files-explorer' | 'graph-close' | 'dag-close' | 'mgr-close'
  | 'issues-close' | 'session';

/** The focused pane `<Space>q` closes: the topmost overlay first, the open session last. */
export function closeTarget(v: PaneView): CloseTarget | null {
  if (v.terminal?.visible) return 'toggle-terminal';
  if (v.files?.viewer) return 'files-close-viewer';
  if (v.files?.explorer) return 'files-explorer';
  if (v.graphs?.pane === 'graph') return 'graph-close';
  if (v.graphs?.pane === 'dag') return 'dag-close';
  if (v.managers?.view) return 'mgr-close';
  if (v.issues?.active) return 'issues-close';
  if (v.open) return 'session';
  return null;
}

// ---------- mouse: file paths and web links ----------

/** Inline code that names a file (`src/a.ts`, `/abs/b.rs`, `Cargo.toml`): no spaces, a slash or an extension. */
export function looksLikePath(text: string): boolean {
  const t = text.trim();
  if (t === '' || t.length > 512 || /\s/.test(t) || /^[a-z][a-z0-9+.-]*:\/\//i.test(t)) return false;
  if (/[<>"|*?`]/.test(t)) return false;
  return /[\\/]/.test(t) ? /[A-Za-z0-9_]/.test(t) : /^[\w.-]+\.[A-Za-z0-9]{1,8}$/.test(t);
}

export function isWebUrl(text: string): boolean {
  return /^https?:\/\/[^\s]+$/i.test(text.trim());
}

// ---------- window, pane and scroll keys ----------

export type NavKey =
  | { kind: 'scroll'; amount: 'half' | 'page'; dir: 1 | -1 }
  | { kind: 'focus'; to: 'list' | 'transcript' | 'composer' }
  | { kind: 'arm-window' }
  | { kind: 'pass' };

export interface NavKeyEvent {
  key: string;
  ctrlKey?: boolean;
  shiftKey?: boolean;
  altKey?: boolean;
  metaKey?: boolean;
}

/**
 * The keys modalkit gives the TUI's panes, read where no text field, dialog
 * or overlay has the keyboard: Ctrl-d / u / f / b scroll half or a full
 * page, and Ctrl-w then h / j / k / l moves the focus. Left / Right,
 * Ctrl-Tab, Ctrl-h / Ctrl-l (zones, through stepZone), Ctrl-Left / Right and
 * Ctrl-Shift-Left / Right are the Global keys gap sweep G1 routes
 * (input-model.ts routeGlobal), so they pass here.
 */
export function routeNavKey(ev: NavKeyEvent, windowArmed: boolean): NavKey {
  if (ev.altKey || ev.metaKey) return { kind: 'pass' };
  const ctrl = !!ev.ctrlKey;
  const shift = !!ev.shiftKey;
  if (windowArmed && !ctrl && !shift) {
    switch (ev.key) {
      case 'h': return { kind: 'focus', to: 'list' };
      case 'l': return { kind: 'focus', to: 'transcript' };
      case 'j': return { kind: 'focus', to: 'composer' };
      case 'k': return { kind: 'focus', to: 'transcript' };
      default: return { kind: 'pass' };
    }
  }
  if (!ctrl || shift) return { kind: 'pass' };
  switch (ev.key) {
    case 'd': return { kind: 'scroll', amount: 'half', dir: 1 };
    case 'u': return { kind: 'scroll', amount: 'half', dir: -1 };
    case 'f': return { kind: 'scroll', amount: 'page', dir: 1 };
    case 'b': return { kind: 'scroll', amount: 'page', dir: -1 };
    case 'w': return { kind: 'arm-window' };
    default: return { kind: 'pass' };
  }
}

/** Pixels one scroll key moves a pane of `height` pixels. */
export function scrollBy(amount: 'half' | 'page', height: number): number {
  return Math.max(1, Math.floor(amount === 'half' ? height / 2 : height));
}

/** How long Ctrl-w waits for its h / j / k / l. */
export const WINDOW_CHORD_MS = 1500;

// ---------- the default launch model (Ctrl-M, :model) ----------

export interface LaunchDefault {
  provider: string;
  model: string;
}

export function parseLaunchDefault(raw: string | null | undefined): LaunchDefault | null {
  if (!raw) return null;
  try {
    const v = JSON.parse(raw) as Partial<LaunchDefault>;
    if (typeof v.provider === 'string' && typeof v.model === 'string' && v.provider !== '') return { provider: v.provider, model: v.model };
  } catch {
    // a corrupt value is no default
  }
  return null;
}

/** DiscoverModels answers `[[id, label], …]` or `{models: [...]}`; the ids, de-duplicated. */
export function modelIds(answer: unknown): Array<{ id: string; label: string }> {
  const list = Array.isArray(answer) ? answer
    : answer && typeof answer === 'object' && Array.isArray((answer as { models?: unknown }).models) ? (answer as { models: unknown[] }).models : [];
  const out: Array<{ id: string; label: string }> = [];
  for (const m of list) {
    const id = Array.isArray(m) ? m[0] : typeof m === 'string' ? m : null;
    const label = Array.isArray(m) && typeof m[1] === 'string' ? m[1] : id;
    if (typeof id === 'string' && id !== '' && !out.some((x) => x.id === id)) out.push({ id, label: String(label) });
  }
  return out;
}

/** The provider after `cur` in the launch form's list (the dropdown's provider cycle). */
export function nextProvider(providers: readonly string[], cur: string): string {
  if (providers.length === 0) return cur;
  const i = providers.indexOf(cur);
  return providers[(i + 1) % providers.length]!;
}

/** `:model <id>` sets the default model directly; a bare `:model` opens the dropdown. */
export function modelArg(arg: string | undefined): string | null {
  const t = (arg ?? '').trim();
  return t === '' ? null : t;
}

/** A transcript path made absolute against the session's directory (the TUI copies the resolved path). */
export function resolvePath(root: string | null | undefined, path: string): string {
  const p = path.trim();
  if (/^([\\/]|[A-Za-z]:[\\/]|~)/.test(p) || !root) return p;
  return `${root.replace(/[\\/]+$/, '')}/${p.replace(/^\.\//, '')}`;
}

// ---------- lazygit / btop (<Space>gg, <Space>bb) ----------

/**
 * The line typed into the embedded terminal. A POSIX shell checks the tool
 * is installed first (the TUI says so instead of failing); Windows' shell
 * runs it as it is.
 */
export function terminalCommand(tool: 'lazygit' | 'btop', windows: boolean): string {
  if (windows) return tool;
  return `command -v ${tool} >/dev/null 2>&1 && ${tool} || echo "${tool} is not installed"`;
}
