// Operator configuration, DOM side (hub #3039, Desktop 13B): the hooks,
// custom-provider and message-bridge dialogs. Every read and write is one of the
// daemon's operator-only RPCs (the same ones the TUI uses), so both frontends see
// one provider list, one hooks block and one bridge config. A custom provider's
// API key goes to the daemon in a save and is never read back: the dialogs show
// only its state and fingerprint. The rules are in src/operator-config-model.ts
// (pure, tested); the registry entries in operatorconfig-spec.ts.
import { confirmDialog } from '../approvals.js';
import type { SliceRuntime } from '../commands.js';
import { toast } from '../dom.js';
import { operatorConfigApi as api } from '../operator-config-api.js';
import {
  BRIDGE_KINDS, type BridgeKind, type ClaudeHooksView, type CustomProvider, DEFAULT_HOOK_TIMEOUT, HOOK_EVENTS,
  type HookRow, type MessageBridge, PROVIDER_ROLES, type RoleRoute, addHook, bridgeDetail, bridgeLabel, bridgePutArgs,
  deleteProviderMessage, hookLabel, hookRows, isHooksConflict, keyStateText, providerDetail, providerPutArgs,
  removeHook, replaceHook, routeOf, routeText, supportsMatcher, validateBridgeForm, validateHookDraft, validateProviderForm,
} from '../operator-config-model.js';
import { openForm, openPicker } from '../pickers.js';
import { registerRowOverlay } from '../settings.js';
import { isVimKeysOn } from '../vimkeys.js';
import { type OperatorConfigActionId, SLICE } from './operatorconfig-spec.js';

const msg = (e: unknown): string => (e instanceof Error ? e.message : String(e));

// ---------- Claude hooks ----------

/**
 * Save `next` against the revision the dialog read. A stale revision (the file
 * changed on the daemon host) asks before overwriting, like the TUI's
 * save-conflict overlay; overwriting puts the same block again with the current
 * revision.
 */
async function saveHooks(view: ClaudeHooksView, put: (revision: string) => Promise<unknown>, done: string): Promise<void> {
  try {
    await put(view.revision);
    toast(done, true);
  } catch (e) {
    if (!isHooksConflict(e)) { toast(`Hooks not saved: ${msg(e)}`); return; }
    const overwrite = await confirmDialog(
      `${view.path || '~/.claude/settings.json'} changed since it was read. Overwrite it with your change?`, 'Overwrite',
    );
    if (!overwrite) { toast('Hooks not saved: the file changed, reopen :hooks to reload'); return; }
    try {
      await put((await api.listHooks()).revision);
      toast(done, true);
    } catch (e2) {
      toast(`Hooks not saved: ${msg(e2)}`);
    }
  }
}

/** The add form, or (with `row`) the edit form of one hook, like the TUI's hook form. */
function hookDialog(view: ClaudeHooksView, row?: HookRow): void {
  openForm({
    title: row ? `Edit the ${row.event} hook` : 'Add a Claude hook',
    submit: row ? 'Save' : 'Add',
    fields: [
      { name: 'event', label: `Event (${HOOK_EVENTS.join(', ')})`, value: row?.event ?? 'PreToolUse', type: 'text' },
      { name: 'matcher', label: 'Tool matcher (regex; PreToolUse and PostToolUse only)', value: row?.matcher ?? '', type: 'text', placeholder: 'optional, e.g. Bash' },
      { name: 'command', label: 'Shell command', value: row?.command.command ?? '', type: 'text' },
      { name: 'timeout', label: 'Timeout (seconds)', value: row?.command.timeout ? String(row.command.timeout) : '', type: 'text', placeholder: String(DEFAULT_HOOK_TIMEOUT) },
    ],
    validate: (v) => validateHookDraft({ event: v.event ?? '', matcher: v.matcher ?? '', command: v.command ?? '', timeout: v.timeout ?? '' }),
    onSubmit: async (v) => {
      const draft = { event: v.event ?? '', matcher: v.matcher ?? '', command: v.command ?? '', timeout: v.timeout ?? '' };
      const next = row ? replaceHook(view.hooks, row, draft) : addHook(view.hooks, draft);
      await saveHooks(view, (rev) => api.putHooks(rev, next), row ? 'Hook saved' : 'Hook added');
      window.setTimeout(() => void openHooks(), 0);
    },
  });
}

