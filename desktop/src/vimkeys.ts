// DOM adapter for the vim key engine: one keydown listener, the "Vim keys"
// toggle, the which-key hint and the `:` command line. All decisions live in
// keyengine.ts / keybindings.ts (pure, unit-tested); a bound key dispatches
// its registry action through commands.ts. Daemon text is set with
// textContent only.
import { dispatch } from './commands.js';
import { $, el, toast } from './dom.js';
import {
  CONFIRM_MS, KeyEngine, MAX_COMPLETIONS, type Outcome, type Platform, SEQUENCE_TIMEOUT_MS, detectPlatform, isTypingTarget,
  readVimKeys, routeKey, writeVimKeys,
} from './keyengine.js';
import { type DesktopAction, desktopKeymap } from './keybindings.js';
import { uiChanged } from './state.js';
import { bindDialogGate } from './dialog-gate.js';
import { installKeyHints } from './key-hints.js';
import { focusOwnsKey } from './modal-gate.js';

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

/** The "Vim keys" setting: engine, top-bar checkbox and local storage together. */
export function setVimKeys(on: boolean): void {
  engine.setEnabled(on);
  $<HTMLInputElement>('vim-keys').checked = on;
  writeVimKeys(storage(), on);
  refreshKeyHints();
  render();
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
  toggle.checked = readVimKeys(storage());
  engine.setEnabled(toggle.checked);
  bindDialogGate(isVimKeysOn);
  // Hint lines and "(x)" button labels follow the switch (local #129); also data-vim-keys on <html>.
  refreshKeyHints = installKeyHints(document.body, isVimKeysOn);
  refreshKeyHints();
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
