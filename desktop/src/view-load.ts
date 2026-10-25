// The one way a view that must read from rsid before it can draw opens (local #310). While rsid is slow, a view used to
// await its reads and only then build its dialog, so choosing it drew nothing for as long as the reads took, and a refused
// read was a toast that was gone before it could be read. Now the frame opens at once, in the same turn as the choice:
//
//   - "Loading…" (role=status, aria-busy) in the place the content will fill, plus a Close button; the frame does not change size
//     when the answer comes, because the error and Retry take the same place;
//   - the reads return: the frame closes and the view's own `open(data)` builds the real dialog or pane from what was read;
//   - a read fails: the daemon's message (the same plain words and Details as a toast) replaces "Loading…", with Retry (r)
//     focused, and Close (q or Esc);
//   - the user closes the frame, or chooses another view, before the reads return: the late answer is dropped and draws nothing;
//   - the same view chosen again while it loads is the same load, not a second dialog and not a second read.
//
// A view calls `loadView` with its reads and the function that builds the rest; it never builds a "Loading…" line of its own.
import { el, renderMessage } from './dom.js';

const FRAME_ID = 'view-load-dialog';

export interface LoadViewOpts<T> {
  /** What this view is; choosing the same key again while it loads joins the load. */
  key: string;
  /** The frame's heading, which is the heading the view will use. */
  title: string;
  /** The daemon reads, run in parallel by the caller (`Promise.all`). It must not change shared state: `open` does. */
  read: () => Promise<T>;
  /** Build the view from what was read. The frame is already closed. */
  open: (data: T) => void | Promise<void>;
  /** Words before the daemon's message when a read fails, e.g. `Failed to load scheduled jobs`. */
  failed: string;
}

interface Active { key: string; promise: Promise<void> }

let active: Active | null = null;
let generation = 0;

/** The key of the view that is loading right now, or null (for the slice snapshot and tests). */
export function loadingView(): string | null {
  return active?.key ?? null;
}

function frameDialog(): HTMLDialogElement {
  let dlg = document.getElementById(FRAME_ID) as HTMLDialogElement | null;
  if (!dlg) {
    dlg = document.createElement('dialog');
    dlg.id = FRAME_ID;
    dlg.className = 'detail-dialog view-load';
    dlg.addEventListener('click', (ev) => { if (ev.target === dlg) dlg!.close(); });
    document.body.append(dlg);
  }
  dlg.onkeydown = null;
  dlg.onclose = null;
  return dlg;
}

/**
 * Open `o.key` now and fill it when `o.read` returns. Resolves once the view is built, the load failed, or it was dropped.
 */
export function loadView<T>(o: LoadViewOpts<T>): Promise<void> {
  const dlg0 = document.getElementById(FRAME_ID) as HTMLDialogElement | null;
  if (active && active.key === o.key && dlg0?.open) return active.promise;

  const mine = ++generation;
  const dlg = frameDialog();
  const heading = el('h2', undefined, o.title);
  const status = el('div', 'view-load-status', 'Loading…');
  const closeBtn = el('button', 'btn', 'Close');
  closeBtn.type = 'button';
  const retryBtn = el('button', 'btn', 'Retry (r)');
  retryBtn.type = 'button';
  retryBtn.hidden = true;
  const bar = el('div', 'actions');
  bar.append(closeBtn, retryBtn);
  const box = el('div', 'launch-form view-load-box');
  box.append(heading, status, bar);
  dlg.replaceChildren(box);

  let attempt = 0;
  let failedNow = false;
  const stale = (n: number): boolean => mine !== generation || n !== attempt || !dlg.open;

  const showLoading = (): void => {
    failedNow = false;
    retryBtn.hidden = true;
    status.className = 'view-load-status';
    status.setAttribute('role', 'status');
    status.setAttribute('aria-busy', 'true');
    status.textContent = 'Loading…';
  };
  const showFailure = (e: unknown): void => {
    failedNow = true;
    status.className = 'view-load-status form-error';
    status.setAttribute('role', 'alert');
    status.setAttribute('aria-busy', 'false');
    renderMessage(status, `${o.failed}: ${e instanceof Error ? e.message : String(e)}`);
    retryBtn.hidden = false;
    retryBtn.focus();
  };

  const finish = (): void => { if (active?.promise === promise) active = null; };
  const run = (): Promise<void> => {
    const n = ++attempt;
    showLoading();
    return (async () => o.read())().then(
      (data) => {
        if (stale(n)) return undefined;
        dlg.onclose = null;
        dlg.close();
        finish();
        return o.open(data);
      },
      (e: unknown) => { if (!stale(n)) showFailure(e); },
    );
  };

  closeBtn.addEventListener('click', () => dlg.close());
  retryBtn.addEventListener('click', () => { void run(); });
  dlg.onclose = () => { generation += 1; finish(); };
  dlg.onkeydown = (ev) => {
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (ev.key === 'q') { ev.preventDefault(); dlg.close(); }
    else if (ev.key === 'r' && failedNow) { ev.preventDefault(); void run(); }
  };

  if (!dlg.open) dlg.showModal();
  dlg.tabIndex = -1;
  dlg.focus();
  const promise: Promise<void> = run();
  active = { key: o.key, promise };
  return promise;
}