async function removeHookFlow(view: ClaudeHooksView, row: HookRow): Promise<void> {
  if (!(await confirmDialog(`Delete this ${row.event} hook?\n${row.command.command}`, 'Delete'))) { void openHooks(); return; }
  const next = removeHook(view.hooks, row);
  await saveHooks(view, (rev) => api.putHooks(rev, next), 'Hook deleted');
  void openHooks();
}

async function clearHooksFlow(view: ClaudeHooksView): Promise<void> {
  if (!(await confirmDialog('Remove the whole hooks block from the settings file? Other settings are kept.', 'Remove all'))) {
    void openHooks();
    return;
  }
  await saveHooks(view, (rev) => api.deleteHooks(rev), 'Hooks removed');
  void openHooks();
}

async function openHooks(opts: { add?: boolean } = {}): Promise<void> {
  let view: ClaudeHooksView;
  try { view = await api.listHooks(); } catch (e) { toast(`Hooks unavailable: ${msg(e)}`); return; }
  if (opts.add) { hookDialog(view); return; }
  const rows = hookRows(view.hooks);
  openPicker({
    title: `Claude hooks · ${view.path || '~/.claude/settings.json'}${view.exists ? '' : ' (not created yet)'}`,
    hint: 'Enter edits · Ctrl-N adds · ↑↓ select',
    emptyText: 'No hooks configured',
    vimKeys: isVimKeysOn(),
    items: rows.map((r, i) => ({ id: String(i), label: hookLabel(r), detail: `${r.command.timeout ?? DEFAULT_HOOK_TIMEOUT}s` })),
    buttons: [
      { text: 'Add', ctrl: 'n', run: () => hookDialog(view) },
      {
        text: 'Delete', ctrl: 'd', danger: true, needsItem: 'Select a hook first',
        run: (item) => { const row = item ? rows[Number(item.id)] : undefined; if (row) void removeHookFlow(view, row); },
      },
      { text: 'Remove all', ctrl: 'x', danger: true, run: () => void clearHooksFlow(view) },
    ],
    onPick: (item) => {
      const row = rows[Number(item.id)];
      if (row) hookDialog(view, row);
    },
  });
}

// ---------- custom providers ----------

function providerForm(existing: CustomProvider | null): void {
  openForm({
    title: existing ? `Edit ${existing.name}` : 'New custom provider',
    submit: 'Save',
    fields: [
      { name: 'name', label: 'Name', value: existing?.name ?? '', type: 'text' },
      { name: 'base_url', label: 'Base URL (OpenAI-compatible)', value: existing?.base_url ?? '', type: 'text', placeholder: 'https://host/v1' },
      { name: 'default_model', label: 'Default model', value: existing?.default_model ?? '', type: 'text', placeholder: 'optional' },
      {
        name: 'api_key', label: 'API key', value: '', type: 'password',
        placeholder: existing ? `${keyStateText(existing.api_key)}; leave blank to keep it` : 'optional',
      },
      ...(existing?.api_key.state === 'set'
        ? [{ name: 'clear_key', label: 'Remove the stored API key', value: 'false', type: 'checkbox' as const }]
        : []),
    ],
    validate: (v) => validateProviderForm(providerFormValues(v)),
    onSubmit: async (v) => {
      await api.putProvider(providerPutArgs(existing?.id ?? null, providerFormValues(v)));
      toast(`Saved ${(v.name ?? '').trim()}`, true);
      window.setTimeout(() => void openProviders(), 0);
    },
  });
}

const providerFormValues = (v: Record<string, string>) => ({
  name: v.name ?? '', base_url: v.base_url ?? '', default_model: v.default_model ?? '',
  api_key: v.api_key ?? '', clear_key: v.clear_key ?? 'false',
});

async function deleteProviderFlow(p: CustomProvider): Promise<void> {
  if (!(await confirmDialog(`Delete the custom provider "${p.name}"? Roles routed to it are detached and its key is removed.`, 'Delete'))) {
    void openProviders();
    return;
  }
  try {
    const r = await api.deleteProvider(p.id);
    toast(deleteProviderMessage(p.name, r.cleared_routes), true);
  } catch (e) {
    toast(`Delete failed: ${msg(e)}`);
  }
  void openProviders();
}

/** The stored routes; a failed read still lets a route be set, it just cannot show the current one. */
async function currentRoutes(): Promise<RoleRoute[]> {
  try { return await api.listRoutes(); } catch (e) { toast(`Current routes unavailable: ${msg(e)}`); return []; }
}

