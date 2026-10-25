// Satellite registry, remote settings, legacy scratch adoption and
// source-worktree settlement (slice 13a; TUI crates/rsi/src/overlay/
// satellite_registry.rs, remote.rs, legacy_scratch.rs, cohort_settlement.rs).
// Rules in admin-model.ts; Tauri commands in src-tauri/src/admin_ops.rs.
import { invoke } from './api.js';
import { el, toast } from './dom.js';
import {
  type AdoptResult, type Cohort, type CohortAudit, type LinkFormState, type PeerFormState, type RemoteStatus, type SatProbe,
  type SatRegistry, type SatSessionPage, type SatTab, type ScratchList, type SettlementReceipt, type SettlementRunSummary,
  LINK_DIRECTIONS, adoptAll, adoptOne, adoptSummary, applyBlock, auditItemViews, countsLine, formatBytes, formatScope, linkForm, linkRequest,
  linkRow, moveCursor, nextSatTab, pageCursor, parseIdList, parseScope, peerForm, peerRequest, peerRow, activitySummary, remoteDeviceEdit,
  receiptMismatch, receiptView, remoteProjectEdit, remoteSummary, satSessionRow, scratchVerdict, settlementApply, settlementKey, toggleLinkEdit, togglePeerEdit,
} from './admin-model.js';
import {
  actions, adminDialog, button, checkField, errorLine, inField, listBox, note, rowEl, show, showError, textField,
} from './admin-dom.js';
import { overlayKey } from './actions/detail.js';
import { emptyText } from './empty-states.js';
import { focusOwnsKey, paneKeyName, tabIsNative } from './modal-gate.js';
import { loadView } from './view-load.js';
import { isVimKeysOn } from './vimkeys.js';
import { syncListbox } from './listbox-a11y.js';

const newId = (): string => crypto.randomUUID();

function grid(rows: [string, string][]): HTMLElement {
  const g = el('div', 'admin-grid');
  g.append(...rows.flatMap(([k, v]) => [el('span', 'k', k), el('span', undefined, v)]));
  return g;
}

// ---------- satellite registry ----------

interface SatState {
  registry: SatRegistry; scope: { inbound?: { kind: string; id: string; state: string }[] } | null; tab: SatTab;
  peer: number; link: number; session: number; sessions: SatSessionPage | null; probe: SatProbe | null;
}
let sat: SatState | null = null;

/** The registry and the inbound scope, read together; changes nothing until the caller assigns it. */
async function fetchSatellites(): Promise<SatState> {
  const [registry, scope] = await Promise.all([
    invoke<SatRegistry>('list_satellite_peers'),
    invoke<SatState['scope']>('get_satellite_scope_status').catch(() => null),
  ]);
  const prevId = sat?.registry.peers[sat.peer]?.config.peer_id;
  const peer = Math.max(0, registry.peers.findIndex((p) => p.config.peer_id === prevId));
  const links = registry.peers[peer]?.links.length ?? 0;
  return {
    registry, scope, tab: sat?.tab ?? 'peers', peer, link: Math.min(sat?.link ?? 0, Math.max(0, links - 1)),
    session: 0, sessions: null, probe: sat?.probe ?? null,
  };
}

async function loadSatellites(): Promise<void> {
  sat = await fetchSatellites();
}

async function loadSatSessions(offset: number): Promise<void> {
  const p = sat?.registry.peers[sat.peer];
  if (!sat || !p) return;
  const page = await invoke<SatSessionPage>('list_hub_satellite_sessions', { peerId: p.config.peer_id, offset });
  if (sat.registry.peers[sat.peer]?.config.peer_id === p.config.peer_id) sat = { ...sat, sessions: page, session: 0 };
}

export function openSatellites(): Promise<void> {
  return loadView({
    key: 'admin-satellites', title: 'Satellite registry', failed: 'Satellite registry load failed',
    read: fetchSatellites,
    open: (next) => { sat = next; buildSatellites(); },
  });
}

