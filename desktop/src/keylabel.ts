// One place that spells modifier keys for the person at the keyboard: macOS
// shows Cmd/Option (the Command key stands in for Ctrl, see keyengine.ts),
// every other platform shows Ctrl/Alt. Pure and platform-explicit so it can
// be tested; callers pass currentPlatform().
import type { Platform } from './keyengine.js';

export type Modifier = 'ctrl' | 'alt' | 'shift';

/** The name printed for a modifier on this platform. */
export function modifierName(mod: Modifier, platform: Platform): string {
  if (mod === 'shift') return 'Shift';
  if (mod === 'ctrl') return platform === 'mac' ? 'Cmd' : 'Ctrl';
  return platform === 'mac' ? 'Option' : 'Alt';
}

/** A chord such as Cmd+Enter or Ctrl+Alt+G. `sep` is `+` in prose, `-` in the TUI's hint notation. */
export function chordLabel(mods: readonly Modifier[], key: string, platform: Platform, sep = '+'): string {
  return [...mods.map((m) => modifierName(m, platform)), key].join(sep);
}

/** The key that sends the message box when Enter alone adds a line. */
export function sendChord(platform: Platform): string {
  return chordLabel(['ctrl'], 'Enter', platform);
}

/** The message box placeholder when Enter adds a line. */
export function sendPlaceholder(platform: Platform): string {
  return `Message this session (${sendChord(platform)} to send)`;
}

/** The message box placeholder for an archived session: it is read-only until it is restored (local #268). */
export const ARCHIVED_PLACEHOLDER = 'Archived: Restore it to message it';

/** The message box placeholder for the open session after it left the list as something other than archived (local #309). */
export const DEPARTED_PLACEHOLDER = 'This session is no longer in the list';

/** The message box placeholder for a finished session: a message continues it (local #225). */
export function continuePlaceholder(platform: Platform): string {
  return `Send a message to continue this session (${sendChord(platform)} to send)`;
}

/**
 * A TUI hint such as `Ctrl-N` or `Ctrl-Alt-G` as typed on this platform. On
 * macOS Ctrl-letter is the Command key and Alt is Option; elsewhere unchanged.
 */
export function hintLabel(key: string, platform: Platform): string {
  if (platform !== 'mac') return key;
  return key.replace(/^Ctrl-/, 'Cmd-').replace(/(^|-)Alt-/g, '$1Option-');
}

/** Prose with `Ctrl+X` / `Alt+X` chords respelled for the platform (a plain `Ctrl` word stays). */
export function proseLabel(text: string, platform: Platform): string {
  if (platform !== 'mac') return text;
  return text.replace(/\bCtrl\+/g, 'Cmd+').replace(/\bAlt\+/g, 'Option+');
}
