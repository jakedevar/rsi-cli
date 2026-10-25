// ESP Square overlay (DOM side of esp-model.ts; slice 11, hub #3013): the
// TUI's `<Space>gc` game. A finished game is saved with SaveEspGame, fire
// and forget, as the TUI does.
import { el, toast } from './dom.js';
import {
  type EspState, HIT_COLOR, MISS_COLOR, ROUNDS, SQUARE_KEYS, espHint, espKey, flashTextOn, liveFlash, newGame, saveParams, squareColorsOn,
} from './esp-model.js';
import { parseHex } from './theme-model.js';
import { graphsApi } from './graphs-api.js';
import { engineBusy } from './graphs.js';
import { uiChanged } from './state.js';
import { focusOwnsKey, paneKeyAllowed } from './modal-gate.js';
import { isVimKeysOn } from './vimkeys.js';

let s: EspState = newGame(Math.random, 0);
let open = false;
let root: HTMLElement | null = null;
let flashTimer: number | undefined;

export const isEspOpen = (): boolean => open;

export function openEsp(): void {
  if (open) { closeEsp(); return; }
  open = true;
  s = newGame(Math.random, performance.now());
  root!.hidden = false;
  draw();
  root!.focus({ preventScroll: true });
  uiChanged();
}

export function closeEsp(): void {
  if (!open) return;
  open = false;
  root!.hidden = true;
  uiChanged();
}

/** The overlay owns every plain key while it is open, as the TUI's does. */
function press(key: string): void {
  const res = espKey(s, { key }, Math.random, performance.now());
  s = res.state;
  if (res.effect === 'close') { closeEsp(); return; }
  if (res.effect === 'save') {
    const p = saveParams(s);
    void graphsApi.espSave(p.score as number, p.rounds_played as number, p.p_value as number, p.round_details as string)
      .catch((e) => toast(`ESP game not saved: ${String(e)}`));
  }
  draw();
  window.clearTimeout(flashTimer);
  if (s.flash) flashTimer = window.setTimeout(draw, Math.max(0, s.flash.until - performance.now()) + 5);
}

function draw(): void {
  if (!open || !root) return;
  const card = root.querySelector<HTMLElement>('.esp-card')!;
  const flash = liveFlash(s, performance.now());
  const head = el('div', 'esp-head', `Round ${s.round}/${ROUNDS}`);
  if (s.interactive || s.round === ROUNDS) head.append(el('span', 'esp-score', `Correct ${s.correct}/${ROUNDS}`));
  const grid = el('div', 'esp-grid');
  // The glyph colours follow the theme: read on the card's own surface so Light and High Contrast stay readable (#301).
  const rootVars = getComputedStyle(document.documentElement);
  const surface = parseHex(rootVars.getPropertyValue('--mantle')) ?? '#181825';
  const textColor = parseHex(rootVars.getPropertyValue('--text')) ?? '#cdd6f4';
  const colors = squareColorsOn(surface, textColor);
  SQUARE_KEYS.forEach((k, i) => {
    const sq = el('button', 'esp-sq', k);
    sq.type = 'button';
    sq.tabIndex = -1;
    const color = colors[i]!;
    if (flash && flash.index === i) {
      sq.style.background = flash.hit ? HIT_COLOR : MISS_COLOR;
      sq.style.borderColor = flash.hit ? HIT_COLOR : MISS_COLOR;
      sq.style.color = flashTextOn(flash.hit);
      sq.classList.add('flash');
    } else if (s.cursor === i) {
      sq.style.borderColor = textColor;
      sq.style.color = textColor;
      sq.classList.add('cursor');
    } else {
      sq.style.borderColor = color;
      sq.style.color = color;
    }
    sq.addEventListener('click', () => { press(k); });
    grid.append(sq);
  });
  const foot = el('div', 'esp-foot');
  if (s.round === ROUNDS) foot.append(el('div', 'esp-message', s.message));
  foot.append(el('div', 'meta', espHint(s)));
  card.replaceChildren(el('h2', '', 'ESP Square'), head, grid, foot);
}

export function initEsp(): void {
  root = el('div', 'esp-overlay');
  root.id = 'esp-view';
  root.hidden = true;
  root.tabIndex = -1;
  root.setAttribute('role', 'dialog');
  root.setAttribute('aria-label', 'ESP Square');
  root.append(el('div', 'esp-card'));
  root.addEventListener('click', (ev) => { if (ev.target === root) closeEsp(); });
  document.body.append(root);
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (!open || ev.isComposing || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (!paneKeyAllowed(isVimKeysOn(), ev)) return;
    if (document.querySelector('dialog[open]') || engineBusy()) return;
    const t = document.activeElement as HTMLElement | null;
    if (t?.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]')) return;
    // Space and `:` reach the app keys (the palette, the command line).
    if (ev.key === ' ' || ev.key === ':' || ev.key === 'Shift') return;
    ev.preventDefault();
    ev.stopImmediatePropagation();
    press(ev.key);
  }, true);
}
