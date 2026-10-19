// ESP Square (hub #3013, slice 11): the TUI's `<Space>gc` 3×3 guessing game
// (crates/rsi/src/overlay/esp_square.rs and ui/overlay/esp_square.rs) as pure
// state. The random target and the clock are injected so the rules are
// testable; esp.ts draws it and saves a finished game with SaveEspGame.

export const ROUNDS = 12;
export const FLASH_MS = 300;
export const SAME_KEY_DEBOUNCE_MS = 600;

/** Square labels in key position: u i o / j k l / m , . */
export const SQUARE_KEYS = ['u', 'i', 'o', 'j', 'k', 'l', 'm', ',', '.'] as const;
/** The TUI's fixed feedback colours (theme-independent on purpose, ui/overlay/esp_square.rs). */
export const SQUARE_COLORS = ['#538fff', '#b431e6', '#f7ed65', '#28d2ab', '#fca207', '#f6ccf9', '#00ffff', '#268189', '#2d1a77'] as const;
export const HIT_COLOR = '#32cd32';
export const MISS_COLOR = '#dc3232';

export interface RoundDetail { pick: number; target: number; guessMs: number }

export interface EspState {
  round: number;
  correct: number;
  /** Show the running score (t). */
  interactive: boolean;
  details: RoundDetail[];
  message: string;
  flash: { index: number; hit: boolean; until: number } | null;
  target: number;
  cursor: number | null;
  lastGuess: { key: string; at: number } | null;
  startedAt: number;
}

export type Rng = () => number;
/** A target square 0..8 from a [0,1) source. */
export const pickTarget = (rng: Rng): number => Math.min(8, Math.floor(rng() * 9));

export function newGame(rng: Rng, now: number): EspState {
  return { round: 0, correct: 0, interactive: true, details: [], message: '', flash: null, target: pickTarget(rng), cursor: null, lastGuess: null, startedAt: now };
}

export function keyToIndex(key: string): number | null {
  const i = (SQUARE_KEYS as readonly string[]).indexOf(key);
  return i < 0 ? null : i;
}

export function moveCursor(cur: number, dir: 'up' | 'down' | 'left' | 'right'): number {
  const row = Math.trunc(cur / 3);
  const col = cur % 3;
  if (dir === 'up') return row > 0 ? cur - 3 : cur;
  if (dir === 'down') return row < 2 ? cur + 3 : cur;
  if (dir === 'left') return col > 0 ? cur - 1 : cur;
  return col < 2 ? cur + 1 : cur;
}

function binomial(n: number, k: number): number {
  if (k > n) return 0;
  let r = 1;
  for (let i = 0; i < k; i++) r *= (n - i) / (i + 1);
  return r;
}

/** P(X >= k) for 12 rounds at p = 1/9. */
export function espPValue(k: number): number {
  const p = 1 / 9;
  let cdf = 0;
  for (let i = 0; i < k; i++) cdf += binomial(ROUNDS, i) * p ** i * (1 - p) ** (ROUNDS - i);
  return 1 - cdf;
}

export function interpretation(p: number): string {
  if (p <= 0.0001) return 'Psychic';
  if (p <= 0.001) return 'Extremely significant';
  if (p <= 0.01) return 'Highly significant';
  if (p <= 0.05) return 'Significant';
  if (p <= 0.15) return 'Lucky';
  return 'Normal';
}

export function gameOverMessage(correct: number): string {
  const p = espPValue(correct);
  return `Game Over — Score: ${correct}/12 — p = ${p.toFixed(4)} (${interpretation(p)})`;
}

/** The TUI's persisted round_details JSON. */
export function roundDetailsJson(details: readonly RoundDetail[]): string {
  return JSON.stringify(details.map((d) => ({ pick: d.pick, target: d.target, hit: d.pick === d.target, guess_ms: d.guessMs })));
}

/** SaveEspGame params (client.rs save_esp_game). */
export function saveParams(s: EspState): Record<string, unknown> {
  return { score: s.correct, rounds_played: s.details.length, total_rounds: ROUNDS, p_value: espPValue(s.correct), round_details: roundDetailsJson(s.details) };
}

export type EspEffect = 'none' | 'close' | 'save';
export interface EspKey { key: string }

const ARROWS: Record<string, 'up' | 'down' | 'left' | 'right'> = { ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left', ArrowRight: 'right' };

/** The flash, cleared once its 300 ms are up. */
export function liveFlash(s: EspState, now: number): EspState['flash'] {
  return s.flash && now < s.flash.until ? s.flash : null;
}

/** One key (handle_esp_square_key). `save` fires once, when round 12 is played. */
export function espKey(s: EspState, k: EspKey, rng: Rng, now: number): { state: EspState; effect: EspEffect } {
  const inGame = s.round < ROUNDS;
  let pick: number | null = null;
  if (k.key === 'Enter') pick = s.cursor !== null && inGame ? s.cursor : null;
  else if (inGame) pick = keyToIndex(k.key);

  if (k.key === 'r' || (k.key === 'Enter' && pick === null)) return { state: newGame(rng, now), effect: 'none' };
  if (k.key === 'Escape' || k.key === 'q') return { state: s, effect: 'close' };
  if (!inGame) return { state: s, effect: 'none' };
  if (k.key === 't') return { state: { ...s, interactive: !s.interactive }, effect: 'none' };
  if (k.key === ';') return { state: { ...s, flash: { index: s.target, hit: false, until: now + FLASH_MS }, target: pickTarget(rng) }, effect: 'none' };
  const dir = ARROWS[k.key];
  if (dir) return { state: { ...s, cursor: s.cursor === null ? 0 : moveCursor(s.cursor, dir) }, effect: 'none' };
  if (pick === null) return { state: s, effect: 'none' };
  let lastGuess = s.lastGuess;
  if (k.key !== 'Enter') {
    if (lastGuess && lastGuess.key === k.key && now - lastGuess.at < SAME_KEY_DEBOUNCE_MS) return { state: s, effect: 'none' };
    lastGuess = { key: k.key, at: now };
  }
  const hit = pick === s.target;
  const round = s.round + 1;
  const correct = s.correct + (hit ? 1 : 0);
  const next: EspState = {
    ...s,
    lastGuess,
    round,
    correct,
    details: [...s.details, { pick, target: s.target, guessMs: Math.max(0, Math.round(now - s.startedAt)) }],
    flash: { index: hit ? pick : s.target, hit, until: now + FLASH_MS },
    target: pickTarget(rng),
    message: round === ROUNDS ? gameOverMessage(correct) : s.message,
  };
  return { state: next, effect: round === ROUNDS ? 'save' : 'none' };
}

export function espHint(s: EspState): string {
  return s.round >= ROUNDS ? 'r: new game  Esc: close' : ';: pass  r: reset  t: toggle  Esc: close';
}
