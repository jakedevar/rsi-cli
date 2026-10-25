// The terminal overlay, DOM side (slice 12, hub #3014): an xterm.js pane (with
// the fit addon) over a shell whose PTY the Tauri backend owns
// (src-tauri/src/pty.rs). The rules live in terminal-model.ts (pure, tested)
// and the registry entries (SLICE) in terminal-spec.ts; commands.ts reaches
// this module through RUNTIME (collected by actions/runtime.generated.ts).
//
// xterm.js is vendored into dist/vendor/ by scripts/copy-static.mjs (from the
// lockfile-pinned npm packages, no CDN) and loaded the first time the overlay
// opens. While the terminal has focus its helper textarea is a text field, so
// the vim keys, the transcript keys and the palette's Ctrl-K (Linux, Windows)
// stand aside and every key goes to the shell, as in the TUI.
import type { FitAddon } from '@xterm/addon-fit';
import type { ITheme, Terminal } from '@xterm/xterm';
import { invoke } from '../api.js';
import { type Invocation, type SliceRuntime, dispatch } from '../commands.js';
import { el, toast } from '../dom.js';
import { selected, uiChanged } from '../state.js';
import {
  DEFAULT_SIZE, EMPTY_TERMS, type GridSize, type Scroll, type TermMode, type TermState,
  chunkInput, clampGrid, exited, gridFor, isRunning, isToggleChord, keyForPty, modeLabel, overlayRect, overlayTitle,
  remove, resizeStep, routeTerminalKey, runningPtys, spawnFailed, spawned, terminalKey, toggle,
} from '../terminal-model.js';
import { currentPlatform, isVimKeysOn } from '../vimkeys.js';
import { SLICE, type TerminalActionId, type TerminalSnapshot } from './terminal-spec.js';
import { focusOwnsKey } from '../modal-gate.js';

type XtermModule = typeof import('@xterm/xterm');
type FitModule = typeof import('@xterm/addon-fit');

interface View {
  term: Terminal;
  fit: FitAddon;
  host: HTMLElement;
  size: GridSize | null;
}

interface TauriEvents {
  event?: { listen(name: string, cb: (e: { payload: unknown }) => void): Promise<() => void> };
}

const FONT_SIZE = 13;
/** Output for a PTY id the spawn reply has not named yet (the shell can speak first). */
const EARLY_LIMIT = 256 * 1024;

/** Catppuccin Mocha, matching styles.css and the TUI. */
const THEME: ITheme = {
  background: '#1e1e2e', foreground: '#cdd6f4', cursor: '#f5e0dc', cursorAccent: '#1e1e2e',
  selectionBackground: '#585b70',
  black: '#45475a', red: '#f38ba8', green: '#a6e3a1', yellow: '#f9e2af',
  blue: '#89b4fa', magenta: '#f5c2e7', cyan: '#94e2d5', white: '#bac2de',
  brightBlack: '#585b70', brightRed: '#f38ba8', brightGreen: '#a6e3a1', brightYellow: '#f9e2af',
  brightBlue: '#89b4fa', brightMagenta: '#f5c2e7', brightCyan: '#94e2d5', brightWhite: '#a6adc8',
};

let st: TermState = EMPTY_TERMS;
const views = new Map<string, View>();
const early = new Map<string, { out: string[]; bytes: number; exit?: number | null }>();
let lib: Promise<{ Terminal: XtermModule['Terminal']; FitAddon: FitModule['FitAddon'] }> | undefined;
let overlay: HTMLElement | undefined;
let returnFocus: HTMLElement | null = null;

// ---------- registry surface ----------

export function terminalSnapshot(): TerminalSnapshot {
  const key = st.visible ?? terminalKey(selected()?.id);
  return { visible: st.visible !== null, running: isRunning(st.entries[key]) };
}

export function runTerminal(id: TerminalActionId, _inv: Invocation): void {
  switch (id) {
    case 'toggle-terminal': void toggleTerminal(); return;
    case 'kill-terminal': void killTerminal(); return;
  }
}

