// Typed adapter over the operator-config Tauri commands (src-tauri/src/operator_config.rs).
// Each call is one operator-only daemon method; the Rust side validates the shape again.
import { invoke } from './api.js';
import {
  type BridgeKind, type ClaudeHooksView, type CustomProvider, type HooksBlock, type MessageBridge, type RoleRoute, parseBridges,
  parseHooksView, parseProviders, parseRoutes,
} from './operator-config-model.js';

async function read<T>(what: string, raw: Promise<unknown>, parse: (v: unknown) => T | null): Promise<T> {
  const v = parse(await raw);
  if (v === null) throw new Error(`the daemon sent no ${what}`);
  return v;
}

export const operatorConfigApi = {
  listHooks: (): Promise<ClaudeHooksView> => read('hooks view', invoke('list_claude_hooks'), parseHooksView),
  putHooks: (expectedRevision: string, hooks: HooksBlock): Promise<ClaudeHooksView> =>
    read('hooks view', invoke('put_claude_hooks', { expectedRevision, hooks }), parseHooksView),
  deleteHooks: (expectedRevision: string): Promise<ClaudeHooksView> =>
    read('hooks view', invoke('delete_claude_hooks', { expectedRevision }), parseHooksView),
  listProviders: (): Promise<CustomProvider[]> =>
    read('provider list', invoke('list_custom_providers'), parseProviders),
  putProvider: (args: Record<string, unknown>) => invoke<unknown>('put_custom_provider', args),
  deleteProvider: (id: string) => invoke<{ cleared_routes?: unknown }>('delete_custom_provider', { id }),
  listRoutes: (): Promise<RoleRoute[]> => read('route list', invoke('list_custom_provider_routes'), parseRoutes),
  setRoute: (role: string, id: string | null) => invoke<unknown>('set_custom_provider_route', { role, id }),
  listBridges: (): Promise<MessageBridge[]> => read('bridge list', invoke('list_message_bridges'), parseBridges),
  putBridge: (args: Record<string, unknown>) => invoke<unknown>('put_message_bridge', args),
  deleteBridge: (kind: BridgeKind) => invoke<unknown>('delete_message_bridge', { kind }),
};
