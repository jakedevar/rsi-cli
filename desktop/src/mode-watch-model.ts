// The model-control mode watch behind the "AI work is stopped" banner (local #224).
// An Emergency stop leaves the daemon in mode `stop-all`, which refuses every new session
// and message until the mode is set back to normal; the Settings row is the only other way.
// This controller reads the mode (`model_control_overview`, the call the Settings row uses),
// says what to show, and runs the one confirmed Resume (`set_model_control_mode` normal, the
// same call as the Settings row's Apply). Pure: the DOM, the confirm dialog and the toast
// are injected. Tested in tests/mode-watch.test.mjs; the DOM side is actions/emergency.ts.
import { RESUME_CONFIRM_TITLE, bannerText, resumeConfirmBody, resumeMessage } from './emergency-model.js';
import { modeOfStatus } from './settings-state-model.js';

/** What the banner shows; null while the mode is normal or unknown. */
export interface BannerView {
  /** The TUI slug of the active mode (`stop-all`, `pause-background`, ...). */
  readonly mode: string;
  readonly text: string;
  /** A resume is running: the button is disabled. */
  readonly busy: boolean;
  /** The last resume failure as the raw message (the view renders it through renderMessage), or null. */
  readonly error: string | null;
}

export interface ModeWatchDeps {
  /** `model_control_overview`. */
  readStatus(): Promise<unknown>;
  /** `set_model_control_mode` with the TUI slug. */
  setMode(slug: string): Promise<unknown>;
  /** The plain-words confirm; true when the person agreed. */
  confirm(title: string, message: string, confirmLabel: string): Promise<boolean>;
  toast(msg: string, ok?: boolean): void;
  /** Draw (or hide, with null) the banner. */
  render(view: BannerView | null): void;
  /** After a resume: re-read what the change affects (the session list). */
  afterResume?(): Promise<void> | void;
}

export interface ModeWatch {
  /** Read the mode and redraw; a failed read leaves the banner as it was. */
  refresh(): Promise<void>;
  /** The banner button: confirm, set the mode back to normal, show the result. */
  resume(): Promise<void>;
  /** The banner as last drawn. */
  view(): BannerView | null;
}

export function createModeWatch(deps: ModeWatchDeps): ModeWatch {
  let mode: string | null = null;
  let busy = false;
  let error: string | null = null;
  let drawn: BannerView | null = null;

  const draw = (): void => {
    const text = bannerText(mode);
    drawn = mode && text ? { mode, text, busy, error } : null;
    deps.render(drawn);
  };

  async function refresh(): Promise<void> {
    if (busy) return;
    try {
      mode = modeOfStatus(await deps.readStatus());
    } catch {
      return; // a daemon that cannot answer is the connection chip's news, not a stop
    }
    if (bannerText(mode) === null) error = null;
    draw();
  }

  async function resume(): Promise<void> {
    if (busy || !mode || bannerText(mode) === null) return;
    const ok = await deps.confirm(RESUME_CONFIRM_TITLE, resumeConfirmBody(mode), 'Resume AI work');
    if (!ok) return;
    busy = true;
    error = null;
    draw();
    try {
      await deps.setMode('normal');
      busy = false;
      deps.toast(resumeMessage(), true);
    } catch (e) {
      busy = false;
      error = `Could not resume AI work: ${String(e)}`;
      deps.toast(error);
    }
    await refresh();
    if (!error) await deps.afterResume?.();
  }

  return { refresh, resume, view: () => drawn };
}
