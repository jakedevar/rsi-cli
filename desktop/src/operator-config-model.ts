// Pure rules for the operator configuration dialogs (hub #3039, Desktop 13B):
// Claude hooks, custom providers and message bridges. The data lives in the
// daemon (operator-only RPCs shared with the TUI); this file only reads the
// views, edits the hooks block and builds/validates form input. Tested in
// tests/operatorconfig.test.mjs; the DOM side is actions/operatorconfig.ts.

/** One hook command (`HookCommand`): `type`, the shell `command`, a timeout in seconds. */
export interface HookCommand { type: string; command: string; timeout?: number }
/** One bucket under an event (`HookEntry`); `matcher` only on Pre/PostToolUse. */
export interface HookEntry { matcher?: string; hooks: HookCommand[] }
export type HooksBlock = Record<string, HookEntry[]>;

/** `ListClaudeHooks` / `PutClaudeHooks` / `DeleteClaudeHooks` result. */
export interface ClaudeHooksView { path: string; exists: boolean; revision: string; hooks: HooksBlock }

/** The events the TUI cycles through (`KnownHookEvent::ALL`). */
export const HOOK_EVENTS: readonly string[] = [
  'PreToolUse', 'PostToolUse', 'UserPromptSubmit', 'SessionStart', 'SessionEnd', 'Stop', 'SubagentStop',
  'Notification', 'PreCompact',
];
const MATCHER_EVENTS: ReadonlySet<string> = new Set(['PreToolUse', 'PostToolUse']);
export const DEFAULT_HOOK_TIMEOUT = 60;

export const supportsMatcher = (event: string): boolean => MATCHER_EVENTS.has(event);

const isObj = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);

/** Read a `ClaudeHooksView`; null when the daemon sent something else. */
export function parseHooksView(raw: unknown): ClaudeHooksView | null {
  if (!isObj(raw) || typeof raw.revision !== 'string' || !isObj(raw.hooks)) return null;
  const hooks: HooksBlock = {};
  for (const [event, entries] of Object.entries(raw.hooks)) {
    if (!Array.isArray(entries)) continue;
    hooks[event] = entries.filter(isObj).map((e) => ({
      ...(typeof e.matcher === 'string' ? { matcher: e.matcher } : {}),
      hooks: (Array.isArray(e.hooks) ? e.hooks : []).filter(isObj).map((h) => ({
        type: typeof h.type === 'string' ? h.type : 'command',
        command: typeof h.command === 'string' ? h.command : '',
        ...(typeof h.timeout === 'number' ? { timeout: h.timeout } : {}),
      })),
    }));
  }
  return {
    path: typeof raw.path === 'string' ? raw.path : '',
    exists: raw.exists === true,
    revision: raw.revision,
    hooks,
  };
}

/** One row of the hooks list: where the command sits in the block. */
export interface HookRow { event: string; entry: number; index: number; matcher: string; command: HookCommand }

/** The hook commands in file order (events sorted, as the daemon's map is). */
export function hookRows(hooks: HooksBlock): HookRow[] {
  const rows: HookRow[] = [];
  for (const event of Object.keys(hooks).sort()) {
    (hooks[event] ?? []).forEach((entry, e) => {
      entry.hooks.forEach((command, i) => rows.push({ event, entry: e, index: i, matcher: entry.matcher ?? '', command }));
    });
  }
  return rows;
}

export function hookLabel(r: HookRow): string {
  const matcher = r.matcher ? ` [${r.matcher}]` : '';
  const kind = r.command.type === 'command' ? '' : `${r.command.type}: `;
  return `${r.event}${matcher}  ${kind}${r.command.command}`;
}

export interface HookDraft { event: string; matcher: string; command: string; timeout: string }

/** Null when the draft can be added. */
export function validateHookDraft(d: HookDraft): string | null {
  if (!d.event.trim()) return 'Pick an event';
  if (!d.command.trim()) return 'The command is empty';
  const t = d.timeout.trim();
  if (t && !/^\d+$/.test(t)) return 'The timeout is whole seconds';
  if (t && Number(t) < 1) return 'The timeout is at least 1 second';
  return null;
}

/** The block with one command added; the first entry with the same matcher takes it. Input is not changed. */
export function addHook(hooks: HooksBlock, d: HookDraft): HooksBlock {
  const event = d.event.trim();
  const matcher = supportsMatcher(event) ? d.matcher.trim() : '';
  const timeout = d.timeout.trim() ? Number(d.timeout.trim()) : DEFAULT_HOOK_TIMEOUT;
  const command: HookCommand = { type: 'command', command: d.command.trim(), ...(timeout === DEFAULT_HOOK_TIMEOUT ? {} : { timeout }) };
  const entries = (hooks[event] ?? []).map((e) => ({ ...e, hooks: [...e.hooks] }));
  const at = entries.findIndex((e) => (e.matcher ?? '') === matcher);
  if (at >= 0) entries[at]?.hooks.push(command);
  else entries.push({ ...(matcher ? { matcher } : {}), hooks: [command] });
  return { ...hooks, [event]: entries };
}