/** Pick the provider a role runs against (or none to detach it); the role's current route is marked. */
function routeProviderDialog(providers: readonly CustomProvider[], role: { id: string; label: string }, routes: readonly RoleRoute[]): void {
  const current = routeOf(routes, role.id);
  openPicker({
    title: `${role.label} runs against`,
    hint: `Now: ${routeText(current)} · Enter applies`,
    items: [
      { id: '', label: '(none: use the default)', current: !current },
      ...providers.map((p) => ({ id: p.id, label: p.name, detail: p.base_url, current: current?.id === p.id })),
    ],
    onPick: async (choice) => {
      try {
        await api.setRoute(role.id, choice.id || null);
        toast(choice.id ? `${role.label} now uses ${providers.find((p) => p.id === choice.id)?.name ?? choice.label}` : `${role.label} detached`, true);
      } catch (e) {
        toast(`Route failed: ${msg(e)}`);
      }
    },
  });
}

/** Pick a role (each shows its current route), then the provider it runs against (or none to detach it). */
async function routesDialog(providers: readonly CustomProvider[]): Promise<void> {
  const routes = await currentRoutes();
  openPicker({
    title: 'Route a model role to a custom provider',
    hint: 'Enter picks the role',
    items: PROVIDER_ROLES.map((r) => ({ id: r.role, label: r.label, detail: routeText(routeOf(routes, r.role)) })),
    onPick: (role) => { void currentRoutes().then((fresh) => routeProviderDialog(providers, { id: role.id, label: role.label }, fresh)); },
  });
}

/** One role's route, from its Settings row. */
async function openRoute(role: { id: string; label: string }): Promise<void> {
  let providers: CustomProvider[];
  try { providers = await api.listProviders(); } catch (e) { toast(`Providers unavailable: ${msg(e)}`); return; }
  routeProviderDialog(providers, role, await currentRoutes());
}

async function openProviders(opts: { add?: boolean } = {}): Promise<void> {
  let providers: CustomProvider[];
  try { providers = await api.listProviders(); } catch (e) { toast(`Providers unavailable: ${msg(e)}`); return; }
  if (opts.add) { providerForm(null); return; }
  openPicker({
    title: 'Custom providers',
    hint: 'Enter edits · ↑↓ select',
    emptyText: 'No custom providers yet: Ctrl-N adds one',
    filterable: true,
    items: providers.map((p) => ({ id: p.id, label: p.name, detail: providerDetail(p) })),
    buttons: [
      { text: 'New', ctrl: 'n', run: () => providerForm(null) },
      {
        text: 'Delete', ctrl: 'd', danger: true, needsItem: 'Select a provider first',
        run: (item) => { const p = providers.find((x) => x.id === item?.id); if (p) void deleteProviderFlow(p); },
      },
      { text: 'Routes', ctrl: 'r', run: () => void routesDialog(providers) },
    ],
    onPick: (item) => { const p = providers.find((x) => x.id === item.id); if (p) providerForm(p); },
  });
}

// ---------- message bridges ----------

function bridgeForm(b: MessageBridge): void {
  const c = b.config;
  openForm({
    title: `${bridgeLabel(b.kind)} bridge`,
    submit: 'Save',
    fields: [
      { name: 'enabled', label: 'Enabled', value: String(c?.enabled ?? false), type: 'checkbox' },
      ...(b.kind === 'signal'
        ? [{ name: 'account', label: 'Signal account', value: c?.account ?? '', type: 'text' as const, placeholder: '+15551234567' }]
        : []),
      { name: 'allow_from', label: 'Allowed senders (comma-separated)', value: c?.allow_from ?? '', type: 'text' },
      { name: 'working_dir', label: 'Working directory', value: c?.working_dir ?? '', type: 'text' },
    ],
    validate: (v) => validateBridgeForm(b.kind, bridgeValues(v)),
    onSubmit: async (v) => {
      await api.putBridge(bridgePutArgs(b.kind, bridgeValues(v)));
      toast(`${bridgeLabel(b.kind)} bridge saved`, true);
      window.setTimeout(() => void openBridges(), 0);
    },
  });
}

const bridgeValues = (v: Record<string, string>) => ({
  enabled: v.enabled ?? 'false', account: v.account ?? '', allow_from: v.allow_from ?? '', working_dir: v.working_dir ?? '',
});

