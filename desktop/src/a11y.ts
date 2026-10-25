// Screen-reader basics (local #245): every <dialog> has an accessible name.
//
// A dialog without aria-label or aria-labelledby reads as an unnamed "dialog" to a screen
// reader, though each one has a visible heading. Dialogs are built all over the app (dialogEl,
// pickers, confirmDialog, hand-made ones), many of them filled after they are created, so the
// name is taken from the dialog's own first heading when it opens (and when it later gains one):
// the heading gets an id and the dialog an aria-labelledby that points at it. A dialog that
// already names itself (aria-label, or an aria-labelledby the code set) is left alone.

/** The element shape naming needs (a real Element, or a test fake). */
export interface DialogLike {
  id?: string;
  open?: boolean;
  matches?(selector: string): boolean;
  hasAttribute?(name: string): boolean;
  getAttribute?(name: string): string | null;
  setAttribute?(name: string, value: string): void;
  querySelector?(selector: string): (DialogLike & { textContent: string | null }) | null;
  querySelectorAll?(selector: string): ArrayLike<DialogLike> | Iterable<DialogLike>;
  closest?(selector: string): DialogLike | null;
}

const HEADING = 'h1, h2, h3, h4, [role="heading"]';
let counter = 0;

/** Marks a name this module set, so a reused dialog (same element, new content) is renamed. */
const AUTO = 'data-auto-name';

/**
 * Name `dlg` from its first non-empty heading unless the code named it. Returns true when the
 * dialog has an accessible name afterwards.
 */
export function nameDialog(dlg: DialogLike): boolean {
  const own = dlg.hasAttribute?.('aria-label') === true && (dlg.getAttribute?.('aria-label') ?? '').trim() !== '';
  if (own) return true;
  const auto = dlg.hasAttribute?.(AUTO) === true;
  if (dlg.hasAttribute?.('aria-labelledby') === true && !auto) return true;
  const headings = Array.from((dlg.querySelectorAll?.(HEADING) ?? []) as ArrayLike<DialogLike & { textContent: string | null }>);
  const head = headings.find((h) => (h.textContent ?? '').trim() !== '');
  if (!head) return false;
  if (!head.getAttribute?.('id')) head.setAttribute?.('id', `${dlg.id || 'dialog'}-title-${++counter}`);
  dlg.setAttribute?.('aria-labelledby', head.getAttribute?.('id') ?? '');
  dlg.setAttribute?.(AUTO, '');
  return true;
}

interface MutationLike {
  readonly type: string;
  readonly target: unknown;
  readonly addedNodes: ArrayLike<unknown>;
}

export type ObserverCtor = new (cb: (records: MutationLike[]) => void) => {
  observe(target: unknown, options: MutationObserverInit): void;
};

const isDialog = (n: unknown): n is DialogLike => typeof (n as DialogLike).matches === 'function' && (n as DialogLike).matches?.('dialog') === true;

/** Name every dialog under `node` (and `node` itself) that is open. */
function nameOpenDialogs(node: DialogLike): void {
  if (isDialog(node) && node.open) nameDialog(node);
  for (const d of Array.from((node.querySelectorAll?.('dialog') ?? []) as ArrayLike<DialogLike>)) if (d.open) nameDialog(d);
}

/**
 * Name dialogs as they open or gain their heading, for the life of the page (one observer, at
 * boot). Returns false when the host has no MutationObserver.
 */
export function installDialogNames(
  root: DialogLike,
  Observer: ObserverCtor | undefined = (globalThis as { MutationObserver?: ObserverCtor }).MutationObserver,
): boolean {
  nameOpenDialogs(root);
  if (!Observer) return false;
  new Observer((records) => {
    for (const r of records) {
      if (r.type === 'attributes') { if (isDialog(r.target) && r.target.open) nameDialog(r.target); continue; }
      const host = (r.target as DialogLike).closest?.('dialog');
      if (host && host.open) nameDialog(host);
      for (const n of Array.from(r.addedNodes)) if (typeof (n as DialogLike).matches === 'function') nameOpenDialogs(n as DialogLike);
    }
  }).observe(root, { childList: true, subtree: true, attributes: true, attributeFilter: ['open'] });
  return true;
}

// ---------- the conversation log ----------

/** How long the conversation stays silent after a full rebuild, so the rebuilt history is not read aloud. */
export const REBUILD_QUIET_MS = 250;

export interface LogLike {
  setAttribute(name: string, value: string): void;
  removeAttribute(name: string): void;
}

let quietTimer: ReturnType<typeof setTimeout> | undefined;

/**
 * Run `rebuild`, which replaces the whole conversation (opening or switching a session), without
 * a screen reader reading it back. #events is a polite `role="log"` that announces only
 * additions, which is right for a streaming event but would queue the entire visible history on
 * a rebuild. The region is marked busy and not live while it is rebuilt and for a short quiet
 * period after, then goes back to announcing new events.
 */
export function quietRebuild(log: LogLike, rebuild: () => void): void {
  clearTimeout(quietTimer);
  log.setAttribute('aria-busy', 'true');
  log.setAttribute('aria-live', 'off');
  try {
    rebuild();
  } finally {
    quietTimer = setTimeout(() => { log.removeAttribute('aria-busy'); log.setAttribute('aria-live', 'polite'); }, REBUILD_QUIET_MS);
  }
}