/** The block without one command; an entry or event left empty goes too. Input is not changed. */
export function removeHook(hooks: HooksBlock, row: Pick<HookRow, 'event' | 'entry' | 'index'>): HooksBlock {
  const entries = (hooks[row.event] ?? []).map((e) => ({ ...e, hooks: [...e.hooks] }));
  entries[row.entry]?.hooks.splice(row.index, 1);
  const kept = entries.filter((e) => e.hooks.length > 0);
  const { [row.event]: _dropped, ...rest } = hooks;
  return kept.length > 0 ? { ...rest, [row.event]: kept } : rest;
}

/**
 * The block with one command changed to `d` (the TUI's HookEdit edit): in place
 * when the event and matcher stay, otherwise removed and added where the new
 * event and matcher belong. Input is not changed.
 */
export function replaceHook(hooks: HooksBlock, row: Pick<HookRow, 'event' | 'entry' | 'index'>, d: HookDraft): HooksBlock {
  const event = d.event.trim();
  const matcher = supportsMatcher(event) ? d.matcher.trim() : '';
  const old = hooks[row.event]?.[row.entry];
  if (old && event === row.event && (old.matcher ?? '') === matcher && old.hooks[row.index]) {
    const timeout = d.timeout.trim() ? Number(d.timeout.trim()) : DEFAULT_HOOK_TIMEOUT;
    const command: HookCommand = { ...old.hooks[row.index]!, command: d.command.trim() };
    if (timeout === DEFAULT_HOOK_TIMEOUT) delete command.timeout; else command.timeout = timeout;
    const entries = hooks[event]!.map((e) => ({ ...e, hooks: [...e.hooks] }));
    entries[row.entry]!.hooks[row.index] = command;
    return { ...hooks, [event]: entries };
  }
  return addHook(removeHook(hooks, row), d);
}

/** The daemon refuses a stale save with the typed code `claude_hooks_conflict`. */
export function isHooksConflict(err: unknown): boolean {
  return String(err).includes('claude_hooks_conflict');
}

// ---------- custom providers ----------

export interface ApiKeyState { state: 'set' | 'absent'; fingerprint?: string; set_at?: string }
export interface CustomProvider { id: string; name: string; base_url: string; default_model: string; api_key: ApiKeyState }

export function parseProviders(raw: unknown): CustomProvider[] | null {
  if (!isObj(raw) || !Array.isArray(raw.providers)) return null;
  const out: CustomProvider[] = [];
  for (const p of raw.providers) {
    if (!isObj(p) || typeof p.id !== 'string' || typeof p.name !== 'string' || typeof p.base_url !== 'string') continue;
    const key = isObj(p.api_key) ? p.api_key : {};
    out.push({
      id: p.id.toLowerCase(),
      name: p.name,
      base_url: p.base_url,
      default_model: typeof p.default_model === 'string' ? p.default_model : '',
      api_key: {
        state: key.state === 'set' ? 'set' : 'absent',
        ...(typeof key.fingerprint === 'string' ? { fingerprint: key.fingerprint } : {}),
        ...(typeof key.set_at === 'string' ? { set_at: key.set_at } : {}),
      },
    });
  }
  return out;
}

/** The key line of a provider; the key itself never reaches the app, only its state and fingerprint. */
export function keyStateText(k: ApiKeyState): string {
  if (k.state !== 'set') return 'no API key';
  return k.fingerprint ? `API key set (${k.fingerprint})` : 'API key set';
}

export const providerDetail = (p: CustomProvider): string => `${p.base_url} · ${keyStateText(p.api_key)}`;

/** The model roles a custom provider can serve (`CustomProviderRole`), with the label the pickers show. */
export const PROVIDER_ROLES: ReadonlyArray<{ role: string; label: string }> = [
  { role: 'title_model', label: 'Title model' },
  { role: 'memory_model_fallback', label: 'Memory model fallback' },
  { role: 'dream_model', label: 'Dream model' },
  { role: 'prompt_compile_model', label: 'Prompt compile model' },
];

/** One model role's stored route (`ListCustomProviderRoutes`): the provider, or none while detached. */
export interface RoleRoute { role: string; id: string | null; name: string | null; api_key: ApiKeyState | null }

/** Every role of the daemon's route list, in the daemon's order; an unknown role is dropped. */
export function parseRoutes(raw: unknown): RoleRoute[] | null {
  if (!isObj(raw) || !Array.isArray(raw.routes)) return null;
  const out: RoleRoute[] = [];
  for (const r of raw.routes) {
    if (!isObj(r) || typeof r.role !== 'string' || !PROVIDER_ROLES.some((x) => x.role === r.role)) continue;
    const id = typeof r.id === 'string' && r.id ? r.id.toLowerCase() : null;
    const key = id && isObj(r.api_key) ? r.api_key : null;
    out.push({
      role: r.role,
      id,
      name: id && typeof r.name === 'string' ? r.name : null,
      api_key: key
        ? {
            state: key.state === 'set' ? 'set' : 'absent',
            ...(typeof key.fingerprint === 'string' ? { fingerprint: key.fingerprint } : {}),
            ...(typeof key.set_at === 'string' ? { set_at: key.set_at } : {}),
          }
        : null,
    });
  }
  return out;
}

