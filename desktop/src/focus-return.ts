// Keyboard focus return (local #244). Closing a full-window view, leaving the message box with Esc,
// or pressing a button with Enter used to leave `document.activeElement === <body>`, so the next Tab
// restarted at the top bar. This is the one place that decides where focus goes instead: a view
// remembers its opener when it opens and gives focus back when it closes, falling back to the
// session list (the TUI's "back to the list"). Everything takes the document so tests can pass a fake.

/** The element a view hands focus to when nothing better is known. */
export const FOCUS_FALLBACK_ID = 'session-list';

/** The slice of an element this module touches (a real HTMLElement satisfies it). */
export interface FocusEl {
  focus(opts?: { preventScroll?: boolean }): void;
  isConnected?: boolean;
  disabled?: boolean;
  closest?(selector: string): unknown;
  tagName?: string;
}

/** The slice of a document this module touches. */
export interface FocusDoc {
  activeElement: FocusEl | null;
  body?: unknown;
  getElementById(id: string): FocusEl | null;
}

const openers = new Map<string, FocusEl>();

const doc0 = (): FocusDoc => document as unknown as FocusDoc;

/** Whether `e` can take focus now: attached, enabled and not inside a hidden view. */
function usable(e: FocusEl | null | undefined): e is FocusEl {
  if (!e || e.isConnected === false || e.disabled === true) return false;
  return !(typeof e.closest === 'function' && e.closest('[hidden]'));
}

function isPageBody(d: FocusDoc, e: FocusEl | null): boolean {
  return e == null || e === d.body || e.tagName === 'BODY' || e.tagName === 'HTML';
}

/**
 * Call when a view opens, before it moves focus into itself: remember what had focus. Nothing
 * focused (the page body) forgets any older opener so the close falls back to the list.
 */
export function rememberOpener(view: string, doc: FocusDoc = doc0()): void {
  const a = doc.activeElement;
  if (isPageBody(doc, a)) openers.delete(view);
  else openers.set(view, a as FocusEl);
}

/** Remember `opener` as where `view` hands focus back to (a view reached by Tab remembers the control focus came from). */
export function setOpener(view: string, opener: FocusEl | null | undefined): void {
  if (opener && !isPageBody(doc0(), opener)) openers.set(view, opener);
}

/** Focus the first usable of `targets` that accepts it; returns it, or null when none did. */
function focusFirst(doc: FocusDoc, targets: Array<FocusEl | null | undefined>): FocusEl | null {
  for (const t of targets) {
    if (!usable(t)) continue;
    t.focus({ preventScroll: true });
    if (doc.activeElement === t) return t;
  }
  return null;
}

/** Focus the session list (or `id`): the landing for Esc in the message box. */
export function focusList(id: string = FOCUS_FALLBACK_ID, doc: FocusDoc = doc0()): FocusEl | null {
  return focusFirst(doc, [doc.getElementById(id)]);
}

/**
 * Call when a view closes, after its container is hidden and the session layout is shown again:
 * focus goes to the remembered opener, else to the session list. Forgets the opener either way.
 */
export function returnFocus(view: string, doc: FocusDoc = doc0(), fallbackId: string = FOCUS_FALLBACK_ID): FocusEl | null {
  const opener = openers.get(view);
  openers.delete(view);
  return focusFirst(doc, [opener, doc.getElementById(fallbackId)]);
}

/** The opener remembered for `view` (tests). */
export function openerOf(view: string): FocusEl | undefined {
  return openers.get(view);
}

/**
 * Whether a button should drop focus after it is clicked. Vim keys on: always (so j/k keep working).
 * Vim keys off: only a pointer click (`detail > 0`); Enter/Space fires `click` with `detail` 0 and
 * the keyboard user keeps their place.
 */
export function shouldBlurAfterClick(detail: number, vimKeysOn: boolean): boolean {
  return vimKeysOn || detail > 0;
}

/** A card's primary button: the first enabled `.primary`, else the first enabled button. */
export function cardPrimaryButton(host: { querySelector(sel: string): FocusEl | null }): FocusEl | null {
  return host.querySelector('button.primary:not(:disabled)') ?? host.querySelector('button:not(:disabled)');
}

