// The headless smoke's step groups (local #250, #281). `node scripts/smoke.mjs --steps GROUP` runs `boot` (always) and the
// steps of one group; with no flag it runs every step in order. tests/smoke.test.mjs runs one group per test, each with its
// own budget, so a new step lengthens one group instead of one serial run.
//
// A step names its own group: `{ id, group: 'core', title, run }` where the step is defined. Nothing here lists step ids, so
// two slices that each add a step touch different lines (or, in their own file under scripts/smoke-steps/, different files).
// This file only holds the group names and budgets, and derives everything else from the steps.
//
// ADD A NEW STEP: put it in a new file `scripts/smoke-steps/<name>.mjs` (see the README there) with `group:` set. A step with
// no group, an unknown group or more than one group fails the "every smoke step runs in exactly one group" test, which names
// the id.
import { readdirSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

/** Runs first in every group: it is the page boot, not a step of the checklist. */
export const PRELUDE = 'boot';

/**
 * The groups, with the time one test may spend on each. A group over its budget is too big: split it (add a name here and
 * move steps by editing their `group:`). Measured on a laptop (local #281, fake daemon): core 24 s, settings 52 s, manager 28 s,
 * keyboard 44 s, a11y 30 s.
 */
export const SMOKE_GROUPS = {
  // The session list, the opened conversation, its text contrast, command mode, Help and the terminal overlay.
  core: { budgetMs: 90000 },
  // The Settings view and the dialogs reached from it (Remote, Scheduled jobs, Vim-keys-gated dialogs, text editors).
  settings: { budgetMs: 90000 },
  // The manager tree and board, the global dashboard, Trash, approvals, the model switcher, budgets and usage.
  manager: { budgetMs: 90000 },
  // Where Tab goes and where focus returns: the tab-* steps, focus-return, the Issue and archive buttons.
  keyboard: { budgetMs: 90000 },
  // What the page announces and which controls are tab stops: tab-stops and screen-reader.
  a11y: { budgetMs: 90000 },
};

export const GROUP_NAMES = Object.keys(SMOKE_GROUPS);

/**
 * Problems with how `steps` declare their groups, as sentences that name the step; empty when every step has an id that no
 * other step has and exactly one known group.
 */
export function validateSteps(steps, groupNames = GROUP_NAMES) {
  const problems = [];
  const seen = new Set();
  for (const step of steps) {
    const id = step?.id;
    if (typeof id !== 'string' || !id) { problems.push(`a step has no id: ${JSON.stringify(step?.title ?? null)}`); continue; }
    if (id === PRELUDE) problems.push(`step "${id}" uses the prelude's id: boot is the page boot every group runs, not a step`);
    if (seen.has(id)) problems.push(`two steps share the id "${id}", so it would run twice or in two groups`);
    seen.add(id);
    const declared = step.group;
    if (Array.isArray(declared) || 'groups' in step) problems.push(`step "${id}" declares more than one group: a step has exactly one \`group\``);
    else if (declared === undefined || declared === null || declared === '') problems.push(`step "${id}" declares no group: add \`group: '${groupNames[0]}'\` (one of ${groupNames.join(', ')}) next to its id`);
    else if (typeof declared !== 'string' || !groupNames.includes(declared)) problems.push(`step "${id}" declares the unknown group ${JSON.stringify(declared)} (one of ${groupNames.join(', ')})`);
  }
  return problems;
}

/** group name -> step ids in the order the steps run, derived from the steps' own `group` (validate first). */
export function groupSteps(steps, groupNames = GROUP_NAMES) {
  const groups = Object.fromEntries(groupNames.map((g) => [g, []]));
  for (const step of steps) groups[step.group]?.push(step.id);
  return groups;
}

/** The ids a `--steps` selection runs: the ids of the steps in those groups (the run keeps the steps' order). */
export function selectedIds(steps, groups) {
  return new Set(steps.filter((s) => groups.includes(s.group)).map((s) => s.id));
}

/**
 * The refusals a group's run must report: a step that tries a write the read-only bridge refuses names it in
 * `refuses: ['command', ...]`, so the expectation sits beside the step. In step order, one entry per attempt.
 */
export function expectedRefusals(steps, group) {
  return steps.filter((s) => s.group === group).flatMap((s) => (s.refuses ?? []).map((cmd) => `${cmd}: not on the read-only allowlist`));
}

/** The `n` slowest results of a report, slowest first, as "id 12.3 s" strings. */
export function slowestSteps(results, n = 5) {
  return [...results].filter((r) => typeof r.durationMs === 'number').sort((a, b) => b.durationMs - a.durationMs).slice(0, n)
    .map((r) => `${r.id} ${(r.durationMs / 1000).toFixed(1)} s`);
}

/**
 * The step factories in `dir`: every `*.mjs` file that does not start with `_`, in file-name order, each a default export
 * `(kit) => step[]`. A missing directory holds none. Files are found by listing the directory, so adding one edits no
 * existing file.
 */
export async function loadStepFactories(dir) {
  let names;
  try { names = readdirSync(dir).filter((n) => n.endsWith('.mjs') && !n.startsWith('_')).sort(); } catch (e) {
    if (e.code === 'ENOENT') return [];
    throw e;
  }
  const factories = [];
  for (const name of names) {
    const mod = await import(pathToFileURL(join(dir, name)).href);
    if (typeof mod.default !== 'function') throw new Error(`${join(dir, name)} must export default a function (kit) => steps`);
    factories.push(mod.default);
  }
  return factories;
}