async function deleteBridgeFlow(kind: BridgeKind): Promise<void> {
  if (!(await confirmDialog(`Remove the ${bridgeLabel(kind)} bridge config?`, 'Remove'))) { void openBridges(); return; }
  try {
    await api.deleteBridge(kind);
    toast(`${bridgeLabel(kind)} bridge config removed`, true);
  } catch (e) {
    toast(`Remove failed: ${msg(e)}`);
  }
  void openBridges();
}

/** One bridge's form, from its Settings row. */
async function openBridge(kind: BridgeKind): Promise<void> {
  try {
    const b = (await api.listBridges()).find((x) => x.kind === kind);
    bridgeForm(b ?? { kind, path: '', config: null });
  } catch (e) { toast(`Bridges unavailable: ${msg(e)}`); }
}

async function openBridges(): Promise<void> {
  let bridges: MessageBridge[];
  try { bridges = await api.listBridges(); } catch (e) { toast(`Bridges unavailable: ${msg(e)}`); return; }
  // Both kinds are always listed, configured or not.
  const all = BRIDGE_KINDS.map(({ kind }) => bridges.find((b) => b.kind === kind) ?? { kind, path: '', config: null });
  openPicker({
    title: 'Message bridges',
    hint: 'Enter edits · ↑↓ select',
    items: all.map((b) => ({ id: b.kind, label: bridgeLabel(b.kind), detail: bridgeDetail(b) })),
    buttons: [{
      text: 'Remove', ctrl: 'd', danger: true, needsItem: 'Select a bridge first',
      run: (item) => { const b = all.find((x) => x.kind === item?.id); if (b?.config) void deleteBridgeFlow(b.kind); else toast('That bridge has no config to remove'); },
    }],
    onPick: (item) => { const b = all.find((x) => x.kind === item.id); if (b) bridgeForm(b); },
  });
}

/**
 * The TUI keeps these in its Settings page, so the Settings rows open the same
 * dialogs: Enter on the row, and `a` where the TUI adds.
 */
const ROLE_ROWS: ReadonlyArray<{ row: string; role: string; label: string }> = [
  { row: 'title_model', role: 'title_model', label: 'Title model' },
  { row: 'prompt_compiler_model', role: 'prompt_compile_model', label: 'Prompt compiler model' },
  { row: 'memory_model', role: 'memory_model_fallback', label: 'Memory model (fallback)' },
  { row: 'dream_model', role: 'dream_model', label: 'Dream model' },
];

function registerSettingsRows(): void {
  registerRowOverlay('claude_hooks', {
    enter: 'Opens the hooks list (Ctrl-N adds, Enter edits, Ctrl-D deletes)',
    stored: "the daemon host's ~/.claude/settings.json, through ListClaudeHooks / PutClaudeHooks",
    keys: { a: 'Add hook' },
    run: (key) => void openHooks({ add: key === 'a' }),
  });
  registerRowOverlay('api_providers', {
    enter: 'Opens the custom providers (Ctrl-N adds, Enter edits, Ctrl-D deletes, Ctrl-R routes)',
    stored: 'the daemon store; keys in the daemon vault (write-only)',
    keys: { a: 'Add provider' },
    run: (key) => void openProviders({ add: key === 'a' }),
  });
  for (const kind of ['signal', 'imessage'] as const) {
    registerRowOverlay(`${kind}_bridge`, {
      enter: 'Opens the bridge form',
      stored: `${kind}.toml in the daemon data dir, through ListMessageBridges / PutMessageBridge`,
      run: () => void openBridge(kind),
    });
  }
  for (const r of ROLE_ROWS) {
    registerRowOverlay(r.row, {
      enter: 'Routes the role to a custom provider, or detaches it (SetCustomProviderRoute); the other model fields stay read-only here',
      stored: 'the daemon config; the key comes from the provider in the daemon vault',
      run: () => void openRoute({ id: r.role, label: r.label }),
    });
  }
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id) => {
    switch (id as OperatorConfigActionId) {
      case 'open-hooks': void openHooks(); break;
      case 'open-providers': void openProviders(); break;
      case 'open-bridges': void openBridges(); break;
    }
  },
  afterAction: (id) => { if (id === 'settings-hooks') void openHooks(); },
  init: registerSettingsRows,
  initOrder: 62,
};
