// DOM adapter for the vim key engine: one keydown listener, the "Vim keys"
// toggle, the which-key hint and the `:` command line. All decisions live in
// keyengine.ts / keybindings.ts (pure, unit-tested); a bound key dispatches
// its registry action through commands.ts. Daemon text is set with
// textContent only.
import { dispatch } from './commands.js';
import { $, el, toast } from './dom.js';
import {
  CONFIRM_MS, KeyEngine, MAX_COMPLETIONS, type Outcome, type Platform, SEQUENCE_TIMEOUT_MS, detectPlatform, isTypingTarget,
  VIM_FIRST_RUN_NOTE, answerVimKeysFirstRun, routeKey, vimKeysFirstRun, writeVimKeys,
} from './keyengine.js';
import { type DesktopAction, desktopKeymap } from './keybindings.js';
import { uiChanged } from './state.js';
import { bindDialogGate } from './dialog-gate.js';
import { installKeyHints } from './key-hints.js';
import { bindCommandLineOpen, focusOwnsKey } from './modal-gate.js';

let engine: KeyEngine<DesktopAction>;
let platform: Platform = 'other';
let expiry: number | undefined;
let refreshKeyHints: () => void = () => {};

/** A chord prefix left waiting expires (keybindings.md "Key Sequence Timing"); the which-key hint goes with it. */
function armExpiry(): void {
  window.clearTimeout(expiry);
  expiry = window.setTimeout(() => { if (engine.expire(Date.now())) render(); }, SEQUENCE_TIMEOUT_MS + 20);
}

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

function platformString(): string {
  const nav = navigator as Navigator & { userAgentData?: { platform?: string } };
  return nav.userAgentData?.platform || nav.platform || nav.userAgent;
}

// ---------- rendering ----------

function render(): void {
  const v = engine.view();
  const hint = $('keyhint');
  if (v.mode === 'normal' && v.pending !== '') {
    const head = el('div', 'keyhint-head', v.pending);
    const grid = el('div', 'keyhint-grid');
    for (const h of v.hints) {
      const row = el('div', `keyhint-row${h.bound ? '' : ' unbound'}${h.prefix ? ' prefix' : ''}`);
      row.append(el('span', 'k', h.key), el('span', 't', h.bound || h.prefix ? h.title : `${h.title} (not in the desktop yet)`));
      grid.append(row);
    }
    hint.replaceChildren(head, grid);
    hint.hidden = false;
  } else {
    hint.hidden = true;
  }
  const bar = $('cmdline');
  if (v.command) {
    const line = el('div', 'cmdline-text');
    line.append(':', v.command.text, el('span', 'cmdline-caret'));
    const list = el('div', 'cmdline-completions');
    const shown = v.command.completions.slice(0, MAX_COMPLETIONS);
    shown.forEach((c, i) => list.append(el('span', i === v.command!.selected ? 'sel' : '', c)));
    const more = v.command.completions.length - shown.length;
    if (more > 0) list.append(el('span', 'cmd-more', `+${more}`));
    bar.replaceChildren(line, list);
    bar.hidden = false;
  } else {
    bar.hidden = true;
  }
}

// ---------- surface for commands.ts ----------

export function currentPlatform(): Platform {
  return platform;
}

export function isVimKeysOn(): boolean {
  return engine?.isEnabled() ?? true;
}

/** A chord, count or `:` line is half typed: its next key (Enter, arrows) belongs to the engine. */
export function vimKeysBusy(): boolean {
  return engine?.busy() ?? false;
}

/** The "Vim keys" setting: engine, top-bar checkbox and local storage together. */
export function setVimKeys(on: boolean): void {
  engine.setEnabled(on);
  $<HTMLInputElement>('vim-keys').checked = on;
  // Changing the switch anywhere (top bar, Settings, palette) answers a first-run question still open.
  if (firstRunCard) { answerVimKeysFirstRun(storage(), on); closeFirstRun(); }
  writeVimKeys(storage(), on);
  refreshKeyHints();
  render();
  if (typeof window.dispatchEvent === 'function') window.dispatchEvent(new Event('rsi:vim-keys')); // the terminal overlay drops its modes
}

// ---------- first-run question (local #208) ----------

let firstRunCard: HTMLElement | null = null;