// ---------- xterm.js ----------

function loadXterm(): NonNullable<typeof lib> {
  lib ??= Promise.all([
    import(new URL('../vendor/xterm.mjs', import.meta.url).href) as Promise<XtermModule>,
    import(new URL('../vendor/addon-fit.mjs', import.meta.url).href) as Promise<FitModule>,
  ]).then(([x, f]) => ({ Terminal: x.Terminal, FitAddon: f.FitAddon }));
  lib.catch(() => { lib = undefined; });
  return lib;
}

function overlayEl(): HTMLElement {
  if (overlay) return overlay;
  const o = el('section', 'terminal-overlay');
  o.id = 'terminal-overlay';
  o.hidden = true;
  o.setAttribute('aria-label', 'Terminal');
  const head = el('header', 'terminal-head');
  const title = el('span', 'terminal-title', 'Terminal');
  const mode = el('span', 'terminal-mode', modeLabel('insert'));
  const hide = el('button', 'btn', 'Hide');
  hide.type = 'button';
  hide.dataset.action = 'toggle-terminal';
  hide.title = 'Hide the terminal; the shell keeps running (Ctrl-\\)';
  head.append(title, mode, hide);
  const body = el('div', 'terminal-body');
  o.append(head, body);
  document.body.append(o);
  new ResizeObserver(() => fitVisible()).observe(body);
  overlay = o;
  return o;
}

function part(cls: string): HTMLElement {
  return overlayEl().querySelector<HTMLElement>(`.${cls}`)!;
}

async function viewFor(key: string): Promise<View> {
  const have = views.get(key);
  if (have) return have;
  const { Terminal: Term, FitAddon: Fit } = await loadXterm();
  const host = el('div', 'terminal-host');
  host.hidden = true;
  part('terminal-body').append(host);
  const term = new Term({
    fontFamily: 'ui-monospace, "JetBrains Mono", "Fira Code", Menlo, monospace',
    fontSize: FONT_SIZE,
    scrollback: 5000,
    cursorBlink: true,
    macOptionIsMeta: true,
    allowProposedApi: false,
    theme: THEME,
  });
  const fit = new Fit();
  term.loadAddon(fit);
  term.open(host);
  const view: View = { term, fit, host, size: null };
  term.attachCustomKeyEventHandler((ev) => onTermKey(view, ev));
  term.onData((data) => sendInput(key, data));
  views.set(key, view);
  return view;
}

/** xterm's key hook: true lets xterm send the key to the shell. */
function onTermKey(view: View, ev: KeyboardEvent): boolean {
  const vim = isVimKeysOn();
  if (!vim && st.mode !== 'insert') setMode('insert'); // the switch went off while in normal mode
  if (ev.type !== 'keydown') return !vim || (st.mode === 'insert' && ev.key !== 'Escape');
  const r = routeTerminalKey(st.mode, ev, currentPlatform(), view.term.rows, vim);
  switch (r.kind) {
    case 'pty': return true;
    case 'native': return false;
    case 'toggle': ev.preventDefault(); dispatch('toggle-terminal', { source: 'key' }); return false;
    case 'mode': ev.preventDefault(); setMode(r.mode); return false;
    case 'scroll': ev.preventDefault(); scroll(view.term, r.scroll); return false;
    case 'swallow': ev.preventDefault(); return false;
  }
}

function scroll(term: Terminal, s: Scroll): void {
  if (s.by === 'top') term.scrollToTop();
  else if (s.by === 'bottom') term.scrollToBottom();
  else term.scrollLines(s.n);
}

function setMode(mode: TermMode): void {
  const vim = isVimKeysOn();
  if (!vim) mode = 'insert'; // no modal layer without Vim keys
  st = { ...st, mode };
  const m = part('terminal-mode');
  m.textContent = modeLabel(mode, vim);
  m.hidden = !vim;
  m.classList.toggle('normal', mode === 'normal');
  // Typing returns to the live prompt.
  if (mode === 'insert' && st.visible) views.get(st.visible)?.term.scrollToBottom();
}

