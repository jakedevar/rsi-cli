// All text is rendered with textContent; no daemon string is ever interpreted as HTML.
// DOM helpers shared by every view module.

import { type ErrorClass, type PresentedError, presentError } from './errors-model.js';
import { focusInside, rememberOpener, returnFocus } from './focus-return.js';

export function $<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
}

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K, cls?: string, text?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  // An explicit tab stop: macOS WebKit skips buttons and summaries without one (local #242, tab-stops.ts).
  if (tag === 'button' || tag === 'summary') e.tabIndex = 0;
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

/** A button on a confirmation toast, such as Undo (local #268). `ms` is how long the toast stays; the default is 2.5 s. */
export interface ToastAction { label: string; run: () => void; ms?: number }

let toastTimer: number | undefined;
let toastListener: ((msg: string, ok: boolean, detail: string | null) => void) | undefined;

/** Hear every toast (the notification browser records them, with the raw error detail). One listener. */
export function onToast(cb: (msg: string, ok: boolean, detail: string | null) => void): void {
  toastListener = cb;
}

/** Tell the notification browser about a notice that is not a toast (the runtime-error notice, Issue #295). */
export function recordNotice(text: string, detail: string | null): void {
  toastListener?.(text, false, detail);
}

/**
 * Fill `node` with a message through the one error presentation (errors-model.ts, local #207):
 * a daemon error inside it becomes a plain sentence and next step, and the raw error goes into a
 * collapsible, copyable Details section. Returns what was shown.
 */
export function renderMessage(node: HTMLElement, msg: string): PresentedError {
  const p = presentError(msg);
  node.replaceChildren(el('span', 'err-text', p.text));
  if (p.detail !== null) node.append(errorDetails(p.detail));
  return p;
}

/** The Details section: the raw error, selectable, with a Copy button (for bug reports). */
export function errorDetails(detail: string): HTMLDetailsElement {
  const d = el('details', 'err-details');
  const copy = el('button', 'btn small err-copy', 'Copy');
  copy.type = 'button';
  copy.title = 'Copy the full error for a bug report';
  copy.addEventListener('click', (ev) => {
    ev.stopPropagation();
    void copyDetail(detail, copy);
  });
  d.append(el('summary', undefined, 'Details'), el('pre', 'err-raw', detail), copy);
  return d;
}

async function copyDetail(detail: string, btn: HTMLButtonElement): Promise<void> {
  let ok = false;
  try {
    await navigator.clipboard.writeText(detail);
    ok = true;
  } catch {
    const area = document.createElement('textarea');
    area.value = detail;
    area.setAttribute('readonly', '');
    area.style.position = 'fixed';
    area.style.opacity = '0';
    document.body.append(area);
    area.select();
    ok = document.execCommand('copy');
    area.remove();
  }
  btn.textContent = ok ? 'Copied' : 'Copy failed: select the text above';
}

/** A tooltip for a message that may hold a daemon error: the plain sentence, then the raw Details. */
export function errorTitle(msg: string): string {
  const p = presentError(msg);
  return p.detail === null ? p.text : `${p.text}\n\nDetails: ${p.detail}`;
}

/**
 * Put the toast where it is seen and heard (local #245). A modal dialog makes the rest of the
 * page inert: the backdrop covers a plain toast, and even a `popover="manual"` shown later in
 * the top layer stays inert when it is not inside the dialog [observed: elementFromPoint at its
 * centre hits the dialog; a screen reader skips inert content]. So while a modal dialog is open
 * the toast moves into the topmost one, and returns to the page when it closes. It is also
 * shown as a popover where the API exists (WebKit 17+), which lifts it above the dialog's own
 * content; without the API it relies on its fixed position and z-index.
 */
export function raiseToast(t: HTMLElement): void {
  try { t.hidePopover(); } catch { /* no popover API, or not showing */ }
  try {
    const open = document.querySelectorAll<HTMLElement>('dialog[open]');
    const dlg = open.length > 0 ? open[open.length - 1] : null;
    const holder = t.closest('dialog');
    if (dlg && holder !== dlg) {
      dlg.append(t);
      // The dialog is usually removed when it closes: take the toast back to the page first.
      dlg.addEventListener('close', () => {
        if (t.closest('dialog') !== dlg) return;
        t.hidden = true;
        document.body.append(t);
      }, { once: true });
    } else if (!dlg && holder) {
      document.body.append(t);
    }
  } catch { /* a host without these DOM calls keeps the toast where it is */ }
  t.hidden = false;
  try { t.showPopover(); } catch { /* no popover API */ }
}

