// The `run` side of the action registry (registry.ts): one dispatch path for
// keys, the palette, the menu bar, right-click menus and toolbar buttons. It
// reads the DOM into a UiSnapshot, refuses an unavailable action with its
// reason, and calls the existing feature code. Daemon text is set with
// textContent only.
//
// A slice's actions run through its RUNTIME (a SliceRuntime exported from a
// src/actions/*.ts module, collected by the generated actions/runtime.generated.ts):
// no slice edits this file to register. The direct imports below are core
// actions that open a slice's dialog (bare :continue, <Space>gq, refresh).
import { interrupt } from './actions.js';
import { openContinue } from './actions/detail.js';
import { openQuestionModal } from './actions/lifecycle.js';
import { sliceRuntimes } from './actions/runtime.generated.js';
import { refreshLabels } from './actions/sessionlist-run.js';
import { api, type ConversationEvent } from './api.js';
import { needsAttention } from './approvals-model.js';
import { archive, clearPause, hardPause, pauseLevel } from './approvals.js';
import { setHideTools } from './conversation.js';
import { deliver } from './operator-messages.js';
import { focusComposer } from './actions/input.js';
import { $, toast } from './dom.js';
import { jumpCursor, nextFlagged, stepCursor } from './keybindings.js';
import { openLaunch } from './launch.js';
import { closePalette, isPaletteOpen, openPalette } from './palette.js';
import {
  type ActionId, type ActionSource, Deduper, type SliceSpec, type UiSnapshot, isActionId, tooltipText, unavailable,
} from './registry.js';
import { refreshHealth, refreshProjects, refreshSessions, renderSessionList, select, setView, visibleRows } from './sessions.js';
import { isReadOnly, selected, state, uiChanged } from './state.js';
import { currentPlatform, isVimKeysOn, openCommandLine, setVimKeys } from './vimkeys.js';

export interface Invocation {
  source: ActionSource;
  /** Numeric count typed before a key (`5j`), else null. */
  count?: number | null;
  /** Argument of a `:` command (`:continue fix it`). */
  arg?: string;
}

/**
 * The DOM side of one slice, exported as `RUNTIME` from a src/actions/*.ts
 * module. It runs exactly the actions of its `slice` (a SliceSpec).
 */
export interface SliceRuntime {
  /** The pure spec whose actions this runtime runs. */
  slice: SliceSpec;
  run(id: ActionId, inv: Invocation): void;
  /** This slice's UiSnapshot fields (see the UiSnapshot augmentation in its spec). */
  snapshot?(): Partial<UiSnapshot>;
  /** Called after every dispatched action, whoever ran it. */
  afterAction?(id: ActionId): void;
  /** Wires the slice's DOM once at startup (main.ts initSlices). */
  init?(): void;
  /** Startup order of init (lower first): the order its listeners register in. */
  initOrder?: number;
  /**
   * Take a core action before commands.ts runs it, where the TUI gives its key
   * a pane-specific meaning (G2: `gg` / `G` in the transcript, `a` / `o` / `O`
   * placing the message-box caret). Returns the action id that ran in its
   * place (what afterAction hears), or null to leave it to the core.
   */
  claim?(id: ActionId, inv: Invocation): ActionId | null;
}

const byOrder = (a: SliceRuntime, b: SliceRuntime): number => a.slice.order - b.slice.order;
let runtimeById: Map<string, SliceRuntime> | null = null;

/** The runtimes in registry order (SliceSpec.order). */
function runtimes(): SliceRuntime[] {
  return [...sliceRuntimes()].sort(byOrder);
}

function runtimeFor(id: ActionId): SliceRuntime | undefined {
  if (!runtimeById) {
    runtimeById = new Map();
    for (const rt of runtimes()) for (const a of rt.slice.actions) runtimeById.set(a.id, rt);
  }
  return runtimeById.get(id);
}

/** Run every slice's init once, in initOrder (main.ts calls this at startup). */
export function initSlices(): void {
  const order = (rt: SliceRuntime): number => rt.initOrder ?? rt.slice.order;
  for (const rt of runtimes().sort((a, b) => order(a) - order(b))) rt.init?.();
}

