// Typed adapter over the Rust backend's Tauri commands. The webview never
// talks to rsid directly; every call goes through one named command.
import type { ItemState, MenuInstallReport, MenuNode, Submenu } from './registry.js';
import type { DaemonService, StartOutcome } from './onboarding-model.js';

export type SessionStatus =
  | 'Starting' | 'Running' | 'WaitingApproval' | 'Completed'
  | 'Failed' | 'Interrupted' | 'Archived' | 'Deleted';

export interface QuestionOption { label: string; description: string }
export interface QuestionItem {
  question: string;
  header: string;
  options?: QuestionOption[];
  multiSelect?: boolean;
}
export interface PendingQuestion { questions: QuestionItem[] }

export interface Session {
  id: string;
  status: SessionStatus | string;
  provider: string;
  session_kind?: string;
  title?: string | null;
  agent_role?: string | null;
  epic_spawn_ordinal?: number | null;
  query: string;
  /** How the run ended (`budget_exhausted`, `provider_error:...`, ...): the TUI's `[terminal: ...]` text. */
  terminal_reason?: string | null;
  /** The model's own stop cause for the last turn (`end_turn`, `max_tokens`, ...). */
  stop_reason?: string | null;
  short_summary?: string | null;
  working_dir: string;
  git_branch?: string | null;
  model?: string | null;
  project_id?: string | null;
  parent_id?: string | null;
  lead_session_id?: string | null;
  group_id?: string | null;
  pinned_at?: string | null;
  /** The scheduled job that launched it (the Jobs zone, gj). */
  scheduled_job_id?: string | null;
  tags?: string[] | null;
  description?: string | null;
  sandbox_root?: string | null;
  pending_question?: PendingQuestion | null;
  /** Marked to archive itself when it finishes (MarkPendingArchive); meaningful while Starting, Running or WaitingApproval. */
  pending_archive?: boolean;
  /** Operator rating, 1-10 (UpdateSessionRating). */
  rating?: number | null;
  /** The topology bound to this session (UpdateSessionWorkflow); children usually inherit it from their Epic. */
  workflow_id?: string | null;
  workflow_id_override?: string | null;
  created_at: string;
  updated_at: string;
}

export interface Project {
  id: string;
  name: string;
  path?: string | null;
  color: string;
}

export interface SessionLabel {
  id: string;
  name: string;
  description?: string | null;
  project_id?: string | null;
  color: string;
}

export interface ConversationEvent {
  id: number;
  session_id: string;
  sequence: number;
  event_type: string;
  role?: 'User' | 'Assistant' | null;
  created_at: string;
  content: string;
  tool_name?: string | null;
  tool_input?: unknown;
  /** Provider tool call id: pairs a ToolUse with its ToolResult. */
  tool_use_id?: string | null;
  metadata?: unknown;
}

export interface DaemonInfo {
  socket: string;
  connected: boolean;
  health?: unknown;
  error?: string;
}

export interface LaunchRequest {
  query: string;
  provider: string;
  model?: string;
  effort?: string;
  project_id?: string;
  working_dir?: string;
}

interface TauriGlobal {
  core: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
}

function tauri(): TauriGlobal {
  const t = (globalThis as unknown as { __TAURI__?: TauriGlobal }).__TAURI__;
  if (!t) throw new Error('Tauri runtime not available');
  return t;
}

export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await tauri().core.invoke<T>(cmd, args);
  } catch (e) {
    throw new Error(typeof e === 'string' ? e : e instanceof Error ? e.message : String(e));
  }
}