/** The current route of `role`, or null while detached or unknown. */
export function routeOf(routes: readonly RoleRoute[], role: string): RoleRoute | null {
  return routes.find((r) => r.role === role && r.id !== null) ?? null;
}

/** How a role's current route reads in a picker: the provider name and its key state, or the default. */
export function routeText(route: RoleRoute | null): string {
  if (!route || !route.id) return 'default (not routed)';
  return `${route.name ?? route.id}${route.api_key ? ` · ${keyStateText(route.api_key)}` : ''}`;
}

export interface ProviderForm { name: string; base_url: string; default_model: string; api_key: string; clear_key: string }

/** Null when the form can be sent. */
export function validateProviderForm(v: ProviderForm): string | null {
  if (!v.name.trim()) return 'Name is required';
  if (!/^https?:\/\/\S+$/i.test(v.base_url.trim())) return 'Base URL must start with http:// or https://';
  if (v.clear_key === 'true' && v.api_key.trim()) return 'Either type a new key or remove the stored one, not both';
  return null;
}

/** The arguments of the `put_custom_provider` command. A blank key keeps the stored one. */
export function providerPutArgs(id: string | null, v: ProviderForm): Record<string, unknown> {
  const secret = v.api_key.trim();
  const action = v.clear_key === 'true' ? 'clear' : secret ? 'set' : 'keep';
  return {
    id,
    name: v.name.trim(),
    baseUrl: v.base_url.trim(),
    defaultModel: v.default_model.trim(),
    keyAction: action,
    secret: action === 'set' ? secret : null,
  };
}

/** The toast after a provider delete: the roles that were routed to it are detached. */
export function deleteProviderMessage(name: string, clearedRoutes: unknown): string {
  const roles = Array.isArray(clearedRoutes) ? clearedRoutes.filter((r): r is string => typeof r === 'string') : [];
  const label = roles.map((r) => PROVIDER_ROLES.find((x) => x.role === r)?.label ?? r);
  return label.length > 0 ? `Deleted ${name}; detached: ${label.join(', ')}` : `Deleted ${name}`;
}

// ---------- message bridges ----------

export type BridgeKind = 'signal' | 'imessage';
export interface BridgeConfig { enabled: boolean; account: string; allow_from: string; working_dir: string }
export interface MessageBridge { kind: BridgeKind; path: string; config: BridgeConfig | null }

export const BRIDGE_KINDS: ReadonlyArray<{ kind: BridgeKind; label: string }> = [
  { kind: 'signal', label: 'Signal' },
  { kind: 'imessage', label: 'iMessage' },
];

export function parseBridges(raw: unknown): MessageBridge[] | null {
  if (!isObj(raw) || !Array.isArray(raw.bridges)) return null;
  const out: MessageBridge[] = [];
  for (const b of raw.bridges) {
    if (!isObj(b) || (b.kind !== 'signal' && b.kind !== 'imessage')) continue;
    const c = isObj(b.config) ? b.config : null;
    out.push({
      kind: b.kind,
      path: typeof b.path === 'string' ? b.path : '',
      config: c
        ? {
          enabled: c.enabled === true,
          account: typeof c.account === 'string' ? c.account : '',
          allow_from: typeof c.allow_from === 'string' ? c.allow_from : '',
          working_dir: typeof c.working_dir === 'string' ? c.working_dir : '',
        }
        : null,
    });
  }
  return out;
}

export const bridgeLabel = (kind: BridgeKind): string => BRIDGE_KINDS.find((k) => k.kind === kind)?.label ?? kind;

export function bridgeDetail(b: MessageBridge): string {
  if (!b.config) return 'not configured';
  return `${b.config.enabled ? 'enabled' : 'disabled'} · ${b.config.working_dir}`;
}

export interface BridgeForm { enabled: string; account: string; allow_from: string; working_dir: string }

/** Null when the form can be sent. iMessage has no account. */
export function validateBridgeForm(kind: BridgeKind, v: BridgeForm): string | null {
  if (!v.working_dir.trim()) return 'Working directory is required';
  if (kind === 'signal' && !v.account.trim()) return 'Signal account required';
  return null;
}

/** The arguments of the `put_message_bridge` command. */
export function bridgePutArgs(kind: BridgeKind, v: BridgeForm): Record<string, unknown> {
  return {
    kind,
    enabled: v.enabled === 'true',
    account: kind === 'imessage' ? '' : v.account.trim(),
    allowFrom: v.allow_from.trim(),
    workingDir: v.working_dir.trim(),
  };
}
