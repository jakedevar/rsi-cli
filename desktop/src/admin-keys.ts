// Provider keys and MCP servers (slice 13a; TUI Settings › Provider Keys and
// MCP Servers, crates/rsi/src/overlay/provider_credential_form.rs and
// mcp_server_form.rs). Rules in admin-model.ts; Tauri commands (with the
// credential allowlists) in src-tauri/src/admin.rs.
//
// A credential is write-only: it is typed into a password field, passed once
// to set_provider_credential / set_mcp_server_secret, and the field is cleared
// on submit and on cancel. Nothing here displays, logs or copies it; the lists
// show the state and the SHA-256 fingerprint prefix the TUI shows.
import { invoke } from './api.js';
import { el, toast } from './dom.js';
import {
  type ContractState, type CredentialList, type KeyRow, type LoginState, type McpFormState, type McpSummary,
  checkTarget, clearStep, keyRows, maskedEcho, mcpForm, mcpRequest, mcpValue, moveCursor,
} from './admin-model.js';
import {
  actions, adminDialog, button, checkField, errorLine, inField, listBox, note, rowEl, secretField, show, showError, textField,
} from './admin-dom.js';
import { emptyText } from './empty-states.js';
import { focusOwnsKey } from './modal-gate.js';
import { syncListbox } from './listbox-a11y.js';
import { loadView } from './view-load.js';

// ---------- the write-only secret form ----------

/** Ask for one credential; `send` gets it once. Esc cancels and scrubs the field. */
function secretForm(title: string, hintText: string, send: (secret: string) => Promise<void>, back: () => void): void {
  const dlg = adminDialog('admin-secret-dialog', 'admin-secret');
  const form = el('form', 'launch-form');
  const field = secretField('Credential (hidden; never shown again)');
  const echo = note();
  const err = errorLine();
  const cancel = button('Cancel (Esc)');
  const save = button('Save (Enter)', 'btn primary', 'submit');
  form.append(el('h2', undefined, title), note(hintText), field.wrap, echo, err, actions(cancel, save));
  const scrub = (): void => { field.input.value = ''; echo.textContent = ''; };
  field.input.addEventListener('input', () => { echo.textContent = maskedEcho(field.input.value.length); });
  // No copy or cut out of the field.
  field.input.addEventListener('copy', (ev) => ev.preventDefault());
  field.input.addEventListener('cut', (ev) => ev.preventDefault());
  const leave = (): void => { scrub(); dlg.close(); back(); };
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const secret = field.input.value;
    scrub();
    if (!secret) { showError(err, 'The credential is empty'); return; }
    save.disabled = true;
    send(secret).then(() => { dlg.close(); back(); }, (e: unknown) => {
      save.disabled = false;
      showError(err, `Save failed: ${String(e)}`);
    });
  });
  cancel.addEventListener('click', leave);
  dlg.onkeydown = (ev) => { if (focusOwnsKey(ev.key, ev.target as Element | null)) return; if (ev.key === 'Escape') { ev.preventDefault(); leave(); } };
  const closed = dlg.onclose;
  dlg.onclose = (e) => { scrub(); closed?.call(dlg, e); };
  show(dlg, form);
  field.input.focus();
}

// ---------- provider keys ----------

interface KeysState { list: CredentialList | null; login: LoginState | null; contract: ContractState | null; at: number; armed: { slot: string; at: number } | null }
let keys: KeysState = { list: null, login: null, contract: null, at: 0, armed: null };

interface KeysRead { list: CredentialList | null; login: LoginState | null; contract: ContractState | null; errors: string[] }

/** The three reads together. A read that fails leaves its part as `keep` has it (null when the dialog is opening) and names the error. */
async function fetchKeys(probe: boolean, keep: Pick<KeysState, 'list' | 'login' | 'contract'> | null): Promise<KeysRead> {
  const errors: string[] = [];
  const [list, login, contract] = await Promise.all([
    invoke<CredentialList>('list_provider_credentials').catch((e: unknown) => { errors.push(`keys: ${String(e)}`); return keep?.list ?? null; }),
    invoke<LoginState>('get_provider_login_state', { refresh: probe }).catch((e: unknown) => { errors.push(`login: ${String(e)}`); return keep?.login ?? null; }),
    invoke<ContractState>('get_provider_contract_state', { refresh: probe }).catch((e: unknown) => { errors.push(`contract: ${String(e)}`); return keep?.contract ?? null; }),
  ]);
  return { list, login, contract, errors };
}

