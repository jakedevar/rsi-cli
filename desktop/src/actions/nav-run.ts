// The DOM side of the navigation actions (actions/nav.ts) and of the pane,
// window and scroll keys the TUI inherits from modalkit. It also claims two
// core actions where the TUI gives their keys a pane meaning: gg / G jump the
// transcript while it has the keys, and the Active / Archived views leave the
// Jobs zone. i / a / o / O and gf1..gf9 are G1's (actions/input.ts). The rules are
// nav-model.ts. Daemon text is set with textContent only.
import { invoke } from '../api.js';
import { type Invocation, type SliceRuntime, copyText, dispatch, snapshot } from '../commands.js';
import { $, el, toast } from '../dom.js';
import { openPathInViewer } from '../files.js';
import { isTypingTarget } from '../keyengine.js';
import { ascendTarget } from '../listmodel.js';
import { modelDropdownOpen, openModelDropdown, providers } from '../model-dropdown.js';
import {
  type LaunchDefault, type ListZone, backStep, closeTarget, isWebUrl, looksLikePath,
  modelArg, parseLaunchDefault, resolvePath, routeNavKey, scrollBy, stepProject,
  terminalCommand, WINDOW_CHORD_MS, zoneLabel,
} from '../nav-model.js';
import { type ActionId, tooltipText } from '../registry.js';
import { renderHeader, renderSessionList, select, setView, visibleRows } from '../sessions.js';
import { isReadOnly, selected, state } from '../state.js';
import { SWEEP_CSS, injectCss } from '../sweep-css.js';
import { currentPlatform, isVimKeysOn } from '../vimkeys.js';
import { detailSnapshot, jumpTranscript, setDetailFocus } from './detail.js';
import { type NavActionId, type NavSnapshot, SLICE } from './nav.js';
import { runInTerminal } from './terminal.js';
import { focusOwnsKey } from '../modal-gate.js';

const LAUNCH_DEFAULT_KEY = 'rsi.desktop.launchDefault';

const errText = (e: unknown): string => (e instanceof Error ? e.message : String(e));

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

function cursor(): string | null {
  return state.cursorId ?? state.selectedId;
}

function focus(): 'list' | 'transcript' {
  return detailSnapshot().focus;
}

export function navSnapshot(): NavSnapshot {
  const open = selected() !== undefined;
  return {
    zone: state.zone,
    back: backStep(focus(), ascendTarget(visibleRows(), cursor()) !== null, open),
    projects: state.projects.length,
  };
}

// ---------- zones ----------

function syncZoneButtons(): void {
  const jobs = state.view === 'active' && state.zone === 'jobs';
  const active = state.view === 'active' && !jobs;
  const set = (id: string, on: boolean): void => {
    const b = document.getElementById(id);
    if (!b) return;
    b.classList.toggle('active', on);
    b.setAttribute('aria-pressed', String(on));
  };
  set('view-active', active);
  set('view-jobs', jobs);
}

async function showJobs(): Promise<void> {
  if (state.view === 'archived') await setView('active');
  state.zone = 'jobs';
  renderSessionList();
  syncZoneButtons();
  toast(zoneLabel('jobs'), true);
}

// ---------- panes ----------

function closeSession(): void {
  state.selectedId = null;
  state.selectionEpoch += 1;
  state.events = [];
  $('events').replaceChildren();
  renderSessionList();
  renderHeader();
}

function closePane(): void {
  const t = closeTarget(snapshot());
  if (!t) return;
  if (t === 'session') { closeSession(); return; }
  dispatch(t, { source: 'key' });
}

function back(): void {
  const step = backStep(focus(), ascendTarget(visibleRows(), cursor()) !== null, selected() !== undefined);
  if (step === 'to-list') {
    ($('message') as HTMLTextAreaElement).blur();
    setDetailFocus('list');
    toast('Keys back on the session list', true);
  } else if (step === 'ascend') {
    dispatch('ascend', { source: 'key' });
  }
}

function sessionNormal(): void {
  const c = cursor();
  if (c && c !== state.selectedId) select(c);
  ($('message') as HTMLTextAreaElement).blur();
  setDetailFocus('transcript');
}