function renderHead(): void {
  part('terminal-title').textContent = overlayTitle(st.visible ? st.entries[st.visible] : undefined);
  setMode(st.mode);
}

// ---------- size ----------

function measure(view: View): GridSize | null {
  const proposed = clampGrid(view.fit.proposeDimensions() ?? null);
  if (proposed) return proposed;
  const r = view.host.getBoundingClientRect();
  return gridFor({ width: r.width, height: r.height }, { width: FONT_SIZE * 0.6, height: Math.ceil(FONT_SIZE * 1.2) });
}

function fitView(key: string): void {
  const view = views.get(key);
  if (!view || view.host.hidden) return;
  const next = resizeStep(view.size, measure(view));
  if (!next) return;
  view.size = next;
  view.term.resize(next.cols, next.rows);
  const pty = st.entries[key]?.ptyId;
  if (pty && isRunning(st.entries[key])) {
    void invoke('pty_resize', { id: pty, rows: next.rows, cols: next.cols }).catch(() => undefined);
  }
}

function fitVisible(): void {
  if (st.visible) fitView(st.visible);
}

function placeOverlay(): void {
  const o = overlayEl();
  const r = overlayRect(window.innerWidth, window.innerHeight);
  o.style.left = `${r.left}px`;
  o.style.top = `${r.top}px`;
  o.style.width = `${r.width}px`;
  o.style.height = `${r.height}px`;
}

// ---------- shells ----------

function sendInput(key: string, data: string): void {
  const e = st.entries[key];
  if (!e || !isRunning(e)) return;
  for (const chunk of chunkInput(data)) {
    void invoke('pty_write', { id: e.ptyId, data: chunk }).catch((err) => toast(`Terminal: ${String(err)}`));
  }
}

async function spawnShell(key: string, cwd: string | undefined): Promise<void> {
  const view = views.get(key);
  const size = (view && (view.size ?? measure(view))) || DEFAULT_SIZE;
  try {
    const r = await invoke<{ id: string; cwd: string }>('pty_spawn', { cwd: cwd ?? null, rows: size.rows, cols: size.cols });
    if (view) view.size = size;
    st = spawned(st, key, r.id, r.cwd);
    const held = early.get(r.id);
    early.delete(r.id);
    for (const chunk of held?.out ?? []) view?.term.write(chunk);
    if (held && 'exit' in held) onExit(r.id, held.exit ?? null);
  } catch (err) {
    toast(`Could not start the terminal: ${String(err)}`);
    st = spawnFailed(st, key);
    if (st.visible === null) hideOverlay();
  }
  if (st.visible === key) renderHead();
  uiChanged();
}

function onOutput(id: string, data: string): void {
  const key = keyForPty(st, id);
  if (key) { views.get(key)?.term.write(data); return; }
  const held = early.get(id) ?? { out: [], bytes: 0 };
  if (held.bytes + data.length > EARLY_LIMIT) return;
  held.out.push(data);
  held.bytes += data.length;
  early.set(id, held);
}

function onExit(id: string, code: number | null): void {
  const r = exited(st, id, code);
  if (!r.key) {
    const held = early.get(id) ?? { out: [], bytes: 0 };
    held.exit = code;
    early.set(id, held);
    return;
  }
  st = r.state;
  views.get(r.key)?.term.write(`\r\n\x1b[2m[shell exited${code === null ? '' : ` with code ${code}`}]\x1b[0m\r\n`);
  if (st.visible === r.key) renderHead();
  uiChanged();
}

// ---------- show / hide ----------

async function toggleTerminal(): Promise<void> {
  const s = selected();
  const step = toggle(st, s?.id, s?.working_dir ?? '');
  st = step.state;
  if (step.kind === 'hide') { hideOverlay(); uiChanged(); return; }
  const reused = views.has(step.key);
  let view: View;
  try {
    view = await viewFor(step.key);
  } catch (err) {
    toast(`Could not load the terminal: ${String(err)}`);
    st = spawnFailed(st, step.key);
    uiChanged();
    return;
  }
  if (st.visible !== step.key) return; // hidden again while xterm loaded
  showOverlay(step.key);
  if (step.spawn) {
    if (reused) view.term.reset(); // a new shell after the last one exited
    await spawnShell(step.key, s?.working_dir);
  }
  view.term.focus();
}