export function lowerToast(t: HTMLElement, view = 'toast'): void {
  const held = focusInside(t);
  t.hidden = true;
  try { t.hidePopover(); } catch { /* not showing */ }
  // The keyboard user was on the toast's Undo (or a Details fold): back to where they were, else the session list (local #297).
  if (held) returnFocus(view);
}

export function toast(msg: string, ok = false, action?: ToastAction): void {
  const t = $('toast');
  // A toast with a button can take focus (Undo); remember where it came from, unless this toast replaces one that has it.
  const held = focusInside(t);
  if (action && !held) rememberOpener('toast');
  let detail: string | null = null;
  let text = msg;
  // A confirmation is spoken politely, an error assertively; set before the text so the change is heard.
  t.setAttribute('role', ok ? 'status' : 'alert');
  t.setAttribute('aria-live', ok ? 'polite' : 'assertive');
  if (ok) {
    t.textContent = msg;
    if (action) t.append(toastActionButton(t, action));
  } else {
    let kind: ErrorClass;
    ({ text, detail, kind } = renderMessage(t, msg));
    // The technical text is also in the console: the toast only has the plain words.
    if (kind === 'internal') console.error(msg);
  }
  toastListener?.(text, ok, detail);
  if (held) returnFocus('toast');
  t.className = ok ? 'toast ok' : 'toast';
  raiseToast(t);
  window.clearTimeout(toastTimer);
  const hide = (): void => { lowerToast(t); };
  toastTimer = window.setTimeout(hide, ok ? (action?.ms ?? 2500) : detail === null ? 6000 : 12000);
  // Opening Details keeps the toast up until it is closed again.
  t.querySelector('details')?.addEventListener('toggle', (ev) => {
    window.clearTimeout(toastTimer);
    if (!(ev.currentTarget as HTMLDetailsElement).open) toastTimer = window.setTimeout(hide, 6000);
  });
}

/** The toast's action button: a real button with a Tab stop; pressing it runs the action once and closes the toast. */
function toastActionButton(t: HTMLElement, action: ToastAction): HTMLButtonElement {
  const b = el('button', 'btn toast-action', action.label);
  b.type = 'button';
  b.addEventListener('click', () => {
    window.clearTimeout(toastTimer);
    lowerToast(t);
    action.run();
  });
  return b;
}

/** A new element showing `msg` through the error presentation (inline panels, list rows). */
export function errorBlock<K extends keyof HTMLElementTagNameMap>(tag: K, cls: string, msg: string): HTMLElementTagNameMap[K] {
  const n = el(tag, cls);
  renderMessage(n, msg);
  return n;
}

/**
 * Plain words for a message that may hold a daemon error, with the raw error kept in brackets
 * (for monospace previews and other text-only surfaces that cannot hold a Details section).
 */
export function errorText(msg: string): string {
  const p = presentError(msg);
  return p.detail === null ? p.text : `${p.text} (details: ${p.detail})`;
}

/** The button of an empty state: its label, the key that does the same, and what both run. */
export interface EmptyAction {
  readonly label: string;
  readonly key?: string;
  readonly run: () => void;
}

/**
 * An empty state that names its next step (local #208; the line comes from empty-states.ts)
 * and, when the panel has no button for it on screen, offers one that runs the same action as
 * the key: "No budget policies ... Click Add policy (or press a) to add one." and an
 * "Add policy (a)" button. With Vim keys off the key-hints pass drops "(or press a)" and "(a)".
 */
export function emptyState<K extends keyof HTMLElementTagNameMap>(tag: K, cls: string, line: string, action?: EmptyAction): HTMLElementTagNameMap[K] {
  const n = el(tag, `${cls} next-step`);
  n.dataset.keyTip = '';
  n.append(el('span', 'next-step-text', line));
  if (action) {
    const btn = el('button', 'btn small', action.key ? `${action.label} (${action.key})` : action.label);
    btn.type = 'button';
    btn.addEventListener('click', (ev) => { ev.stopPropagation(); action.run(); });
    n.append(btn);
  }
  return n;
}