function stepTab(dir: 1 | -1): void {
  const sel = $<HTMLSelectElement>('project-filter');
  const next = stepProject(state.projects.map((p) => p.id), sel.value, dir);
  sel.value = next;
  renderSessionList();
  toast(next ? `Project: ${state.projects.find((p) => p.id === next)?.name ?? next}` : 'All projects', true);
}

function isWindows(): boolean {
  return /win/i.test(navigator.platform || navigator.userAgent) && !/darwin|mac/i.test(navigator.platform || navigator.userAgent);
}

// ---------- the default launch model (Ctrl-M, :model) ----------

function launchDefault(): LaunchDefault | null {
  return parseLaunchDefault(storage()?.getItem(LAUNCH_DEFAULT_KEY));
}

/** Prefill the New session dialog: `defaultValue` survives its form.reset() after a launch. */
function applyLaunchDefault(): void {
  const d = launchDefault();
  const form = document.getElementById('launch-form') as HTMLFormElement | null;
  if (!d || !form) return;
  const model = form.elements.namedItem('model') as HTMLInputElement | null;
  if (model && model.value === model.defaultValue) { model.defaultValue = d.model; model.value = d.model; }
  const prov = form.elements.namedItem('provider') as HTMLSelectElement | null;
  for (const o of prov?.options ?? []) o.defaultSelected = o.value === d.provider;
  if (prov && [...prov.options].some((o) => o.value === d.provider)) prov.value = d.provider;
}

function setLaunchDefault(d: LaunchDefault): void {
  try { storage()?.setItem(LAUNCH_DEFAULT_KEY, JSON.stringify(d)); } catch { /* storage unavailable */ }
  const form = document.getElementById('launch-form') as HTMLFormElement | null;
  const model = form?.elements.namedItem('model') as HTMLInputElement | null;
  if (model) { model.defaultValue = ''; model.value = ''; }
  applyLaunchDefault();
  toast(`New sessions start on ${d.model || 'the provider default'} (${d.provider})`, true);
}

/** Ctrl-M / :model: the shared model dropdown (model-dropdown.ts) sets the launch default. */
async function modelDropdown(inv: Invocation): Promise<void> {
  const list = await providers();
  const cur = launchDefault();
  const direct = modelArg(inv.arg);
  if (direct) { setLaunchDefault({ provider: cur?.provider ?? list[0] ?? 'Claude', model: direct }); return; }
  if (modelDropdownOpen()) return;
  openModelDropdown({
    title: 'Default model for new sessions',
    provider: cur?.provider && list.includes(cur.provider) ? cur.provider : (list[0] ?? 'Claude'),
    ...(cur ? { current: { provider: cur.provider, model: cur.model } } : {}),
    withDefault: true,
    onPick: (provider, m) => setLaunchDefault({ provider, model: m.id }),
  });
}

// ---------- keys ----------

let windowArmedAt = 0;

/** Something other than the list and the transcript has the screen (an overlay pane). */
function overlayUp(): boolean {
  const t = closeTarget(snapshot());
  return t !== null && t !== 'session';
}

function scrollPane(amount: 'half' | 'page', dir: 1 | -1): void {
  const pane = focus() === 'transcript' && selected() ? $('events') : $('session-list');
  pane.scrollBy({ top: dir * scrollBy(amount, pane.clientHeight) });
}

function onKey(ev: KeyboardEvent): void {
  if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
  if (ev.defaultPrevented || ev.isComposing || !isVimKeysOn()) return;
  if (document.querySelector('dialog[open]') !== null) return;
  const t = document.activeElement as HTMLElement | null;
  if (isTypingTarget(t)) return;
  if (t && t !== document.body && !t.closest('.layout')) return;
  if (overlayUp()) return;
  const armed = Date.now() - windowArmedAt < WINDOW_CHORD_MS;
  const r = routeNavKey(ev, armed);
  windowArmedAt = 0;
  if (r.kind === 'pass') return;
  ev.preventDefault();
  ev.stopImmediatePropagation();
  switch (r.kind) {
    case 'arm-window': windowArmedAt = Date.now(); return;
    case 'scroll': scrollPane(r.amount, r.dir); return;
    case 'focus': {
      const s = selected();
      if (r.to === 'list') { setDetailFocus('list'); return; }
      if (!s) { toast('Open a session first (j/k, then Enter)'); return; }
      if (r.to === 'transcript') { setDetailFocus('transcript'); return; }
      if (isReadOnly(s)) { toast('This session is read-only'); return; }
      $('message').focus();
    }
  }
}

