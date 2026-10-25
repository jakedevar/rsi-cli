// Key rules of a confirm dialog, pure (no DOM). Enter confirms and Escape
// cancels (the browser's dialog does that itself). With the Vim keys on, a
// dialog may also take the chord of the action that opened it as a second
// confirming press, like the TUI's repeat-the-chord confirm.

/** The land action's chord: <Space>gl. */
export const LAND_CHORD: readonly string[] = [' ', 'g', 'l'];

export interface ChordKey {
  readonly key: string;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
  readonly altKey?: boolean;
}

/** Feed one key to a chord matcher; `done` is true when the chord just completed. */
export function advanceChord(progress: number, ev: ChordKey, chord: readonly string[]): { progress: number; done: boolean } {
  if (ev.ctrlKey || ev.metaKey || ev.altKey) return { progress: 0, done: false };
  if (ev.key === chord[progress]) {
    const next = progress + 1;
    return next === chord.length ? { progress: 0, done: true } : { progress: next, done: false };
  }
  return { progress: ev.key === chord[0] ? 1 : 0, done: false };
}

export interface ConfirmKeyEvent extends ChordKey {
  readonly targetIsButton: boolean;
}

export interface ConfirmKeyState {
  progress: number;
}

/**
 * What a keydown inside the confirm dialog does. Enter confirms unless a button
 * has the focus (then the button's own press decides); the chord confirms only
 * with the Vim keys on; every other key is left alone.
 */
export function confirmKey(
  st: ConfirmKeyState, ev: ConfirmKeyEvent, opts: { vimKeysOn: boolean; chord?: readonly string[] },
): { st: ConfirmKeyState; confirm: boolean } {
  if (ev.key === 'Enter' && !ev.targetIsButton && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
    return { st: { progress: 0 }, confirm: true };
  }
  if (!opts.vimKeysOn || !opts.chord) return { st: { progress: 0 }, confirm: false };
  const r = advanceChord(st.progress, ev, opts.chord);
  return { st: { progress: r.progress }, confirm: r.done };
}
