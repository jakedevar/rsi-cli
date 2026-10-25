/**
 * The built-in "RSI home" project (hub #3315, #172): rsid registers it at every start with this fixed id
 * (`rsi_common::Project::HOME_ID`), at the daemon's $HOME, for global and portfolio manager seats. It is
 * a system project, not one the person made, so (desktop #319) it stays in the project filter and pickers,
 * labelled and after their own projects, but it never counts as "a project exists" and is never a default.
 * The TUI lists it like any project; only its Global Dashboard marks it (`is_home`).
 */

export const HOME_PROJECT_ID = '00000000-0000-4000-8000-727369686f6d';

/** Why rsid refuses a worktree there: the person's home folder is not a repository to branch from. */
export const HOME_WORKTREE_WHY = 'Not for RSI home: it is your home folder, not a code folder, so a git worktree is not offered there.';
/** Why Edit / Delete are off for it. */
export const HOME_LOCKED_WHY = 'RSI home is built in: rsid keeps it at your home folder and does not allow editing or deleting it.';

export const HOME_PROJECT_LABEL = 'RSI home (built-in home project)';

export interface ProjectLike { id: string }

export const isHomeProjectId = (id: string | null | undefined): boolean => id === HOME_PROJECT_ID;

/** Projects the person made: everything but RSI home. */
export function ownProjects<T extends ProjectLike>(projects: readonly T[]): T[] {
  return projects.filter((p) => !isHomeProjectId(p.id));
}

/** The person's own projects in rsid's order, then RSI home last. */
export function sortProjects<T extends ProjectLike>(projects: readonly T[]): T[] {
  return [...ownProjects(projects), ...projects.filter((p) => isHomeProjectId(p.id))];
}

/** The name shown in a filter or picker: RSI home says what it is. */
export function projectLabel(p: ProjectLike & { name: string }): string {
  return isHomeProjectId(p.id) ? HOME_PROJECT_LABEL : p.name;
}
