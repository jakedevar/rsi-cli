// The Vim keys switch for pane key handlers (hub #3101). With the switch off,
// the modal letter keys stop: a pane's own window keydown handler keeps only
// the standard desktop keys (arrows, Enter, Escape, Tab, paging) and lets every
// other key through untouched, so the pane stays usable with the mouse and the
// ordinary navigation keys. Every pane capture handler calls paneKeyAllowed
// first; tests/modal-gate.test.mjs fails when a new one does not.

export const STANDARD_KEYS: ReadonlySet<string> = new Set([
  'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight',
  'Enter', 'Escape', 'Tab',
  'PageUp', 'PageDown', 'Home', 'End',
]);

/** Whether a pane's own handler may act on this key, given the Vim keys switch. */
export function paneKeyAllowed(vimKeysOn: boolean, ev: { readonly key: string }): boolean {
  return vimKeysOn || STANDARD_KEYS.has(ev.key);
}

/** The parts of a keydown event the dialog gate reads. */
export interface DialogKeyEvent {
  readonly key: string;
  readonly target: unknown;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
  readonly altKey?: boolean;
  readonly isComposing?: boolean;
}

/** Input types that are buttons, not text: a letter pressed on one is not typing. */
const NON_TEXT_INPUT_TYPES: ReadonlySet<string> = new Set([
  'button', 'submit', 'reset', 'checkbox', 'radio', 'file', 'image', 'range', 'color',
]);

interface EditableLike extends KeyTarget { tagName?: string; type?: string; isContentEditable?: boolean }

/** Whether the key is being typed into a text field, which a dialog never takes letters from. */
function typingTarget(target: EditableLike | null): boolean {
  if (!target) return false;
  if (target.isContentEditable) return true;
  const tag = (target.tagName ?? '').toUpperCase();
  if (tag === 'INPUT') return !NON_TEXT_INPUT_TYPES.has((target.type ?? 'text').toLowerCase());
  return tag === 'TEXTAREA' || tag === 'SELECT';
}

/**
 * Whether an overlay dialog must not see this key (hub #3101 follow-up, local #111).
 * The overlay dialogs are a separate layer from the panes: they list single-letter
 * keys (a/d/j/k/q/y/n...) in a hint line and act on them with no field focused.
 * With the Vim keys switch off those letters do nothing, the same rule as the
 * panes: Enter, Escape, arrows, Tab and paging stay, every action is still on its
 * button, and typing into a text field is untouched. `dialogOpen` says a dialog is
 * open while the key is aimed at the page body (a click on dialog text can leave
 * focus there). Modifier chords (Ctrl+K and the like) are not letter keys.
 */
export function dialogKeyBlocked(vimKeysOn: boolean, ev: DialogKeyEvent, dialogOpen = false): boolean {
  if (vimKeysOn || STANDARD_KEYS.has(ev.key)) return false;
  if (ev.ctrlKey || ev.metaKey || ev.altKey || ev.isComposing) return false;
  const target = ev.target as EditableLike | null;
  if (typingTarget(target)) return false;
  if (target?.closest?.('dialog')) return true;
  const tag = (target?.tagName ?? '').toUpperCase();
  return dialogOpen && (target === null || tag === 'BODY' || tag === 'HTML');
}

/** The element a key event is aimed at, as much of it as the gates need. */
export interface KeyTarget { closest?(selector: string): unknown }

/** Activatable controls: Enter and Space belong to them, never to a row or pane action. */
export const ACTIVATABLE_SELECTOR = [
  'button', 'a[href]', 'summary',
  'input[type="button"]', 'input[type="submit"]', 'input[type="reset"]',
  'input[type="checkbox"]', 'input[type="radio"]', 'input[type="file"]',
  '[role="button"]',
].join(', ');

/**
 * Whether Enter or Space must be left to the focused control (hub #3112). A mouse
 * click focuses a button; the next Enter or Space activates that button, never
 * the dialog's or pane's row action (which can write trust or delete).
 */
export function focusOwnsKey(key: string, target: KeyTarget | null): boolean {
  return (key === 'Enter' || key === ' ') && !!target?.closest?.(ACTIVATABLE_SELECTOR);
}

/** The parts of a keydown event the activation guard touches. */
export interface GuardEvent {
  readonly key: string;
  readonly target: unknown;
  stopImmediatePropagation(): void;
  preventDefault(): void;
}

interface Activatable extends KeyTarget { tagName?: string; click?(): void; hasAttribute?(name: string): boolean }

/** Whether the browser activates this control itself on Enter/Space: an a only with href (#3158). */
function nativelyActivated(control: Activatable): boolean {
  const tag = (control.tagName ?? '').toUpperCase();
  if (tag === 'A') return !!control.hasAttribute?.('href');
  return tag === 'BUTTON' || tag === 'SUMMARY' || tag === 'INPUT';
}

/**
 * The one authority for Enter and Space on a focused control (hub #3112, #3120,
 * #3128). Installed on window in the capture phase before any other listener
 * (src/activation-guard.ts, imported first by main.ts), it stops every later
 * listener, whatever way it was registered, from seeing the key.
 *
 * Contract (hub PM ruling on #3159, 2026-10-19):
 * - A native activatable target (button, a with href, summary, an input of type
 *   button/submit/reset/checkbox/radio/file): stopImmediatePropagation only,
 *   never preventDefault, so the browser's own activation still fires.
 * - A non-native [role=button] (a span, a div, an a WITHOUT href, #3158): it has
 *   no browser activation, so stopImmediatePropagation, preventDefault (Space
 *   would scroll; the WAI-ARIA custom-button behaviour) and exactly one
 *   emulated click.
 * Returns what it did.
 */
export function activationGuard(ev: GuardEvent): 'native' | 'emulated' | null {
  if (ev.key !== 'Enter' && ev.key !== ' ') return null;
  const target = ev.target as Activatable | null;
  const control = target?.closest?.(ACTIVATABLE_SELECTOR) as Activatable | null | undefined;
  if (!control) return null;
  ev.stopImmediatePropagation();
  if (nativelyActivated(control)) return 'native';
  ev.preventDefault();
  control.click?.();
  return 'emulated';
}

/** Register the guard: window, capture phase, before anything else. */
export function installActivationGuard(win: { addEventListener(type: 'keydown', listener: (ev: KeyboardEvent) => void, capture: boolean): void }): void {
  win.addEventListener('keydown', (ev) => { activationGuard(ev); }, true);
}

/**
 * Register the dialog key gate: window, capture phase, right after the activation
 * guard. A blocked key stops there, so no dialog's own keydown handler, whatever
 * way it was registered, ever sees it. `vimKeysOn` is read per key.
 */
export function installDialogGate(
  win: { addEventListener(type: 'keydown', listener: (ev: KeyboardEvent) => void, capture: boolean): void; document?: { querySelector(sel: string): unknown } },
  vimKeysOn: () => boolean,
): void {
  win.addEventListener('keydown', (ev) => {
    const open = !!win.document?.querySelector('dialog[open]');
    if (dialogKeyBlocked(vimKeysOn(), ev, open)) ev.stopImmediatePropagation();
  }, true);
}