const deduper = new Deduper();
/** Sequence of the conversation event a context menu was opened on (the
 * pane may re-render before the menu answers, so not the node itself). */
let contextSeq: number | null = null;

export function setContextEvent(node: HTMLElement | null): void {
  const seq = node ? Number(node.dataset.seq) : NaN;
  contextSeq = Number.isFinite(seq) ? seq : null;
}

function contextNode(): HTMLElement | null {
  if (contextSeq === null) return null;
  return $('events').querySelector<HTMLElement>(`.ev[data-seq="${contextSeq}"]`);
}

function contextEventData(): ConversationEvent | undefined {
  return contextSeq === null ? undefined : state.events.find((e) => e.sequence === contextSeq);
}

function modalOpen(): boolean {
  for (const d of document.querySelectorAll('dialog[open]')) if (d.id !== 'palette') return true;
  return false;
}

function contextEventSnap(): UiSnapshot['event'] {
  const node = contextNode();
  const ev = contextEventData();
  if (!node && !ev) return null;
  return { foldable: node?.querySelector('details') != null, hasText: !!ev && ev.content.trim() !== '' };
}

export function snapshot(): UiSnapshot {
  const s = selected();
  const rows = visibleRows();
  const cur = cursor();
  return {
    open: s ? {
      status: s.status,
      readOnly: isReadOnly(s),
      pauseLevel: pauseLevel(s.id),
      hasQuestion: !$('approval-banner').hidden && $('approval-banner').querySelector('input, textarea') !== null,
    } : null,
    cursorOnRow: cur !== null && rows.some((r) => r.session.id === cur),
    rowCount: rows.length,
    attentionRows: rows.filter((r) => needsAttention(r.session)).length,
    view: state.view,
    vimKeys: isVimKeysOn(),
    hideTools: state.hideTools,
    showFinished: $<HTMLInputElement>('show-finished').checked,
    modalOpen: modalOpen(),
    event: contextEventSnap(),
    ...Object.assign({}, ...runtimes().map((rt) => rt.snapshot?.() ?? {})),
  };
}

/** Run a registry action from any surface. */
export function dispatch(id: ActionId, inv: Invocation): void {
  if (!deduper.admit(id, inv.source, Date.now())) return;
  if (inv.source !== 'palette' && id !== 'palette' && id !== 'help' && isPaletteOpen()) closePalette();
  const why = unavailable(id, snapshot());
  if (why) {
    toast(why);
  } else {
    const ran = claimed(id, inv) ?? (run(id, inv), id);
    for (const rt of runtimes()) rt.afterAction?.(ran);
  }
  if (inv.source === 'context') contextSeq = null;
  uiChanged();
}

/** A slice's claim on a core action (SliceRuntime.claim), else null. */
function claimed(id: ActionId, inv: Invocation): ActionId | null {
  if (runtimeFor(id)) return null;
  for (const rt of runtimes()) {
    const ran = rt.claim?.(id, inv);
    if (ran) return ran;
  }
  return null;
}

// ---------- implementations ----------

function rowIds(): string[] {
  return visibleRows().map((r) => r.session.id);
}

function cursor(): string | null {
  return state.cursorId ?? state.selectedId;
}

function moveCursor(id: string | null): void {
  if (!id) return;
  state.cursorId = id;
  renderSessionList();
  for (const li of $('session-list').querySelectorAll<HTMLElement>('.session-item')) {
    if (li.dataset.id === id) { li.scrollIntoView({ block: 'nearest' }); break; }
  }
}

async function send(text: string): Promise<void> {
  const s = selected();
  if (!s) return;
  try {
    const how = await deliver(s, text);
    if (how === 'sent') toast(text === 'continue' ? 'Sent continue' : 'Sent', true);
    void refreshSessions();
  } catch (e) {
    toast(`Send failed: ${String(e)}`);
  }
}

async function launch(prefill: string | undefined): Promise<void> {
  await openLaunch();
  if (prefill) {
    const q = $<HTMLFormElement>('launch-form').elements.namedItem('query') as HTMLTextAreaElement | null;
    if (q) q.value = prefill;
  }
}

