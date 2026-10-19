// The admin overlays slice's entries for the action registry (pure, no DOM;
// slice 13a, hub #3015): the operator configuration overlays the TUI keeps in
// Settings and behind <Space>K / :diagnostics. The DOM side is
// actions/admin.ts; the rules for each overlay live in admin-model.ts.
// The registry collects SLICE through the generated actions/index.generated.ts.
import type { DesktopBinding } from '../keyengine.js';
import type { ActionId, ActionSpec, SliceSpec, UiSnapshot } from '../registry.js';

export type AdminActionId =
  | 'admin-schedules' | 'admin-provider-keys' | 'admin-mcp' | 'admin-diagnostics'
  | 'admin-satellites' | 'admin-remote' | 'admin-scratch' | 'admin-settlement';

/** What the slice's actions can see (built by actions/admin.ts). */
export interface AdminSnapshot {
  /** The admin overlay that is open, if any. */
  open: string | null;
}

type Rule = (s: UiSnapshot) => string | null;

export const ADMIN_RULES: Readonly<Record<AdminActionId, Rule>> = {
  'admin-schedules': () => null,
  'admin-provider-keys': () => null,
  'admin-mcp': () => null,
  'admin-diagnostics': () => null,
  'admin-satellites': () => null,
  'admin-remote': () => null,
  'admin-scratch': () => null,
  'admin-settlement': () => null,
};

export const ADMIN_ACTIONS: readonly ActionSpec[] = [
  { id: 'admin-schedules', title: 'Scheduled jobs…', group: 'view', menuBreak: true, detail: 'Create, edit, enable, run and delete scheduled jobs (<Space>K)' },
  { id: 'admin-provider-keys', title: 'Provider keys…', group: 'view', detail: 'Set, rotate, check, clear or import provider keys; values are write-only and never shown' },
  { id: 'admin-mcp', title: 'MCP servers…', group: 'view', detail: 'Add, edit and enable MCP servers; set, rotate or clear their credential (write-only)' },
  { id: 'admin-satellites', title: 'Satellite registry…', group: 'view', detail: 'Peers, links and cached remote sessions on the hub; add, edit, enable, repair, probe; the inbound policy' },
  { id: 'admin-remote', title: 'Remote access…', group: 'view', detail: 'The phone view over the tailnet: devices, exposed projects, answers and messages' },
  { id: 'admin-scratch', title: 'Legacy scratch adoption…', group: 'view', detail: 'Record unrecorded scratch directories so reclaim may delete them (confirmation lists every path)' },
  { id: 'admin-settlement', title: 'Source-worktree settlement…', group: 'view', detail: 'Audit a cohort, then apply only after typing its exact APPLY phrase' },
  { id: 'admin-diagnostics', title: 'Diagnostics', group: 'view', detail: 'Daemon round trip, frame interval and the listed sessions (:diagnostics)' },
];

type Bindings = Readonly<Record<string, DesktopBinding<ActionId>>>;
const b = (action: ActionId): DesktopBinding<ActionId> => ({ action });

/** TUI Normal-mode keys the slice runs (keybindings.md notation). */
export const ADMIN_KEY_BINDINGS: Bindings = {
  '<Space>K': b('admin-schedules'),
};

/** TUI `:` commands (canonical names) the slice runs. */
export const ADMIN_COMMAND_BINDINGS: Bindings = {
  diagnostics: b('admin-diagnostics'),
};

declare module '../registry.js' {
  interface UiSnapshot {
    /** The admin overlays (slice 13a, actions/admin.ts). */
    admin?: AdminSnapshot;
  }
}

export const SLICE: SliceSpec<AdminActionId> = {
  name: 'admin overlays (slice 13a, hub #3015)',
  order: 70,
  actions: ADMIN_ACTIONS,
  rules: ADMIN_RULES,
  keys: ADMIN_KEY_BINDINGS,
  commands: ADMIN_COMMAND_BINDINGS,
};
