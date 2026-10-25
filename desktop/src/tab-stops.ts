// Explicit tab stops for the macOS WebKit tab order (local #242).
//
// With macOS "Keyboard navigation" off (System Settings › Keyboard, the default), WKWebView,
// the engine Tauri uses on macOS, leaves every <button>, checkbox, radio, link and <summary>
// out of the Tab order: Tab visits only text fields, selects and elements with an explicit
// tabindex. Chrome and WebKitGTK tab to them anyway, so this went unseen in headless walks.
// An explicit tabindex="0" puts a control back in the order on every engine (the #239 WebKit
// probe confirmed it), and changes nothing where the control was already a tab stop.
//
// Rules: a control that already has a tabindex attribute keeps it (a roving-focus widget's
// -1 stays); text inputs, textareas and selects are already tab stops in WebKit and are left
// alone; no positive tabindex is ever set. Disabled and hidden controls stay out of the order
// on their own. el() (dom.ts) stamps buttons and summaries as they are made; installTabStops
// stamps the static page at boot and every control added later (rendered markdown links,
// checkboxes whose type is set after el(), anything not made through el()).

/** The controls WebKit skips without "Keyboard navigation": each gets tabindex="0". */
export const TAB_STOP_SELECTOR = 'button, a[href], input[type="checkbox"], input[type="radio"], summary';

/** The element shape the stamp needs (a real Element, or a test fake). */
export interface StampTarget {
  matches?(selector: string): boolean;
  hasAttribute?(name: string): boolean;
  setAttribute?(name: string, value: string): void;
  querySelectorAll?(selector: string): ArrayLike<StampTarget> | Iterable<StampTarget>;
}

/** Give one control tabindex="0" when it is a control WebKit skips and sets no tabindex itself. */
export function stampTabStop(node: StampTarget): boolean {
  if (!node.matches?.(TAB_STOP_SELECTOR) || node.hasAttribute?.('tabindex') !== false) return false;
  node.setAttribute?.('tabindex', '0');
  return true;
}

/** Stamp `root` and every control under it. Returns how many controls gained a tab stop. */
export function stampTabStops(root: StampTarget): number {
  let n = stampTabStop(root) ? 1 : 0;
  const all = root.querySelectorAll?.(TAB_STOP_SELECTOR);
  if (all) for (const c of Array.from(all as ArrayLike<StampTarget>)) if (stampTabStop(c)) n++;
  return n;
}

/**
 * Make a scrolling region reachable and scrollable from the keyboard (the conversation):
 * WebKit, unlike Chrome, never makes a scroll container focusable itself. A focused region
 * scrolls natively with the arrows, PageUp/PageDown and Home/End. Keeps a tabindex or an
 * aria-label the markup already sets.
 */
export function makeScrollRegion(node: StampTarget, label: string): void {
  if (node.hasAttribute?.('tabindex') === false) node.setAttribute?.('tabindex', '0');
  if (node.hasAttribute?.('aria-label') === false && node.hasAttribute?.('aria-labelledby') === false) node.setAttribute?.('aria-label', label);
}

interface MutationLike {
  readonly type: string;
  readonly target: unknown;
  readonly addedNodes: ArrayLike<unknown>;
}

/** What installTabStops observes with (window.MutationObserver; a test passes a fake). */
export type ObserverCtor = new (cb: (records: MutationLike[]) => void) => {
  observe(target: unknown, options: MutationObserverInit): void;
};

/**
 * Stamp the page now and every control added or retyped from now on (one observer, at boot).
 * Returns false when the host has no MutationObserver (then only el() and this first pass stamp).
 */
export function installTabStops(
  root: StampTarget,
  Observer: ObserverCtor | undefined = (globalThis as { MutationObserver?: ObserverCtor }).MutationObserver,
): boolean {
  stampTabStops(root);
  if (!Observer) return false;
  new Observer((records) => {
    for (const r of records) {
      if (r.type === 'attributes') { stampTabStop(r.target as StampTarget); continue; }
      for (const n of Array.from(r.addedNodes)) {
        // Element nodes only: text and comment nodes have no matches().
        if (typeof (n as StampTarget).matches !== 'function') continue;
        // A session row holds no control to stamp (its twisty is a span with role=button); thousands arrive at once (local #275).
        if ((n as { classList?: { contains(c: string): boolean } }).classList?.contains('session-item')) continue;
        stampTabStops(n as StampTarget);
      }
    }
  }).observe(root, { childList: true, subtree: true, attributes: true, attributeFilter: ['type', 'href'] });
  return true;
}