/** Clipboard write; a native menu click is not a page gesture, so fall back to execCommand. */
export async function copyText(text: string, what: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    toast(`Copied ${what}`, true);
    return;
  } catch {
    // fall through
  }
  const area = document.createElement('textarea');
  area.value = text;
  area.setAttribute('readonly', '');
  area.style.position = 'fixed';
  area.style.opacity = '0';
  document.body.append(area);
  area.select();
  const ok = document.execCommand('copy');
  area.remove();
  toast(ok ? `Copied ${what}` : `Could not copy ${what}: the webview refused clipboard access`, ok);
}

function toggleEvent(node: HTMLElement): void {
  const folds = [...node.querySelectorAll('details')];
  const open = folds.some((d) => !d.open);
  for (const d of folds) d.open = open;
}

function run(id: ActionId, inv: Invocation): void {
  const slice = runtimeFor(id);
  if (slice) return slice.run(id, inv);
  const count = inv.count ?? null;
  const n = count ?? 1;
  switch (id) {
    case 'cursor-down': return moveCursor(stepCursor(rowIds(), cursor(), n));
    case 'cursor-up': return moveCursor(stepCursor(rowIds(), cursor(), -n));
    case 'cursor-first': return moveCursor(jumpCursor(rowIds(), count, 'first'));
    case 'cursor-last': return moveCursor(jumpCursor(rowIds(), count, 'last'));
    case 'open': {
      const c = cursor();
      if (c) select(c);
      return;
    }
    case 'next-attention':
    case 'prev-attention': {
      const rows = visibleRows();
      const next = nextFlagged(rowIds(), rows.map((r) => needsAttention(r.session)), cursor(), id === 'next-attention' ? 1 : -1);
      if (next) { moveCursor(next); select(next); }
      return;
    }
    case 'focus-search': $('search').focus(); return;
    case 'refresh':
      void Promise.all([refreshHealth(), refreshProjects(), refreshLabels(), refreshSessions()]).then(() => toast('Refreshed', true));
      return;
    case 'launch': void launch(inv.arg); return;
    case 'quit': void api.quit().catch((e) => toast(`Quit failed: ${String(e)}`)); return;
    case 'view-active': void setView('active'); return;
    case 'view-archived': void setView('archived'); return;
    case 'toggle-finished': {
      const box = $<HTMLInputElement>('show-finished');
      box.checked = !box.checked;
      renderSessionList();
      return;
    }
    case 'toggle-tools': setHideTools(!state.hideTools); return;
    case 'toggle-vim-keys': setVimKeys(!isVimKeysOn()); return;
    case 'palette': if (isPaletteOpen()) closePalette(); else openPalette(); return;
    case 'help': openPalette(); return;
    case 'command-line': openCommandLine(); return;
    case 'soft-interrupt': void interrupt(); return;
    case 'hard-pause': void hardPause(); return;
    case 'clear-pause': void clearPause(); return;
    case 'archive': void archive(); return;
    case 'quick-continue': void send('continue'); return;
    case 'continue': if (inv.arg) void send(inv.arg); else openContinue(); return;
    case 'focus-composer': focusComposer(inv.arg); return;
    case 'answer-question': openQuestionModal(); return;
    case 'copy-session-id': {
      const s = selected();
      if (s) void copyText(s.id, 'session ID');
      return;
    }
    case 'copy-event': {
      const ev = contextEventData();
      if (ev) void copyText(ev.content, 'message text');
      return;
    }
    case 'toggle-event': {
      const node = contextNode();
      if (node) toggleEvent(node);
      return;
    }
    default: return;
  }
}

// ---------- buttons ----------

/** Toolbar buttons with data-action run through dispatch. */
export function wireActionButtons(): void {
  document.addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement | null)?.closest<HTMLButtonElement>('button[data-action]');
    const id = btn?.dataset.action;
    if (!btn || !id || btn.disabled || !isActionId(id)) return;
    dispatch(id, { source: 'button' });
  });
  for (const b of document.querySelectorAll<HTMLElement>('[data-action]')) {
    const id = b.dataset.action ?? '';
    if (isActionId(id)) b.title = tooltipText(id, currentPlatform());
  }
}