function buildSatellites(): void {
  const dlg = adminDialog('admin-sat-dialog', 'admin-satellites');
  const box = el('div', 'launch-form');
  const head = note();
  const tabs = el('div', 'admin-tabs');
  const tabBtns: Record<SatTab, HTMLButtonElement> = { peers: button('Peers'), links: button('Links'), sessions: button('Cached sessions') };
  tabs.append(tabBtns.peers, tabBtns.links, tabBtns.sessions);
  const ul = listBox('Satellite registry');
  const empty = note();
  const detail = note();
  const err = errorLine();
  const addBtn = button('Add (a)');
  const editBtn = button('Edit (e)');
  const toggleBtn = button('Enable/disable (d)');
  const repairBtn = button('Repair quarantine (R)');
  const probeBtn = button('Probe link (p)');
  const inboundBtn = button('Inbound policy (I)');
  const refreshBtn = button('Refresh (r)');
  const closeBtn = button('Close');
  box.append(el('h2', undefined, 'Satellite registry'), head, tabs, ul, empty, detail, err,
    actions(closeBtn, refreshBtn, inboundBtn, probeBtn, repairBtn, toggleBtn, editBtn, addBtn),
    note('Tab cycles peers, links and cached sessions; j/k select; a / e add or edit; d enables or disables; R repairs a quarantined peer; p probes a link; I edits this daemon\'s inbound policy; n / b page cached sessions; r refresh; q/Esc close. Reads only the hub daemon.'));

  const s = (): SatState => sat!;
  const peer = () => s().registry.peers[s().peer];
  const draw = (): void => {
    const st = s();
    head.textContent = `registry revision ${st.registry.revision} · ${st.registry.peers.length} peer(s)${st.scope?.inbound?.length ? ` · inbound scope: ${st.scope.inbound.map((e) => `${e.kind} ${e.id.slice(0, 8)} ${e.state}`).join(', ')}` : ''}`;
    for (const t of ['peers', 'links', 'sessions'] as SatTab[]) tabBtns[t].classList.toggle('on', st.tab === t);
    const now = Date.now();
    let rows: { label: string; value: string; off?: boolean }[] = [];
    let at = 0;
    if (st.tab === 'peers') { rows = st.registry.peers.map((p) => ({ ...peerRow(p), off: !p.config.enabled })); at = st.peer; }
    else if (st.tab === 'links') { rows = (peer()?.links ?? []).map((l) => ({ ...linkRow(l), off: !l.enabled })); at = st.link; }
    else { rows = (st.sessions?.sessions ?? []).map((x) => satSessionRow(x, now)); at = st.session; }
    empty.textContent = emptyText(st.tab === 'peers' ? 'admin-peers' : st.tab === 'links' ? (peer() ? 'admin-links' : 'admin-links-no-peer') : 'admin-sessions');
    empty.hidden = rows.length > 0;
    ul.hidden = rows.length === 0;
    ul.replaceChildren(...rows.map((r, i) => {
      const li = rowEl(r.label, r.value, i === at, r.off ? 'admin-off' : '');
      li.addEventListener('click', () => { void select(i); });
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    syncListbox(ul, dlg);
    const p = peer();
    const lines: string[] = [];
    if (st.tab === 'peers' && p) {
      lines.push(`peer ${p.config.peer_id}`, `expected installation ${p.config.expected_installation_id ?? '— (unpaired)'}`);
      if (p.dispatch_scope?.length || p.dispatch_project_scope?.length) lines.push(`dispatch scope ${formatScope(p.dispatch_scope ?? [], p.dispatch_project_scope ?? [])}`);
      lines.push(activitySummary(p.observation));
      if (p.observation) lines.push(`installation ${p.observation.installation_id ?? '—'} · incarnation ${p.observation.incarnation_id ?? '—'}`);
    }
    if (st.tab === 'sessions' && st.sessions) lines.push(`offset ${st.sessions.offset}${st.sessions.next_offset !== null ? ` · more from ${st.sessions.next_offset} (n)` : ''} · cached remote sessions have no local actions`);
    if (st.probe) lines.push(`last probe: ${st.probe.direction} trust ${st.probe.trust_reference} · installation ${String(st.probe.identity.installation_id ?? '—')} · incarnation ${String(st.probe.identity.daemon_incarnation_id ?? '—')} · ${st.probe.matches_expected === null ? 'not paired yet' : st.probe.matches_expected ? 'matches the expected installation' : 'CONTINUITY CONFLICT'}`);
    detail.textContent = lines.join('\n');
    detail.style.whiteSpace = 'pre-wrap';
    addBtn.disabled = st.tab === 'sessions' || (st.tab === 'links' && !p);
    editBtn.disabled = toggleBtn.disabled = st.tab === 'sessions' || rows.length === 0;
    repairBtn.disabled = st.tab !== 'peers' || !p;
    probeBtn.disabled = st.tab !== 'links' || rows.length === 0;
  };
  const fail = (what: string) => (e: unknown): void => { showError(err, `${what}: ${String(e)}`); draw(); };
  const refresh = (): void => { err.hidden = true; loadSatellites().then(() => loadSatSessions(0)).then(draw, fail('Registry refresh failed')); };
  const select = async (i: number): Promise<void> => {
    const st = s();
    if (st.tab === 'peers') {
      const changed = i !== st.peer;
      sat = { ...st, peer: i, ...(changed ? { link: 0, sessions: null } : {}) };
      draw();
      if (changed) await loadSatSessions(0).then(draw, fail('Cached session read failed'));
    } else if (st.tab === 'links') { sat = { ...st, link: i }; draw(); }
    else { sat = { ...st, session: i }; draw(); }
  };
  const len = (): number => {
    const st = s();
    return st.tab === 'peers' ? st.registry.peers.length : st.tab === 'links' ? peer()?.links.length ?? 0 : st.sessions?.sessions.length ?? 0;
  };
  const cur = (): number => (s().tab === 'peers' ? s().peer : s().tab === 'links' ? s().link : s().session);
  const cycleTab = (to?: SatTab): void => {
    const tab = to ?? nextSatTab(s().tab);
    sat = { ...s(), tab };
    draw();
    if (tab === 'sessions') void loadSatSessions(0).then(draw, fail('Cached session read failed'));
  };
  const toggle = (): void => {
    const st = s();
    const p = peer();
    if (!p) return;
    err.hidden = true;
    const call = st.tab === 'peers'
      ? invoke('put_satellite_peer', { edit: togglePeerEdit(p, st.registry.revision) })
      : st.tab === 'links' && p.links[st.link]
        ? invoke('put_satellite_link', { edit: toggleLinkEdit(p.config.peer_id, p.links[st.link]!, st.registry.revision) })
        : null;
    call?.then(refresh, fail('Satellite edit failed'));
  };
  const probe = (): void => {
    const p = peer();
    const l = p?.links[s().link];
    if (s().tab !== 'links' || !p || !l) return;
    invoke<SatProbe>('probe_satellite_link', { peerId: p.config.peer_id, linkId: l.link_id })
      .then((r) => { sat = { ...s(), probe: r }; err.hidden = true; draw(); }, fail('Link probe failed'));
  };
  const openForm = (mode: 'add' | 'edit' | 'repair'): void => {
    const st = s();
    const p = peer();
    if (st.tab === 'peers') {
      if (mode !== 'add' && !p) return;
      dlg.close();
      openPeerForm(peerForm(mode === 'add' ? undefined : p, newId(), mode === 'repair'), st.registry.revision);
    } else if (st.tab === 'links' && p && mode !== 'repair') {
      const l = mode === 'edit' ? p.links[st.link] : undefined;
      if (mode === 'edit' && !l) return;
      dlg.close();
      openLinkForm(linkForm(p.config.peer_id, l, newId()), st.registry.revision);
    }
  };
  const inbound = (): void => {
    invoke<{ allowed_hub_installations?: string[]; scope_roots?: string[]; scope_projects?: string[] }>('get_satellite_inbound_policy').then((pol) => {
      dlg.close();
      openInboundForm(formatScope(pol.allowed_hub_installations ?? [], []), formatScope(pol.scope_roots ?? [], pol.scope_projects ?? []));
    }, fail('Inbound policy read failed'));
  };
  const page = (dir: 1 | -1): void => {
    const pg = s().sessions;
    if (s().tab !== 'sessions' || !pg) return;
    const offset = dir === 1 ? pg.next_offset : Math.max(0, pg.offset - 30);
    if (offset === null || offset === undefined) return;
    void loadSatSessions(offset).then(draw, fail('Cached session read failed'));
  };
  for (const t of ['peers', 'links', 'sessions'] as SatTab[]) tabBtns[t].addEventListener('click', () => cycleTab(t));
  addBtn.addEventListener('click', () => openForm('add'));
  editBtn.addEventListener('click', () => openForm('edit'));
  repairBtn.addEventListener('click', () => openForm('repair'));
  toggleBtn.addEventListener('click', toggle);
  probeBtn.addEventListener('click', probe);
  inboundBtn.addEventListener('click', inbound);
  refreshBtn.addEventListener('click', refresh);
  closeBtn.addEventListener('click', () => dlg.close());
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (inField(ev) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (tabIsNative(isVimKeysOn(), ev)) return; // Vim keys off: Tab is the browser's focus order (local #248)
    if (paneKeyName(ev) === 'Tab') { ev.preventDefault(); cycleTab(); return; }
    const moved = moveCursor(cur(), len(), ev.key);
    if (moved !== null) { ev.preventDefault(); void select(moved); return; }
    const hit = (fn: () => void): void => { ev.preventDefault(); fn(); };
    switch (ev.key) {
      case 'a': hit(() => openForm('add')); break;
      case 'e': case 'Enter': hit(() => openForm('edit')); break;
      case 'd': hit(toggle); break;
      case 'R': hit(() => openForm('repair')); break;
      case 'I': hit(inbound); break;
      case 'p': hit(probe); break;
      case 'n': case 'PageDown': hit(() => page(1)); break;
      case 'b': case 'PageUp': hit(() => page(-1)); break;
      case 'r': hit(refresh); break;
      case 'q': case 'Escape': hit(() => dlg.close()); break;
    }
  };
  draw();
  show(dlg, box);
  if (sat?.tab === 'sessions' || sat?.registry.peers.length) void loadSatSessions(0).then(draw, () => {});
}

function formShell(id: string, title: string): { dlg: HTMLDialogElement; form: HTMLFormElement; err: HTMLElement; back: () => void; finish: (cancel: HTMLButtonElement, save: HTMLButtonElement, hintText: string) => void } {
  const dlg = adminDialog(id, 'admin-satellites');
  const form = el('form', 'launch-form');
  form.append(el('h2', undefined, title));
  const err = errorLine();
  const back = (): void => { dlg.close(); void openSatellites(); };
  const finish = (cancel: HTMLButtonElement, save: HTMLButtonElement, hintText: string): void => {
    form.append(err, actions(cancel, save), note(hintText));
    cancel.addEventListener('click', back);
    dlg.onkeydown = (ev) => { if (focusOwnsKey(ev.key, ev.target as Element | null)) return; if (ev.key === 'Escape') { ev.preventDefault(); back(); } };
    show(dlg, form);
    (form.querySelector('input:not([readonly])') as HTMLInputElement | null)?.focus();
  };
  return { dlg, form, err, back, finish };
}

function openPeerForm(f: PeerFormState, revision: number): void {
  const title = f.isNew ? 'Add satellite peer' : f.repairQuarantine ? `Repair quarantine: ${f.label}` : `Edit peer ${f.label}`;
  const { form, err, back, finish } = formShell('admin-peer-form-dialog', title);
  const label = textField('Label', f.label);
  const expected = textField('Expected installation id (empty: pair on first contact)', f.expectedInstallationId);
  const enabled = checkField('Enabled', f.enabled);
  const reads = checkField('Session reads', f.readEnabled);
  const dispatch = checkField('Hub-manager dispatch', f.dispatchEnabled);
  const scope = textField('Dispatch scope (session ids, project:<id>, comma-separated)', f.scope);
  const repair = checkField('Acknowledge quarantine repair (after fixing SSH trust or replacing the peer)', f.repairQuarantine);
  form.append(label.wrap, expected.wrap, enabled.wrap, reads.wrap, dispatch.wrap, scope.wrap, repair.wrap);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const r = peerRequest({
      ...f, label: label.input.value, expectedInstallationId: expected.input.value, enabled: enabled.input.checked,
      readEnabled: reads.input.checked, dispatchEnabled: dispatch.input.checked, scope: scope.input.value, repairQuarantine: repair.input.checked,
    }, revision);
    if (!r.ok) { showError(err, r.error); return; }
    // One command saves the peer and its scope fail-closed (admin_ops.rs save_peer, hub #3111):
    // a refused scope leaves the peer with access off, never its old scope newly authorised.
    const before = f.isNew ? null : parseScope(f.scope);
    const previous = before && before.ok ? { previous_sessions: before.sessions, previous_projects: before.projects } : {};
    invoke('save_satellite_peer', { save: { edit: r.edit, sessions: r.scope.sessions, projects: r.scope.projects, ...previous } })
      .then(() => { toast(`Peer ${r.edit.label} saved`, true); back(); },
        // The registry may have moved (access turned off): say what happened and reload it.
        (e: unknown) => { toast(`Satellite edit failed: ${String(e)}`); back(); });
  });
  finish(button('Cancel (Esc)'), button('Save (Enter)', 'btn primary', 'submit'), 'Tab moves, Space toggles a checkbox, Enter saves, Esc cancels.');
}

function openLinkForm(f: LinkFormState, revision: number): void {
  const { form, err, back, finish } = formShell('admin-link-form-dialog', f.isNew ? 'Add link' : 'Edit link');
  const socket = textField('Socket path (a listener on this hub)', f.socketPath);
  const ssh = textField('SSH target (optional)', f.sshTarget);
  const trust = textField('Trust reference', f.trustReference);
  const dirWrap = el('label', undefined, 'Direction');
  const dir = el('select');
  dir.append(...LINK_DIRECTIONS.map((d) => Object.assign(el('option', undefined, d.replace(/_/g, ' ')), { value: d })));
  dir.value = f.direction;
  dirWrap.append(dir);
  const enabled = checkField('Enabled', f.enabled);
  const priority = textField('Priority (0-255)', f.priority);
  form.append(socket.wrap, ssh.wrap, trust.wrap, dirWrap, enabled.wrap, priority.wrap);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const r = linkRequest({
      ...f, socketPath: socket.input.value, sshTarget: ssh.input.value, trustReference: trust.input.value, direction: dir.value,
      enabled: enabled.input.checked, priority: priority.input.value,
    }, revision);
    if (!r.ok) { showError(err, r.error); return; }
    invoke('put_satellite_link', { edit: r.edit }).then(() => { toast('Link saved', true); back(); },
      (e: unknown) => showError(err, `Satellite edit failed: ${String(e)}`));
  });
  finish(button('Cancel (Esc)'), button('Save (Enter)', 'btn primary', 'submit'), 'Tab moves, Space toggles, Enter saves, Esc cancels.');
}