// ---------- mouse: links and file paths in the transcript ----------

async function openUrl(url: string): Promise<void> {
  try {
    await invoke<unknown>('open_url', { url });
    toast(`Opened ${url}`, true);
  } catch (e) {
    toast(`Could not open the link: ${errText(e)}`);
  }
}

/** Inline code (not a code block) that names a file. */
function pathOf(target: EventTarget | null): string | null {
  const code = (target as HTMLElement | null)?.closest?.<HTMLElement>('.md code');
  if (!code || code.closest('pre')) return null;
  const text = code.textContent ?? '';
  return looksLikePath(text) ? text.trim() : null;
}

function wireMouse(): void {
  const events = $('events');
  // Right-click: a web link opens in the browser, a file path in the viewer.
  events.addEventListener('contextmenu', (ev) => {
    const link = (ev.target as HTMLElement | null)?.closest?.<HTMLAnchorElement>('.md a');
    const href = link?.getAttribute('href') ?? '';
    if (link && isWebUrl(href)) { ev.preventDefault(); ev.stopPropagation(); void openUrl(href); return; }
    const path = pathOf(ev.target);
    if (path) { ev.preventDefault(); ev.stopPropagation(); openPathInViewer(path); }
  }, true);
  // Left-click a file path: copy the resolved path (the event cursor still moves).
  events.addEventListener('click', (ev) => {
    const path = pathOf(ev.target);
    if (path) void copyText(resolvePath(selected()?.working_dir, path), 'file path');
  }, true);
}

// ---------- claims on core actions ----------

function claim(id: ActionId, inv: Invocation): ActionId | null {
  switch (id) {
    case 'cursor-first':
    case 'cursor-last':
      if (inv.source !== 'key' || !selected() || focus() !== 'transcript') return null;
      jumpTranscript(id === 'cursor-first' ? 'first' : 'last');
      // A transcript action, so the keys stay in the transcript.
      return 'event-next';
    case 'view-active':
    case 'view-archived':
      if (state.zone !== 'main') {
        state.zone = 'main';
        if (id === 'view-active' && state.view === 'active') renderSessionList();
      }
      return null;
    default:
      return null;
  }
}

// ---------- dispatch ----------

export function runNav(id: NavActionId, inv: Invocation): void {
  switch (id) {
    case 'view-jobs': void showJobs(); return;
    case 'ascend-or-back': back(); return;
    case 'close-pane': closePane(); return;
    case 'next-tab': stepTab(1); return;
    case 'prev-tab': stepTab(-1); return;
    case 'session-normal': sessionNormal(); return;
    case 'model-dropdown': void modelDropdown(inv); return;
    case 'git-panel': void runInTerminal(terminalCommand('lazygit', isWindows())); return;
    case 'system-monitor': void runInTerminal(terminalCommand('btop', isWindows())); return;
  }
}

function initNav(): void {
  injectCss('sweep-css', SWEEP_CSS);
  // The Jobs zone's segment beside Active and Archived.
  const active = document.getElementById('view-active');
  if (active && !document.getElementById('view-jobs')) {
    const jobs = el('button', 'seg', 'Jobs');
    jobs.id = 'view-jobs';
    jobs.type = 'button';
    jobs.dataset.action = 'view-jobs';
    jobs.setAttribute('aria-pressed', 'false');
    jobs.title = tooltipText('view-jobs', currentPlatform());
    active.after(jobs);
  }
  const launch = document.getElementById('launch-dialog');
  if (launch) {
    new MutationObserver(() => { if (launch.hasAttribute('open')) applyLaunchDefault(); })
      .observe(launch, { attributes: true, attributeFilter: ['open'] });
  }
  // Capture on the window: ahead of the vim engine, so Ctrl-w's h / j / k / l
  // is not read as an unbound Normal-mode key.
  window.addEventListener('keydown', onKey, true);
  wireMouse();
}

/** The navigation slice's DOM side, collected by actions/runtime.generated.ts. */
export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runNav(id as NavActionId, inv),
  snapshot: () => ({ nav: navSnapshot() }),
  afterAction: () => syncZoneButtons(),
  claim,
  init: initNav,
  initOrder: 900,
};
