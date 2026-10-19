// The DOM side of the admin overlays slice (actions/admin-spec.ts); the feature
// code is admin-schedules.ts, admin-keys.ts and admin-ops.ts. Collected by
// actions/runtime.generated.ts.
import { api } from '../api.js';
import type { Invocation, SliceRuntime } from '../commands.js';
import { el } from '../dom.js';
import { type DiagMetrics, diagLines, median } from '../admin-model.js';
import { ADMIN_CSS, actions, adminDialog, adminOpen, button, show } from '../admin-dom.js';
import { openMcpServers, openProviderKeys } from '../admin-keys.js';
import { openRemote, openSatellites, openScratch, openSettlement } from '../admin-ops.js';
import { openSchedules } from '../admin-schedules.js';
import { state } from '../state.js';
import { overlayKey } from './detail.js';
import { type AdminActionId, type AdminSnapshot, SLICE } from './admin-spec.js';
import { focusOwnsKey } from '../modal-gate.js';

// ---------- diagnostics ----------

/** Median frame interval over `frames` animation frames. */
function sampleFrames(frames: number): Promise<number | null> {
  return new Promise((resolve) => {
    const times: number[] = [];
    const tick = (t: number): void => {
      times.push(t);
      if (times.length > frames) resolve(median(times.slice(1).map((v, i) => v - times[i]!)));
      else requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
}

async function measure(): Promise<DiagMetrics> {
  const t0 = performance.now();
  const info = await api.daemonInfo().catch((e: unknown) => ({ connected: false, error: String(e) }));
  const roundTripMs = performance.now() - t0;
  return {
    roundTripMs,
    frameMs: await sampleFrames(30),
    sessions: state.sessions.length,
    daemon: info.connected ? 'connected' : (info as { error?: string }).error ?? 'not connected',
  };
}

/** The TUI's diagnostics overlay is read-only (q / Esc close); the desktop measures its own loop. */
async function openDiagnostics(): Promise<void> {
  const dlg = adminDialog('admin-diag-dialog', 'admin-diagnostics');
  const box = el('div', 'launch-form');
  const grid = el('div', 'admin-grid');
  const again = button('Measure again (r)');
  const close = button('Close (q)');
  box.append(el('h2', undefined, 'Diagnostics'), grid, actions(close, again),
    el('div', 'dialog-note', 'q / Esc close, r measures again, Space N new session, Space ; command palette.'));
  const draw = (m: DiagMetrics | null): void => {
    grid.replaceChildren(...(m ? diagLines(m) : [['measuring…', '']]).flatMap(([k, v]) => [el('span', 'k', k), el('span', undefined, v)]));
  };
  const run = (): void => { draw(null); void measure().then(draw); };
  again.addEventListener('click', run);
  close.addEventListener('click', () => dlg.close());
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (overlayKey(ev, dlg)) return;
    if (ev.key === 'q' || ev.key === 'Escape') { ev.preventDefault(); dlg.close(); }
    else if (ev.key === 'r') { ev.preventDefault(); run(); }
  };
  show(dlg, box);
  run();
}

// ---------- the action switch ----------

export function runAdmin(id: AdminActionId, _inv: Invocation): void {
  switch (id) {
    case 'admin-schedules': void openSchedules(); return;
    case 'admin-provider-keys': void openProviderKeys(); return;
    case 'admin-mcp': void openMcpServers(); return;
    case 'admin-diagnostics': void openDiagnostics(); return;
    case 'admin-satellites': void openSatellites(); return;
    case 'admin-remote': void openRemote(); return;
    case 'admin-scratch': void openScratch(); return;
    case 'admin-settlement': void openSettlement(); return;
  }
}

export function adminSnapshot(): AdminSnapshot {
  return { open: adminOpen() };
}

function initAdmin(): void {
  const style = document.createElement('style');
  style.id = 'admin-css';
  style.textContent = ADMIN_CSS;
  document.head.append(style);
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runAdmin(id as AdminActionId, inv),
  snapshot: () => ({ admin: adminSnapshot() }),
  init: initAdmin,
  initOrder: 60,
};