export const api = {
  daemonInfo: () => invoke<DaemonInfo>('daemon_info'),
  /** GetDaemonInfo (build_sha, local_build): the newer-build notice. An older rsid answers method-not-found. */
  daemonBuild: () => invoke<unknown>('get_daemon_build'),
  /** Is rsid installed as an OS service this window can start (src-tauri/src/service.rs, local #232)? */
  daemonService: () => invoke<DaemonService>('daemon_service'),
  /** Ask launchd or systemd to start the installed rsid; a fixed argv, no arguments from here. */
  startDaemon: () => invoke<StartOutcome>('start_daemon'),
  restartDaemon: () => invoke<StartOutcome>('restart_daemon'),
  /** Prints the boot result to the process log (src-tauri/src/boot.rs). */
  bootReport: (report: unknown) => invoke<void>('boot_report', { report }),
  providers: () => invoke<string[]>('providers'),
  listProjects: () => invoke<Project[]>('list_projects'),
  listSessions: () => invoke<Session[]>('list_sessions'),
  listArchivedSessions: (projectId?: string | null) =>
    invoke<Session[]>('list_archived_sessions', { projectId: projectId ?? null }),
  getSession: (sessionId: string) => invoke<Session>('get_session', { sessionId }),
  getConversation: (sessionId: string, sinceSequence?: number) =>
    invoke<ConversationEvent[]>('get_conversation', { sessionId, sinceSequence: sinceSequence ?? null }),
  sendMessage: (sessionId: string, text: string) => invoke<unknown>('send_message', { sessionId, text }),
  interrupt: (sessionId: string) => invoke<unknown>('interrupt_session', { sessionId }),
  /** Markdown to HTML sanitized by the backend; the only HTML safe for innerHTML. */
  renderMarkdown: (texts: string[]) => invoke<string[]>('render_markdown', { texts }),
  launch: (request: LaunchRequest) => invoke<unknown>('launch_session', { request }),
  answerQuestion: (sessionId: string, responseText: string) =>
    invoke<unknown>('answer_question', { sessionId, responseText }),
  archiveSession: (sessionId: string) => invoke<unknown>('archive_session', { sessionId }),
  /** An active session cannot be archived (rsid refuses ArchiveSession); this sets or clears "archive when it finishes". A write. */
  markPendingArchive: (sessionId: string, pending: boolean) =>
    invoke<unknown>('mark_pending_archive', { sessionId, pending }),
  /** The same command the U key, the menu and the palette run (actions/lifecycle.ts); the Undo toast calls it by session id. */
  unarchiveSession: (sessionId: string) => invoke<unknown>('unarchive_session', { sessionId }),
  pauseSession: (sessionId: string, level: 'soft' | 'hard') =>
    invoke<unknown>('pause_session', { sessionId, level }),
  getOperatorPause: (sessionId: string) =>
    invoke<{ pause_level: 'none' | 'soft' | 'hard' }>('get_operator_pause', { sessionId }),
  setOperatorPause: (sessionId: string, level: 'none' | 'soft') =>
    invoke<unknown>('set_operator_pause', { sessionId, level }),
  /** The native window's light/dark follows the app theme; "system" hands it back to the OS (src-tauri/src/window_theme.rs, local #283). */
  setWindowTheme: (theme: 'light' | 'dark' | 'system') => invoke<void>('set_window_theme', { theme }),
  setAttentionCount: (count: number) => invoke<void>('set_attention_count', { count }),
  /** One OS notification (src-tauri/src/notify.rs, local #294); a click opens `sessionId` (null: the Settings test). */
  notifySession: (sessionId: string | null, title: string, body: string) => invoke<void>('notify_session', { sessionId, title, body }),
  /** Whether macOS lets RSI show notifications: granted, denied, not_determined or unknown (src-tauri/src/notify.rs, local #300). */
  notifyPermission: () => invoke<string>('notify_permission'),
  /** Opens System Settings › Notifications (src-tauri/src/nav.rs, local #300). */
  openNotificationSettings: () => invoke<void>('open_notification_settings'),
  /** Native menus built from the action registry (src-tauri/src/menu.rs validates them and installs what fits, #305). */
  setAppMenu: (menu: Submenu[]) => invoke<MenuInstallReport>('set_app_menu', { menu }),
  syncMenuState: (items: Record<string, ItemState>) => invoke<void>('sync_menu_state', { items }),
  popupContextMenu: (items: MenuNode[]) => invoke<void>('popup_context_menu', { items }),
  quit: () => invoke<void>('quit_app'),
};

/** True inside the Tauri shell (false in a plain browser preview). */
export function inTauri(): boolean {
  return !!(globalThis as unknown as { __TAURI__?: TauriGlobal }).__TAURI__;
}

/** Hear an event the Rust side emits (the page holds `core:event:allow-listen`); a plain browser preview hears nothing. */
export function onTauriEvent(name: string, cb: (payload: unknown) => void): void {
  const listen = (globalThis as unknown as { __TAURI__?: { event?: { listen(n: string, f: (e: { payload: unknown }) => void): Promise<() => void> } } }).__TAURI__?.event?.listen;
  if (listen) void listen(name, (e) => cb(e.payload));
}