/**
 * `<Space>gg` / `<Space>bb` (G2): show the terminal (starting its shell in the
 * open session's directory when it has none) and type `command` into it.
 */
export async function runInTerminal(command: string): Promise<void> {
  if (st.visible === null) await toggleTerminal();
  const key = st.visible;
  if (key === null) return;
  sendInput(key, `${command}\r`);
}

function showOverlay(key: string): void {
  const o = overlayEl();
  const active = document.activeElement as HTMLElement | null;
  if (o.hidden) returnFocus = active && active !== document.body && !o.contains(active) ? active : null;
  placeOverlay();
  for (const [k, v] of views) v.host.hidden = k !== key;
  o.hidden = false;
  renderHead();
  fitView(key);
  uiChanged();
}

function hideOverlay(): void {
  const o = overlayEl();
  const had = o.contains(document.activeElement);
  o.hidden = true;
  for (const v of views.values()) v.term.blur();
  if (had) {
    if (returnFocus?.isConnected) returnFocus.focus();
    else (document.activeElement as HTMLElement | null)?.blur();
  }
  returnFocus = null;
}

async function killTerminal(): Promise<void> {
  const key = st.visible ?? terminalKey(selected()?.id);
  const e = st.entries[key];
  if (!e?.ptyId) return;
  const wasVisible = st.visible === key;
  try {
    await invoke<boolean>('pty_kill', { id: e.ptyId });
  } catch (err) {
    toast(`Could not end the terminal: ${String(err)}`);
    return;
  }
  st = remove(st, key);
  const view = views.get(key);
  views.delete(key);
  view?.term.dispose();
  view?.host.remove();
  if (wasVisible) hideOverlay();
  toast('Terminal shell ended', true);
  uiChanged();
}

// ---------- wiring ----------

export function initTerminal(): void {
  for (const href of ['vendor/xterm.css', 'terminal.css']) {
    const css = document.createElement('link');
    css.rel = 'stylesheet';
    css.href = href;
    document.head.append(css);
  }
  // Ctrl-\ (Cmd-\ on macOS) from anywhere, ahead of the vim keys, text fields and the shell.
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (!isToggleChord(ev, currentPlatform())) return;
    ev.preventDefault();
    ev.stopPropagation();
    dispatch('toggle-terminal', { source: 'key' });
  }, { capture: true });
  // Flipping the Vim keys switch with the overlay open: drop a leftover normal mode, follow the label.
  window.addEventListener('rsi:vim-keys', () => { if (overlay) setMode(st.mode); });
  window.addEventListener('resize', () => { if (st.visible) { placeOverlay(); fitVisible(); } });
  // A reload drops the panes; take their shells with them (the backend also kills all on window close).
  window.addEventListener('pagehide', () => {
    for (const id of runningPtys(st)) void invoke('pty_kill', { id }).catch(() => undefined);
  });
  const ev = (globalThis as unknown as { __TAURI__?: TauriEvents }).__TAURI__?.event;
  if (!ev) return;
  void ev.listen('pty-output', (e) => {
    const p = e.payload as { id?: unknown; data?: unknown };
    if (typeof p?.id === 'string' && typeof p.data === 'string') onOutput(p.id, p.data);
  });
  void ev.listen('pty-exit', (e) => {
    const p = e.payload as { id?: unknown; code?: unknown };
    if (typeof p?.id === 'string') onExit(p.id, typeof p.code === 'number' ? p.code : null);
  });
}

/** The terminal slice's DOM side, collected by actions/runtime.generated.ts. */
export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runTerminal(id as TerminalActionId, inv),
  snapshot: () => ({ terminal: terminalSnapshot() }),
  init: initTerminal,
  // After the lifecycle slice (40): the order main.ts called the inits in before hub #3000.
  initOrder: 50,
};