function openInboundForm(hubsText: string, rootsText: string): void {
  const { form, err, back, finish } = formShell('admin-inbound-form-dialog', 'Inbound policy (this daemon as a satellite)');
  const hubs = textField('Allowed hub installation ids (comma-separated)', hubsText);
  const roots = textField('Scope roots (session ids, project:<id>, comma-separated)', rootsText);
  form.append(note('Empty lists refuse every delivery.'), hubs.wrap, roots.wrap);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const h = parseIdList(hubs.input.value);
    if (!h.ok) { showError(err, h.error); return; }
    const r = parseScope(roots.input.value);
    if (!r.ok) { showError(err, r.error); return; }
    invoke('put_satellite_inbound_policy', { hubs: h.ids, roots: r.sessions, projects: r.projects })
      .then(() => { toast('Inbound policy saved', true); back(); }, (e: unknown) => showError(err, `Inbound policy save failed: ${String(e)}`));
  });
  finish(button('Cancel (Esc)'), button('Save (Enter)', 'btn primary', 'submit'), 'Enter saves, Esc cancels.');
}

// ---------- remote settings ----------

export function openRemote(): Promise<void> {
  return loadView({
    key: 'admin-remote', title: 'Remote access', failed: 'Remote status failed',
    read: () => invoke<RemoteStatus>('remote_get_status'),
    open: buildRemote,
  });
}