/**
 * Focus the first-run / daemon-down card's primary button, but only when nothing holds focus: never
 * steal it from a field the user is typing in or a control they tabbed to.
 */
export function focusCardPrimary(host: { querySelector(sel: string): FocusEl | null }, doc: FocusDoc = doc0()): FocusEl | null {
  if (!isPageBody(doc, doc.activeElement)) return null;
  return focusFirst(doc, [cardPrimaryButton(host)]);
}

type KeyedEl = FocusEl & { textContent?: string | null; className?: string; dataset?: Record<string, string | undefined> };

/** What keeps a rebuilt button "the same": its `data-focus-key` when it has one, else its label and class. */
const buttonKey = (b: KeyedEl): string => b.dataset?.focusKey ?? `${b.className ?? ''}|${b.textContent ?? ''}`;

/** A view root whose buttons a redraw replaces. */
export interface RedrawRoot {
  contains(n: unknown): boolean;
  querySelectorAll(sel: string): ArrayLike<FocusEl>;
}

/**
 * Run `rebuild` (a redraw that replaces buttons inside `root`) and, if a button in `root` had focus,
 * focus the rebuilt button with the same label: a keyboard Enter on a tab or action button must not
 * drop focus to the page because the button was recreated.
 */
export function keepFocus(root: RedrawRoot, rebuild: () => void, doc: FocusDoc = doc0()): void {
  const a = doc.activeElement as KeyedEl | null;
  const key = a && a.tagName === 'BUTTON' && root.contains(a) ? buttonKey(a) : null;
  rebuild();
  if (key === null || (doc.activeElement !== null && doc.activeElement === a && a.isConnected !== false)) return;
  if (!isPageBody(doc, doc.activeElement)) return;
  for (const b of Array.from(root.querySelectorAll('button'))) {
    if (buttonKey(b as KeyedEl) === key && focusFirst(doc, [b])) return;
  }
}

/** Whether focus is on the page itself (nothing focused, or the body): a keyboard user has lost their place. */
export function focusIsLost(doc: FocusDoc = doc0()): boolean {
  return isPageBody(doc, doc.activeElement);
}

/** Whether the focused element is `root` or inside it; call before a redraw or removal that may take it away. */
export function focusInside(root: { contains(n: unknown): boolean }, doc: FocusDoc = doc0()): boolean {
  const a = doc.activeElement;
  return !isPageBody(doc, a) && typeof root.contains === 'function' && root.contains(a);
}

/**
 * Hand focus to the first of `targets` (an element, or an id looked up now) that is attached, enabled and visible; returns
 * it, or null when none took it. The one landing for "the focused control went away": callers list the sensible places in
 * order (the next question, the message box, the conversation, the session list) instead of leaving focus on the page.
 */
export function landFocus(targets: ReadonlyArray<FocusEl | string | null | undefined>, doc: FocusDoc = doc0()): FocusEl | null {
  return focusFirst(doc, targets.map((t) => (typeof t === 'string' ? doc.getElementById(t) : t)));
}

/** Whether focus has nowhere to type: the page itself, or an element that is gone, disabled or in a hidden view. */
export function focusStranded(doc: FocusDoc = doc0()): boolean {
  const a = doc.activeElement;
  return isPageBody(doc, a) || !usable(a);
}

/**
 * Run `redraw` (it rebuilds, hides or disables controls inside `root`) and, if focus was inside `root` and the redraw
 * left it stranded, land on the first usable of `fallbacks`. For a view whose buttons come and go with the session's state
 * (Stop, Resume, Restore, Archive): the keyboard user keeps a place to Tab from.
 */
export function keepFocusIn(
  root: { contains(n: unknown): boolean }, redraw: () => void, fallbacks: ReadonlyArray<FocusEl | string | null | undefined>, doc: FocusDoc = doc0(),
): void {
  const had = focusInside(root, doc);
  redraw();
  if (had && focusStranded(doc)) landFocus(fallbacks, doc);
}