async function loadKeys(probe: boolean): Promise<string[]> {
  const r = await fetchKeys(probe, keys);
  keys = { ...keys, list: r.list, login: r.login, contract: r.contract };
  return r.errors;
}

export function openProviderKeys(): Promise<void> {
  return loadView({
    key: 'admin-provider-keys', title: 'Provider keys', failed: 'Failed to load provider keys',
    read: async () => {
      const r = await fetchKeys(false, null);
      if (!r.list) throw new Error(r.errors.join('; '));
      return r;
    },
    open: (r) => { keys = { ...keys, list: r.list, login: r.login, contract: r.contract }; buildProviderKeys(r.errors); },
  });
}

function buildProviderKeys(errors: string[]): void {
  const dlg = adminDialog('admin-keys-dialog', 'admin-provider-keys');
  const box = el('div', 'launch-form');
  const head = note();
  const ul = listBox('Provider keys');
  const info = note();
  const err = errorLine();
  const setBtn = button('Set (s)');
  const rotateBtn = button('Rotate (r)');
  const checkBtn = button('Check (c)');
  const clearBtn = button('Clear (dd)', 'btn danger');
  const importBtn = button('Import from env (i)');
  const refreshBtn = button('Refresh (R)');
  const closeBtn = button('Close');
  box.append(el('h2', undefined, 'Provider keys'), head, ul, info, err,
    actions(closeBtn, refreshBtn, importBtn, checkBtn, clearBtn, rotateBtn, setBtn),
    note('j/k move, s or Enter set, r rotate, c check (on a login or contract row: probe now), d twice clears, i imports legacy env keys, R refreshes, q/Esc close. Key values are write-only.'));
  if (errors.length) showError(err, errors.join('; '));

  const rows = (): KeyRow[] => keyRows(keys.list, keys.login, keys.contract, Date.now());
  const sel = (): KeyRow | undefined => rows()[keys.at];
  const draw = (): void => {
    head.textContent = keys.list ? `env compat: ${keys.list.env_compat ? 'on' : 'off'} · check TTL: ${keys.list.check_ttl_secs}s` : '';
    const all = rows();
    ul.replaceChildren(...all.map((r, i) => {
      const li = rowEl(r.label, r.value, i === keys.at, r.set === false ? 'admin-off' : '');
      li.addEventListener('click', () => { keys = { ...keys, at: i, armed: null }; draw(); });
      li.addEventListener('dblclick', () => openSet(false));
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    syncListbox(ul, dlg);
    const r = sel();
    const slot = r?.kind === 'slot';
    for (const b2 of [setBtn, rotateBtn, clearBtn]) b2.disabled = !slot;
    info.textContent = keys.armed && r?.kind === 'slot' && keys.armed.slot === r.name ? `Clear ${r.name} credential? Press d again to confirm` : '';
  };
  const after = async (msg: string): Promise<void> => {
    toast(msg, true);
    const errs = await loadKeys(false);
    err.hidden = errs.length === 0;
    if (errs.length) showError(err, errs.join('; '));
    draw();
  };
  const openSet = (rotate: boolean): void => {
    const r = sel();
    if (r?.kind !== 'slot') return;
    const slot = r.name;
    dlg.close();
    secretForm(`${rotate ? 'Rotate' : 'Set'} ${slot} key`, `Stored in the daemon's key vault; the list shows only its fingerprint.`,
      async (secret) => {
        await invoke('set_provider_credential', { slot, secret, rotate });
        toast(`${slot} key ${rotate ? 'rotated' : 'set'}`, true);
      }, () => void openProviderKeys());
  };
  const check = (): void => {
    const t = checkTarget(sel());
    if (!t) return;
    err.hidden = true;
    invoke(t.cmd, t.args).then(() => after('Checked'), (e: unknown) => showError(err, `Check failed: ${String(e)}`));
  };
  const clear = (): void => {
    const r = sel();
    if (r?.kind !== 'slot') return;
    const step = clearStep(keys.armed, r.name, Date.now());
    keys = { ...keys, armed: step.armed };
    if (!step.run) { draw(); return; }
    invoke('clear_provider_credential', { slot: r.name }).then(() => after(`${r.name} key cleared`),
      (e: unknown) => showError(err, `Clear failed: ${String(e)}`));
  };
  const importEnv = (): void => {
    invoke<{ imported?: unknown[] }>('import_provider_credentials').then((res) => after(`Imported ${res?.imported?.length ?? 0} key(s) from the environment`),
      (e: unknown) => showError(err, `Import failed: ${String(e)}`));
  };
  const refresh = (): void => {
    void loadKeys(true).then((errs) => { err.hidden = errs.length === 0; if (errs.length) showError(err, errs.join('; ')); draw(); });
  };
  setBtn.addEventListener('click', () => openSet(false));
  rotateBtn.addEventListener('click', () => openSet(true));
  checkBtn.addEventListener('click', check);
  clearBtn.addEventListener('click', clear);
  importBtn.addEventListener('click', importEnv);
  refreshBtn.addEventListener('click', refresh);
  closeBtn.addEventListener('click', () => dlg.close());
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (inField(ev) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const moved = moveCursor(keys.at, rows().length, ev.key);
    if (moved !== null) { ev.preventDefault(); keys = { ...keys, at: moved, armed: null }; draw(); return; }
    const hit = (fn: () => void): void => { ev.preventDefault(); fn(); };
    switch (ev.key) {
      case 's': case 'Enter': hit(() => openSet(false)); break;
      case 'r': hit(() => openSet(true)); break;
      case 'c': hit(check); break;
      case 'd': hit(clear); break;
      case 'i': hit(importEnv); break;
      case 'R': hit(refresh); break;
      case 'q': case 'Escape': hit(() => dlg.close()); break;
    }
  };
  draw();
  show(dlg, box);
}

// ---------- MCP servers ----------

let mcp: { servers: McpSummary[]; at: number } = { servers: [], at: 0 };

export function openMcpServers(): Promise<void> {
  return loadView({
    key: 'admin-mcp', title: 'MCP servers', failed: 'Failed to load MCP servers',
    read: () => invoke<{ servers: McpSummary[] }>('list_mcp_servers'),
    open: (list) => { mcp = { servers: list.servers, at: Math.min(mcp.at, Math.max(0, list.servers.length - 1)) }; buildMcpServers(); },
  });
}

function buildMcpServers(): void {
  const dlg = adminDialog('admin-mcp-dialog', 'admin-mcp');
  const box = el('div', 'launch-form');
  const ul = listBox('MCP servers');
  const empty = note(emptyText('admin-mcp'));
  const err = errorLine();
  const addBtn = button('Add (a)');
  const editBtn = button('Edit (e)');
  const toggleBtn = button('Enable/disable (t)');
  const setBtn = button('Set secret (s)');
  const rotateBtn = button('Rotate secret (r)');
  const clearBtn = button('Clear secret (d)', 'btn danger');
  const refreshBtn = button('Refresh (R)');
  const closeBtn = button('Close');
  box.append(el('h2', undefined, 'MCP servers'), ul, empty, err,
    actions(closeBtn, refreshBtn, addBtn, editBtn, toggleBtn, setBtn, rotateBtn, clearBtn),
    note('j/k move, a add, Enter/e edit, t enable or disable, s set or r rotate the credential (write-only), d clear it, R refresh, q/Esc close.'));

  const sel = (): McpSummary | undefined => mcp.servers[mcp.at];
  const draw = (): void => {
    empty.hidden = mcp.servers.length > 0;
    ul.hidden = mcp.servers.length === 0;
    ul.replaceChildren(...mcp.servers.map((s, i) => {
      const li = rowEl(s.definition.id, mcpValue(s), i === mcp.at, s.definition.enabled ? '' : 'admin-off');
      li.addEventListener('click', () => { mcp = { ...mcp, at: i }; draw(); });
      li.addEventListener('dblclick', () => openForm(mcpForm(s)));
      return li;
    }));
    ul.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
    syncListbox(ul, dlg);
    for (const b2 of [editBtn, toggleBtn, setBtn, rotateBtn, clearBtn]) b2.disabled = !sel();
  };
  const reload = (): void => { dlg.close(); void openMcpServers(); };
  const call = (what: string, cmd: string, args: Record<string, unknown>): void => {
    err.hidden = true;
    invoke(cmd, args).then(() => { toast(what, true); reload(); }, (e: unknown) => showError(err, `${what} failed: ${String(e)}`));
  };
  const openForm = (f: McpFormState): void => { dlg.close(); openMcpForm(f); };
  const openSecret = (rotate: boolean): void => {
    const s = sel();
    if (!s) return;
    const id = s.definition.id;
    dlg.close();
    secretForm(`${rotate ? 'Rotate' : 'Set'} ${id} credential`, 'Passed to the server through its secret environment names; the list shows only its fingerprint.',
      async (secret) => { await invoke('set_mcp_server_secret', { id, secret, rotate }); toast(`${id} credential ${rotate ? 'rotated' : 'set'}`, true); },
      () => void openMcpServers());
  };
  const toggle = (): void => { const s = sel(); if (s) call(s.definition.enabled ? 'Disabled' : 'Enabled', 'set_mcp_server_enabled', { id: s.definition.id, enabled: !s.definition.enabled }); };
  const clear = (): void => { const s = sel(); if (s) call('Credential cleared', 'clear_mcp_server_secret', { id: s.definition.id }); };
  addBtn.addEventListener('click', () => openForm(mcpForm()));
  editBtn.addEventListener('click', () => { const s = sel(); if (s) openForm(mcpForm(s)); });
  toggleBtn.addEventListener('click', toggle);
  setBtn.addEventListener('click', () => openSecret(false));
  rotateBtn.addEventListener('click', () => openSecret(true));
  clearBtn.addEventListener('click', clear);
  refreshBtn.addEventListener('click', reload);
  closeBtn.addEventListener('click', () => dlg.close());
  dlg.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (inField(ev) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const moved = moveCursor(mcp.at, mcp.servers.length, ev.key);
    if (moved !== null) { ev.preventDefault(); mcp = { ...mcp, at: moved }; draw(); return; }
    const hit = (fn: () => void): void => { ev.preventDefault(); fn(); };
    switch (ev.key) {
      case 'a': hit(() => openForm(mcpForm())); break;
      case 'e': case 'Enter': hit(() => { const s = sel(); if (s) openForm(mcpForm(s)); }); break;
      case 't': hit(toggle); break;
      case 's': hit(() => openSecret(false)); break;
      case 'r': hit(() => openSecret(true)); break;
      case 'd': hit(clear); break;
      case 'R': hit(reload); break;
      case 'q': case 'Escape': hit(() => dlg.close()); break;
    }
  };
  draw();
  show(dlg, box);
}

function openMcpForm(f: McpFormState): void {
  const dlg = adminDialog('admin-mcp-form-dialog', 'admin-mcp');
  const form = el('form', 'launch-form');
  const id = textField('Server id (lowercase, digits, -)', f.id, { readOnly: f.editing });
  const command = textField('Command (absolute path)', f.command, { placeholder: '/usr/local/bin/my-mcp' });
  const args = textField('Arguments (comma-separated)', f.args);
  const envNames = textField('Secret environment names (comma-separated)', f.envNames, { placeholder: 'MY_MCP_TOKEN' });
  const dir = textField('Working directory (absolute, optional)', f.workingDir);
  const enabled = checkField('Enabled', f.enabled);
  const err = errorLine();
  const cancel = button('Cancel (Esc)');
  const save = button('Save (Enter)', 'btn primary', 'submit');
  form.append(el('h2', undefined, f.editing ? `Edit MCP server ${f.id}` : 'Add MCP server'), id.wrap, command.wrap, args.wrap,
    envNames.wrap, dir.wrap, enabled.wrap, err, actions(cancel, save),
    note('The definition holds no secret; set the credential from the list (s). Enter saves, Esc returns.'));
  const back = (): void => { dlg.close(); void openMcpServers(); };
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const r = mcpRequest({
      ...f, id: id.input.value, command: command.input.value, args: args.input.value, envNames: envNames.input.value,
      workingDir: dir.input.value, enabled: enabled.input.checked,
    });
    if (!r.ok) { showError(err, r.error); return; }
    invoke('upsert_mcp_server', { server: r.server }).then(() => { toast(`MCP server ${r.server.id} saved`, true); back(); },
      (e: unknown) => showError(err, `Save failed: ${String(e)}`));
  });
  cancel.addEventListener('click', back);
  dlg.onkeydown = (ev) => { if (focusOwnsKey(ev.key, ev.target as Element | null)) return; if (ev.key === 'Escape') { ev.preventDefault(); back(); } };
  show(dlg, form);
  (f.editing ? command : id).input.focus();
}
