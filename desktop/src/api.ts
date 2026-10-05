// Typed adapter over the Rust backend's Tauri commands. The webview never
// talks to rsid directly; every call goes through one named command.

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
  short_summary?: string | null;
  working_dir: string;
  git_branch?: string | null;
  model?: string | null;
  project_id?: string | null;
  parent_id?: string | null;
  pending_question?: PendingQuestion | null;
  created_at: string;
  updated_at: string;
}

export interface Project {
  id: string;
  name: string;
  path?: string | null;
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

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await tauri().core.invoke<T>(cmd, args);
  } catch (e) {
    throw new Error(typeof e === 'string' ? e : e instanceof Error ? e.message : String(e));
  }
}

export const api = {
  daemonInfo: () => invoke<DaemonInfo>('daemon_info'),
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
  pauseSession: (sessionId: string, level: 'soft' | 'hard') =>
    invoke<unknown>('pause_session', { sessionId, level }),
  getOperatorPause: (sessionId: string) =>
    invoke<{ pause_level: 'none' | 'soft' | 'hard' }>('get_operator_pause', { sessionId }),
  setOperatorPause: (sessionId: string, level: 'none' | 'soft') =>
    invoke<unknown>('set_operator_pause', { sessionId, level }),
  setAttentionCount: (count: number) => invoke<void>('set_attention_count', { count }),
};