function buildRemote(first: RemoteStatus): void {
  let status = first;
  let tab: 'devices' | 'projects' = 'devices';
  let at = 0;
  const dlg = adminDialog('admin-remote-dialog', 'admin-remote');
  const box = el('div', 'launch-form');
  const summary = el('div');
  const tabs = el('div', 'admin-tabs');
  const devBtn = button('Devices');
  const projBtn = button('Projects');
  tabs.append(devBtn, projBtn);
  const ul = listBox('Remote');
  const empty = note();
  const err = errorLine();
  const enableBtn = button('Enable (e)');
  const answersBtn = button('Answers (n)');
  const messagesBtn = button('Messages (m)');
  const applyBtn = button('Apply (a)');
  const refreshBtn = button('Refresh (r)');
  const closeBtn = button('Close');
  box.append(el('h2', undefined, 'Remote access'), summary, tabs, ul, empty, err,
    actions(closeBtn, refreshBtn, applyBtn, messagesBtn, answersBtn, enableBtn),
    note('Tab switches Devices and Projects; j/k select; Space / Enter allows or removes a device, exposes or hides a project (max 32); e enables or disables Remote; n answers and m messages from the phone; a re-converges the gateway; r refresh; q/Esc close.'));
  const draw = (): void => {
    summary.replaceChildren(grid(remoteSummary(status)));
    devBtn.classList.toggle('on', tab === 'devices');
    projBtn.classList.toggle('on', tab === 'projects');
    const rows = tab === 'devices'
      ? status.peers.map((p) => ({ label: p.name || p.id, value: `${p.os}  ${p.online ? 'online' : 'offline'}  ${p.allowed ? 'allowed' : '—'}`, on: p.allowed }))
      : status.projects.map((p) => ({ label: p.name || p.id, value: p.exposed ? 'exposed' : 'hidden', on: p.exposed }));
    at = Math.max(0, Math.min(at, rows.length - 1));
    empty.textContent = tab === 'devices' ? 'No tailnet devices.' : 'No projects.';
    empty.hidden = rows.length > 0;
    ul.hidden = rows.length === 0;
    ul.replaceChildren(...rows.map((r, i) => {
      const li = rowEl(`${r.on ? '●' : '○'} ${r.label}`, r.value, i === at, r.on ? '' : 'admin-off');
      li.addEventListener('click', () => { at = i; draw(); });
      li.addEventListener('dblclick', () => toggleRow());
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    syncListbox(ul, dlg);
    enableBtn.textContent = status.enabled ? 'Disable (e)' : 'Enable (e)';
    answersBtn.textContent = `Answers: ${status.allow_answers ? 'on' : 'off'} (n)`;
    messagesBtn.textContent = `Messages: ${status.allow_messages ? 'on' : 'off'} (m)`;
    applyBtn.disabled = !status.enabled;
  };
  const send = (edit: Record<string, unknown>, done: string): void => {
    err.hidden = true;
    invoke<RemoteStatus>('remote_set_config', { edit }).then((s2) => { status = s2; toast(done, true); draw(); },
      (e: unknown) => { showError(err, `Remote edit failed: ${String(e)}`); });
  };
  const toggleRow = (): void => {
    if (tab === 'devices') {
      const edit = remoteDeviceEdit(status, at);
      if (edit) send(edit, 'Devices updated');
    } else {
      const r = remoteProjectEdit(status, at);
      if (!r) return;
      if (!r.ok) { showError(err, r.error); return; }
      send(r.edit, 'Projects updated');
    }
  };
  const refresh = (): void => {
    invoke<RemoteStatus>('remote_get_status').then((s2) => { status = s2; err.hidden = true; draw(); }, (e: unknown) => showError(err, `Refresh failed: ${String(e)}`));
  };
  const setTab = (t: 'devices' | 'projects'): void => { tab = t; at = 0; draw(); };
  devBtn.addEventListener('click', () => setTab('devices'));
  projBtn.addEventListener('click', () => setTab('projects'));
  enableBtn.addEventListener('click', () => send({ enabled: !status.enabled }, status.enabled ? 'Remote disabled' : 'Remote enabled'));
  answersBtn.addEventListener('click', () => send({ allow_answers: !status.allow_answers }, 'Answers setting changed'));
  messagesBtn.addEventListener('click', () => send({ allow_messages: !status.allow_messages }, 'Messages setting changed'));
  applyBtn.addEventListener('click', () => send({}, 'Applied'));
  refreshBtn.addEventListener('click', refresh);
  closeBtn.addEventListener('click', () => dlg.close());
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (inField(ev) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (tabIsNative(isVimKeysOn(), ev)) return; // Vim keys off: Tab is the browser's focus order (local #248)
    if (paneKeyName(ev) === 'Tab') { ev.preventDefault(); setTab(tab === 'devices' ? 'projects' : 'devices'); return; }
    const len = tab === 'devices' ? status.peers.length : status.projects.length;
    const moved = moveCursor(at, len, ev.key);
    if (moved !== null) { ev.preventDefault(); at = moved; draw(); return; }
    const hit = (fn: () => void): void => { ev.preventDefault(); fn(); };
    switch (ev.key) {
      case ' ': case 'Enter': hit(toggleRow); break;
      case 'e': hit(() => enableBtn.click()); break;
      case 'n': hit(() => answersBtn.click()); break;
      case 'm': hit(() => messagesBtn.click()); break;
      case 'a': hit(() => { if (status.enabled) applyBtn.click(); }); break;
      case 'r': hit(refresh); break;
      case 'q': case 'Escape': hit(() => dlg.close()); break;
    }
  };
  draw();
  show(dlg, box);
}

// ---------- legacy scratch adoption ----------

export function openScratch(): Promise<void> {
  return loadView({
    key: 'admin-scratch', title: 'Legacy scratch adoption', failed: 'Legacy scratch list failed',
    read: () => invoke<ScratchList>('list_legacy_scratch'),
    open: buildScratch,
  });
}

function buildScratch(first: ScratchList): void {
  let list = first;
  let at = 0;
  let confirm: string[] | null = null;
  const dlg = adminDialog('admin-scratch-dialog', 'admin-scratch');
  const box = el('div', 'launch-form');
  const head = note();
  const ul = listBox('Legacy scratch directories');
  const empty = note('No unrecorded scratch directories.');
  const confirmBox = el('div');
  const err = errorLine();
  const adoptBtn = button('Adopt selected (a)');
  const allBtn = button('Adopt all adoptable (A)');
  const yesBtn = button('Adopt these (y)', 'btn danger');
  const noBtn = button('Cancel (Esc)');
  const refreshBtn = button('Refresh (r)');
  const closeBtn = button('Close');
  box.append(el('h2', undefined, 'Legacy scratch adoption'), head, ul, empty, confirmBox, err,
    actions(closeBtn, refreshBtn, allBtn, adoptBtn, noBtn, yesBtn),
    note('Scratch reclaim is enabled: an adopted directory that is old enough and unheld is deleted automatically later. a / Enter confirms the selected one, A every adoptable one (at most 32); nothing is sent until y. Esc / n / q cancels a confirmation or closes; r refreshes.'));
  const draw = (): void => {
    const c = list.candidates;
    at = Math.max(0, Math.min(at, c.length - 1));
    const adoptable = c.filter((x) => !x.blocker).length;
    head.textContent = `${c.length} unrecorded · ${adoptable} adoptable now${list.refused_roots ? ` · ${list.refused_roots} root(s) refused` : ''}${list.budget_exhausted ? ' · scan budget exhausted (partial list)' : ''}`;
    empty.hidden = c.length > 0;
    ul.hidden = c.length === 0 || confirm !== null;
    ul.replaceChildren(...c.map((x, i) => {
      const li = rowEl(x.path, `${x.kind}  ${formatBytes(x.bytes)}  ${scratchVerdict(x)}${x.detail ? `  (${x.detail})` : ''}`, i === at, x.blocker ? 'admin-off' : '');
      li.addEventListener('click', () => { at = i; draw(); });
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    syncListbox(ul, dlg);
    confirmBox.replaceChildren();
    if (confirm) {
      const listEl = listBox('Directories to adopt');
      listEl.append(...confirm.map((p) => rowEl(p, '', false)));
      confirmBox.append(el('strong', undefined, `Adopt ${confirm.length} director${confirm.length === 1 ? 'y' : 'ies'}? Reclaim may then delete them.`), listEl);
    }
    for (const b2 of [adoptBtn, allBtn, refreshBtn]) b2.hidden = confirm !== null;
    yesBtn.hidden = noBtn.hidden = confirm === null;
  };
  const ask = (r: { ok: true; paths: string[] } | { ok: false; error: string }): void => {
    if (!r.ok) { showError(err, r.error); return; }
    err.hidden = true;
    confirm = r.paths;
    draw();
  };
  const refresh = (): void => {
    invoke<ScratchList>('list_legacy_scratch').then((l) => { list = l; draw(); }, (e: unknown) => showError(err, `Refresh failed: ${String(e)}`));
  };
  const adopt = (): void => {
    if (!confirm) return;
    const paths = confirm;
    confirm = null;
    invoke<{ results?: AdoptResult[] }>('adopt_legacy_scratch', { paths }).then((res) => {
      toast(adoptSummary(res?.results ?? []), true);
      refresh();
    }, (e: unknown) => { showError(err, `Adopt failed: ${String(e)}`); draw(); });
  };
  const cancel = (): void => { confirm = null; draw(); };
  adoptBtn.addEventListener('click', () => ask(adoptOne(list.candidates[at])));
  allBtn.addEventListener('click', () => ask(adoptAll(list.candidates)));
  yesBtn.addEventListener('click', adopt);
  noBtn.addEventListener('click', cancel);
  refreshBtn.addEventListener('click', refresh);
  closeBtn.addEventListener('click', () => dlg.close());
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (inField(ev) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const hit = (fn: () => void): void => { ev.preventDefault(); fn(); };
    if (confirm) {
      if (ev.key === 'y') hit(adopt);
      else if (ev.key === 'Escape' || ev.key === 'n' || ev.key === 'q') hit(cancel);
      else if (ev.key === 'j' || ev.key === 'k') hit(() => confirmBox.querySelector('ul')?.scrollBy(0, ev.key === 'j' ? 24 : -24));
      return;
    }
    const moved = moveCursor(at, list.candidates.length, ev.key);
    if (moved !== null) { ev.preventDefault(); at = moved; draw(); return; }
    switch (ev.key) {
      case 'a': case 'Enter': hit(() => ask(adoptOne(list.candidates[at]))); break;
      case 'A': hit(() => ask(adoptAll(list.candidates))); break;
      case 'r': hit(refresh); break;
      case 'q': case 'Escape': case 'n': hit(() => dlg.close()); break;
    }
  };
  draw();
  show(dlg, box);
}

// ---------- source-worktree settlement ----------

export function openSettlement(): Promise<void> {
  return loadView({
    key: 'admin-settlement', title: 'Source-worktree settlement', failed: 'Source-worktree cohorts failed',
    read: async () => {
      const [cohorts, runs] = await Promise.all([
        invoke<Cohort[]>('list_source_worktree_cohorts'),
        invoke<SettlementRunSummary[]>('list_source_worktree_settlement_runs').catch(() => [] as SettlementRunSummary[]),
      ]);
      return { cohorts, runs };
    },
    open: ({ cohorts, runs }) => buildSettlement(cohorts, runs),
  });
}

function buildSettlement(firstCohorts: Cohort[], firstRuns: SettlementRunSummary[]): void {
  let cohorts = firstCohorts;
  let runs = firstRuns;
  let at = 0;
  let audit: CohortAudit | null = null;
  let authorizing = false;
  let key: string | null = null;
  // The full receipt of the shown run: its roots and unproven survivors (#149, #276), or why it could not be read.
  let full: SettlementReceipt | null = null;
  let fullError: { run: string; msg: string } | null = null;
  const dlg = adminDialog('admin-settle-dialog', 'admin-settlement');
  const box = el('div', 'launch-form');
  const ul = listBox('Source-worktree cohorts');
  const empty = note('No source-worktree cohorts.');
  const auditBox = el('div');
  const authBox = el('div');
  const phraseWrap = el('label', undefined, 'Type the APPLY phrase shown above exactly (nothing is prefilled)');
  const phrase = el('input');
  phrase.type = 'text';
  phrase.spellcheck = false;
  phrase.autocomplete = 'off';
  phrase.maxLength = 8192;
  phraseWrap.append(phrase);
  authBox.append(phraseWrap);
  const receipt = note();
  const detail = el('div', 'admin-receipt');
  const err = errorLine();
  const auditBtn = button('Audit (Enter)');
  const authBtn = button('Begin authorization (A)', 'btn danger');
  const applyBtn = button('Apply (Enter)', 'btn danger');
  const leaveBtn = button('Leave authorization (Esc)');
  const refreshBtn = button('Refresh receipt (r)');
  const closeBtn = button('Close');
  box.append(el('h2', undefined, 'Source-worktree settlement'), ul, empty, auditBox, authBox, receipt, detail, err,
    actions(closeBtn, refreshBtn, auditBtn, authBtn, leaveBtn, applyBtn),
    note('j/k (J/K page) select; Enter audits (read-only); A begins authorization after a fresh applyable audit; r refreshes the receipt; Esc / q close; Space N new session, Space ; palette.'));
  const sel = (): Cohort | undefined => cohorts[at];
  const draw = (): void => {
    empty.hidden = cohorts.length > 0;
    ul.hidden = cohorts.length === 0;
    ul.replaceChildren(...cohorts.map((c, i) => {
      const li = rowEl(c.repository_identity, `${c.canonical_repo_dir}  live ${c.live_roots}  terminal ${c.terminal_roots}`, i === at);
      li.addEventListener('click', () => move(i));
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    syncListbox(ul, dlg);
    auditBox.replaceChildren();
    if (audit) {
      const rows: [string, string][] = [
        ['audited', audit.repository_identity], ['target', audit.target_ref ? `${audit.target_ref} ${audit.target_oid ?? ''}` : '—'],
        ['counts', countsLine(audit.counts)], ['applyable', audit.applyable ? 'yes' : 'no'],
      ];
      if (audit.refusal) rows.push(['refusal', audit.refusal]);
      auditBox.append(grid(rows));
      const roots = auditItemViews(audit);
      if (roots.length) {
        auditBox.append(el('h3', undefined, 'Roots in this check'));
        for (const r of roots) {
          const row = el('div', `admin-item ${r.tone}`);
          row.append(el('div', 'admin-item-title', r.title), el('div', undefined, r.verdict));
          if (r.detail) row.append(el('div', 'admin-detail', r.detail));
          auditBox.append(row);
        }
      }
      if (audit.authorization_phrase) {
        const ph = el('div', 'admin-phrase', audit.authorization_phrase);
        auditBox.append(note('Authorization phrase:'), ph);
      }
    }
    const last = runs.find((r) => r.repository_identity === sel()?.repository_identity) ?? runs[0];
    const shown = !!last && full?.run_id === last.run_id;
    // The one-line summary stands in until the full receipt has loaded (or if it cannot).
    receipt.hidden = shown;
    receipt.textContent = last ? `Latest run ${last.run_id} (${last.repository_identity}): ${last.state} · ${countsLine(last.counts)} · ${last.created_at}` : 'No settlement run yet.';
    const failed = last && fullError?.run === last.run_id ? fullError.msg : null;
    detail.replaceChildren(...(shown && full ? receiptBlock(last.run_id, full) : failed ? [note(failed)] : []));
    if (last && !shown && !failed) loadReceipt(last.run_id);
    authBox.hidden = !authorizing;
    auditBtn.hidden = authBtn.hidden = authorizing;
    applyBtn.hidden = leaveBtn.hidden = !authorizing;
    auditBtn.disabled = !sel();
  };
  /** The receipt block: the run, each root with its reason, then the programs settlement left running. */
  const receiptBlock = (runId: string, r: SettlementReceipt): HTMLElement[] => {
    const v = receiptView(r);
    const out: HTMLElement[] = [el('h3', undefined, `Latest run ${runId}`), grid(v.facts)];
    if (v.error) out.push(el('div', 'admin-warn', v.error));
    out.push(el('h3', undefined, 'Roots'));
    if (v.itemsEmpty) out.push(note(v.itemsEmpty));
    for (const i of v.items) {
      const row = el('div', `admin-item ${i.tone}`);
      row.append(el('div', 'admin-item-title', i.title), el('div', undefined, i.status));
      if (i.reason) row.append(el('div', 'admin-warn', `Reason: ${i.reason}`));
      if (i.detail) row.append(el('div', 'admin-detail', i.detail));
      if (i.hint) row.append(note(i.hint));
      out.push(row);
    }
    if (v.survivors.length) {
      out.push(el('h3', undefined, v.survivorsTitle), note(v.survivorsNote), ...v.survivorMeanings.map((m) => note(`What this means: ${m}`)),
        ...v.survivors.map((line) => el('div', 'admin-warn', line)));
    }
    return out;
  };
  let loading: string | null = null;
  const loadReceipt = (runId: string): void => {
    if (loading === runId) return;
    loading = runId;
    invoke<SettlementReceipt>('get_source_worktree_settlement_run', { runId }).then((r) => {
      if (loading !== runId) return;
      const c = sel();
      const bad = c ? receiptMismatch(r, runId, c.repository_identity, c.canonical_repo_dir) : null;
      if (bad) fullError = { run: runId, msg: `Run details not shown: ${bad}` };
      else full = r;
      draw();
    }, (e: unknown) => {
      if (loading !== runId) return;
      fullError = { run: runId, msg: `Run details could not be read: ${String(e)}` };
      draw();
    });
  };
  const move = (i: number): void => {
    if (i !== at) { audit = null; authorizing = false; phrase.value = ''; key = null; }
    at = i;
    draw();
  };
  const runAudit = (): void => {
    const c = sel();
    if (!c) return;
    err.hidden = true;
    invoke<CohortAudit>('audit_source_worktree_cohort', { repositoryIdentity: c.repository_identity }).then((a) => {
      if (sel()?.repository_identity !== c.repository_identity) return;
      if (a.repository_identity !== c.repository_identity || a.canonical_repo_dir !== c.canonical_repo_dir) {
        showError(err, 'The audit does not match the selected cohort; refresh and audit again');
        return;
      }
      audit = a;
      draw();
    }, (e: unknown) => showError(err, `Audit failed: ${String(e)}`));
  };
  const beginAuth = (): void => {
    const why = applyBlock(audit, sel()?.repository_identity);
    if (why) { showError(err, why); return; }
    err.hidden = true;
    authorizing = true;
    phrase.value = '';
    key = settlementKey(newId());
    draw();
    phrase.focus();
  };
  const leaveAuth = (): void => { authorizing = false; phrase.value = ''; draw(); dlg.focus(); };
  const apply = (): void => {
    if (!audit || !key) return;
    const r = settlementApply(audit, phrase.value, key);
    if (!r.ok) { showError(err, r.error); return; }
    applyBtn.disabled = true;
    invoke<{ run_id?: string }>('apply_source_worktree_cohort', { apply: r.apply }).then(async (run) => {
      applyBtn.disabled = false;
      authorizing = false;
      phrase.value = '';
      key = null;
      audit = null;
      toast(`Settlement applied${run?.run_id ? ` (run ${run.run_id})` : ''}`, true);
      runs = await invoke<SettlementRunSummary[]>('list_source_worktree_settlement_runs').catch(() => runs);
      cohorts = await invoke<Cohort[]>('list_source_worktree_cohorts').catch(() => cohorts);
      draw();
    }, (e: unknown) => {
      // The same key is kept, so a retry of an uncertain failure is the identical request.
      applyBtn.disabled = false;
      showError(err, `Apply failed: ${String(e)}`);
    });
  };
  const refresh = (): void => {
    invoke<SettlementRunSummary[]>('list_source_worktree_settlement_runs').then((r) => { runs = r; err.hidden = true; full = null; fullError = null; loading = null; draw(); },
      (e: unknown) => showError(err, `Receipt refresh failed: ${String(e)}`));
  };
  auditBtn.addEventListener('click', runAudit);
  authBtn.addEventListener('click', beginAuth);
  applyBtn.addEventListener('click', apply);
  leaveBtn.addEventListener('click', leaveAuth);
  refreshBtn.addEventListener('click', refresh);
  closeBtn.addEventListener('click', () => dlg.close());
  phrase.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Enter') { ev.preventDefault(); apply(); }
    else if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); leaveAuth(); }
  });
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (authorizing) { if (ev.key === 'Escape') { ev.preventDefault(); leaveAuth(); } return; }
    if (inField(ev)) return;
    if (overlayKey(ev, dlg) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const moved = pageCursor(at, cohorts.length, ev.key);
    if (moved !== null) { ev.preventDefault(); move(moved); return; }
    const hit = (fn: () => void): void => { ev.preventDefault(); fn(); };
    switch (ev.key) {
      case 'Enter': hit(runAudit); break;
      case 'A': hit(beginAuth); break;
      case 'r': hit(refresh); break;
      case 'q': case 'Escape': hit(() => dlg.close()); break;
    }
  };
  draw();
  show(dlg, box);
}