function closeFirstRun(): void {
  firstRunCard?.remove();
  firstRunCard = null;
}

/**
 * "Use Vim-style keys?" once, on a new install: No is the default (already applied and focused),
 * Esc answers No. A card, not a modal dialog, so nothing else waits on it. Settings › Workspace › Editing Mode
 * (Vim keys in this window) and the top-bar switch change the answer later.
 */
function askVimKeys(): void {
  const card = el('section', 'first-run-card');
  card.id = 'vim-first-run';
  card.setAttribute('role', 'dialog');
  card.setAttribute('aria-label', 'Use Vim-style keys?');
  const no = el('button', 'btn primary', 'No');
  const yes = el('button', 'btn', 'Yes');
  no.type = 'button';
  yes.type = 'button';
  const answer = (on: boolean): void => {
    answerVimKeysFirstRun(storage(), on);
    closeFirstRun();
    if (on !== isVimKeysOn()) setVimKeys(on);
    uiChanged();
  };
  no.addEventListener('click', () => answer(false));
  yes.addEventListener('click', () => answer(true));
  card.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Escape') return;
    ev.preventDefault();
    ev.stopPropagation();
    answer(false);
  });
  const actions = el('div', 'actions');
  actions.append(no, yes);
  card.append(
    el('h2', undefined, 'Use Vim-style keys?'),
    el('p', undefined, 'Vim-style keys let you drive the app with the rsi TUI\u2019s single-letter keys (j and k move, : runs a command). Everything also works with the mouse and the menus.'),
    el('p', 'first-run-note', VIM_FIRST_RUN_NOTE),
    actions,
  );
  firstRunCard = card;
  document.body.append(card);
  no.focus();
}


/** Run one `:` line typed elsewhere (the palette); works with Vim keys off. */
export function runCommandLine(line: string): void {
  apply(engine.runCommand(line));
  render();
}

export function openCommandLine(): void {
  engine.openCommandLine();
  render();
}

function apply(o: Outcome<DesktopAction>): void {
  switch (o.kind) {
    case 'run': dispatch(o.action, { source: 'key', count: o.count, ...(o.arg ? { arg: o.arg } : {}) }); break;
    case 'confirm': toast(`Press ${o.keys} again within ${CONFIRM_MS / 1000} s: ${o.title}`); break;
    case 'missing': toast(`${o.keys} (${o.title}) is not in the desktop yet`); break;
    case 'retired': toast(`${o.keys} is retired in the TUI: ${o.why}`); break;
    case 'unbound': toast(`${o.keys} is not bound`); break;
    case 'error': toast(o.message); break;
    default: break;
  }
}

// ---------- wiring ----------

export function initVimKeys(): void {
  engine = new KeyEngine(desktopKeymap());
  platform = detectPlatform(platformString());
  const toggle = $<HTMLInputElement>('vim-keys');
  const first = vimKeysFirstRun(storage());
  toggle.checked = first.on;
  engine.setEnabled(toggle.checked);
  bindDialogGate(isVimKeysOn);
  bindCommandLineOpen(() => engine.commandLineOpen());
  // Hint lines and "(x)" button labels follow the switch (local #129); also data-vim-keys on <html>.
  refreshKeyHints = installKeyHints(document.body, isVimKeysOn);
  refreshKeyHints();
  if (first.ask) askVimKeys();
  toggle.addEventListener('change', () => {
    setVimKeys(toggle.checked);
    toggle.blur();
    uiChanged();
  });
  document.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    const o = routeKey(engine, ev, {
      platform,
      target: document.activeElement as HTMLElement | null,
      modalOpen: document.querySelector('dialog[open]') !== null,
      now: Date.now(),
    });
    if (o.kind === 'pass') return;
    ev.preventDefault();
    ev.stopPropagation();
    apply(o);
    render();
    if (o.kind === 'pending') armExpiry();
  });
  // Clicking into a text field abandons a half-typed chord or command line.
  document.addEventListener('focusin', (ev) => {
    if (engine.busy() && isTypingTarget(ev.target as HTMLElement | null)) { engine.reset(); render(); }
  });
  document.addEventListener('paste', (ev) => {
    const text = ev.clipboardData?.getData('text/plain') ?? '';
    if (engine.insertText(text).kind === 'pass') return;
    ev.preventDefault();
    render();
  });
}
