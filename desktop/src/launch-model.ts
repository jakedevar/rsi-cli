// New session dialog wording (local #211): plain names for the providers, short explanations for
// Model / Effort / Folder, and the success toast. Pure (no DOM), so a test can pin every string.
// The submitted values never come from here: the provider option's `value` stays the exact
// `Session.provider` enum string the daemon expects.

import { ownProjects } from './project-model.js';

export interface ProviderInfo {
  /** The label shown in the Provider list. */
  name: string;
  /** One line: what it runs and what it needs. */
  description: string;
}

/** Every `Session.provider` the daemon names (`SessionProvider::CANONICAL_NAMES`), by exact enum string. */
export const PROVIDER_INFO: Readonly<Record<string, ProviderInfo>> = {
  Claude: { name: 'Claude Code (Anthropic CLI)', description: 'Runs the Claude Code command-line tool with your Claude login.' },
  Codex: { name: 'Codex CLI', description: 'Runs the OpenAI Codex command-line tool with your Codex login.' },
  Pioneer: { name: 'Pioneer', description: 'Pioneer models through the Codex tool; needs a Pioneer API key (Settings › Provider keys).' },
  OpenRouter: { name: 'OpenRouter', description: 'Many vendors’ models through one OpenRouter API key (Settings › Provider keys).' },
  Bedrock: { name: 'Amazon Bedrock', description: 'Models on your AWS account; needs AWS credentials.' },
  Local: { name: 'Local model', description: 'A model running on this computer (Ollama or another OpenAI-compatible server); no key needed.' },
  Antigravity: { name: 'Google Antigravity CLI', description: 'Runs Google’s Antigravity command-line tool (agy) with its own login.' },
  CodexAppServer: { name: 'Codex app server', description: 'Codex through its app server, so approvals show up here; uses your Codex login.' },
  Harness: { name: 'Direct API (Harness)', description: 'RSI talks to the model’s API itself instead of a command-line tool; needs an API key (Settings › Provider keys).' },
};

/** The name shown for a provider; a provider this table does not know shows its raw name. */
export function providerLabel(provider: string): string {
  return PROVIDER_INFO[provider]?.name ?? provider;
}

/** The one-line description (the option tooltip and the line under the list); '' when unknown. */
export function providerDescription(provider: string): string {
  return PROVIDER_INFO[provider]?.description ?? '';
}

export const EFFORT_HINT = 'How hard the model thinks: higher is slower and costs more. Default follows Settings › Models.';
export const MODEL_HINT = 'Leave blank for the provider’s default, pick one from the list, or type any model id.';
export const FOLDER_HINT = 'Blank: the project’s folder';
export const FOLDER_PICK_LABEL = 'Choose…';
export const FOLDER_PICK_TITLE = 'Pick a folder on this computer; you can still type or edit the path.';
/**
 * Where a session with neither a project nor a folder runs. The desktop cannot read the daemon's own
 * fallback directory (the New session request carries none), so it says so in these words.
 */
export const NO_FOLDER_WORDS = 'your home folder or the daemon’s default';
export const NO_FOLDER_TITLE = 'Start without a project or folder?';
export const NO_FOLDER_ASK = `This session is not in a project and has no folder, so it will run in ${NO_FOLDER_WORDS}.`;
export const NO_FOLDER_START = 'Start anyway';
export const NO_FOLDER_CHOOSE = 'Choose a folder';
export const PROMPT_PLACEHOLDER = 'Describe the task…';

const TITLE_MAX = 60;

/** What to call the new session in the toast: the title the daemon returned, else the prompt's first line. */
export function launchedName(result: unknown, query: string): string {
  const own = result && typeof result === 'object' ? (result as { title?: unknown; session?: { title?: unknown } }) : null;
  const title = [own?.title, own?.session?.title].find((t): t is string => typeof t === 'string' && t.trim() !== '');
  const text = (title ?? query).trim().split('\n')[0]!.trim();
  return text.length > TITLE_MAX ? `${text.slice(0, TITLE_MAX - 1).trimEnd()}…` : text;
}

/** `Session launched: <title>` (the sandbox variant names the worktree); bare when there is no name. */
export function launchedToast(result: unknown, query: string, sandbox: boolean): string {
  const name = launchedName(result, query);
  const base = sandbox ? 'Session launched in its own git worktree' : 'Session launched';
  return name ? `${base}: ${name}` : base;
}

/** What the New session dialog needs to choose a Project when it opens (local #271). */
export interface DefaultProjectInput {
  /** Every project the dialog offers. */
  projects: readonly { id: string }[];
  /** The open session's project. */
  selectedProjectId?: string | null;
  /** The session list's project filter (empty: All projects). */
  filterProjectId?: string | null;
  /** The project of the last session launched from this dialog (kept in local storage). */
  lastUsedId?: string | null;
  /** The sessions on screen: the project of the most recently updated one is the fallback. */
  sessions?: readonly { project_id?: string | null; updated_at?: string }[];
}

/**
 * The Project the dialog opens with: the open session's project, else the list filter's, else the one
 * launched from last, else the project of the most recently updated session, else the only project, else
 * none (empty). A candidate that is not among `projects` is skipped. "none" stays a choice in the list.
 */
export function defaultProjectId(i: DefaultProjectInput): string {
  // RSI home is never a default (desktop #319): only the person's own projects are candidates.
  const own = ownProjects(i.projects);
  const known = new Set(own.map((p) => p.id));
  const recent = [...(i.sessions ?? [])]
    .filter((s) => s.project_id)
    .sort((a, b) => (b.updated_at ?? '').localeCompare(a.updated_at ?? ''))[0]?.project_id;
  const only = own.length === 1 ? own[0]!.id : null;
  for (const id of [i.selectedProjectId, i.filterProjectId, i.lastUsedId, recent, only]) {
    if (id && known.has(id)) return id;
  }
  return '';
}

/**
 * The Folder box after the Project changed. A project's folder fills the box while the person has not
 * written their own path: an empty box, or the very text `auto` filled in earlier, is replaced (a project
 * with no folder empties it again); any other text is theirs and stays.
 */
export function folderAfterProjectChange(current: string, auto: string, projectPath: string | null | undefined): { value: string; auto: string } {
  const text = current.trim();
  if (text !== '' && text !== auto) return { value: current, auto };
  const path = (projectPath ?? '').trim();
  return { value: path, auto: path };
}

/** Whether a launch has neither a project nor a folder (so the daemon picks where it runs): ask first. */
export function launchHasNoPlace(projectId: string, folder: string): boolean {
  return projectId.trim() === '' && folder.trim() === '';
}
