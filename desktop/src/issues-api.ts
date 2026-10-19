// Typed adapter over the Issues Tauri commands (src-tauri/src/issues.rs).
// Each call is one daemon method; the Rust side validates the shape again.
import { invoke } from './api.js';
import type {
  DepDirection, DependencyPage, EventPage, HandoffPage, Issue, IssuePage, IssueRow, MutationResult, PageCursor,
} from './issues-model.js';

export const issuesApi = {
  listPage: (request: Record<string, unknown>) => invoke<IssuePage>('list_issues_page', { request }),
  get: (projectId: string, issueId: string) => invoke<IssueRow | null>('get_issue', { projectId, issueId }),
  create: (projectId: string, draft: Record<string, unknown>) =>
    invoke<MutationResult>('create_issue', { projectId, draft }),
  update: (projectId: string, issueId: string, write: Record<string, unknown>) =>
    invoke<MutationResult>('update_issue', { projectId, issueId, write }),
  setStatus: (projectId: string, issueId: string, write: Record<string, unknown>) =>
    invoke<MutationResult>('update_issue_status', { projectId, issueId, write }),
  archive: (projectId: string, issueId: string, write: Record<string, unknown>) =>
    invoke<MutationResult>('archive_issue', { projectId, issueId, write }),
  restore: (projectId: string, issueId: string, write: Record<string, unknown>) =>
    invoke<MutationResult>('restore_issue', { projectId, issueId, write }),
  dependencies: (projectId: string, issueId: string, direction: DepDirection, cursor?: PageCursor | null) =>
    invoke<DependencyPage>('list_issue_dependencies', {
      projectId, issueId, page: { direction, limit: 32, ...(cursor ? { cursor } : {}) },
    }),
  addDependency: (projectId: string, issueId: string, dependsOnId: string) =>
    invoke<{ changed: boolean }>('add_issue_dependency', { projectId, issueId, dependsOnId }),
  removeDependency: (projectId: string, issueId: string, dependsOnId: string) =>
    invoke<{ changed: boolean }>('remove_issue_dependency', { projectId, issueId, dependsOnId }),
  events: (projectId: string, issueId: string, afterSequence?: number | null) =>
    invoke<EventPage>('list_issue_events', { projectId, issueId, afterSequence: afterSequence ?? null, limit: 32 }),
  handoffs: (projectId: string, issueId: string, afterSequence?: number | null) =>
    invoke<HandoffPage>('list_issue_handoffs', { projectId, issueId, afterSequence: afterSequence ?? null, limit: 8 }),
};

export type { Issue };
