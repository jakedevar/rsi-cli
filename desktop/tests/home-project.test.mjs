// The built-in RSI home project (hub #3315) in the desktop (desktop #319, audit 4 finding 2). The pure rules
// (project-model.ts) run from the built module; the places that must use them are pinned by reading their
// sources, as the repo does for DOM paths a unit test cannot reach. The DOM behaviour of the New session
// dialog and the project filter is in launch-project-folder.test.mjs; the first-run card in a browser is
// the connected-no-projects scenario of smoke-firstrun.mjs (onboarding.test.mjs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  HOME_LOCKED_WHY, HOME_PROJECT_ID, HOME_PROJECT_LABEL, HOME_WORKTREE_WHY, isHomeProjectId, ownProjects, projectLabel, sortProjects,
} from '../dist/project-model.js';
import { onboardingView } from '../dist/onboarding-model.js';

const src = (rel) => readFileSync(fileURLToPath(new URL(`../src/${rel}`, import.meta.url)), 'utf8');
const HOME = { id: HOME_PROJECT_ID, name: 'RSI home' };
const A = { id: 'a', name: 'Alpha' };
const B = { id: 'b', name: 'Beta' };

test('the fixed id is rsid\'s Project::HOME_ID', () => {
  assert.equal(HOME_PROJECT_ID, '00000000-0000-4000-8000-727369686f6d');
  const types = readFileSync(fileURLToPath(new URL('../../crates/rsi-common/src/types.rs', import.meta.url)), 'utf8');
  assert.ok(types.includes('Uuid::from_u128(0x0000_0000_0000_4000_8000_7273_6968_6f6d)'), 'the Rust constant still has this value');
});

test('own projects leave RSI home out; sorting puts it last and keeps rsid\'s order otherwise', () => {
  assert.deepEqual(ownProjects([HOME, A, B]), [A, B]);
  assert.deepEqual(sortProjects([HOME, A, B]), [A, B, HOME]);
  assert.deepEqual(sortProjects([B, A]), [B, A]);
  assert.deepEqual(sortProjects([HOME]), [HOME]);
  assert.equal(isHomeProjectId(HOME_PROJECT_ID), true);
  assert.equal(isHomeProjectId('a'), false);
  assert.equal(isHomeProjectId(null), false);
});

test('RSI home says what it is in a filter or picker; other projects keep their name', () => {
  assert.equal(projectLabel(HOME), 'RSI home (built-in home project)');
  assert.equal(projectLabel(HOME), HOME_PROJECT_LABEL);
  assert.equal(projectLabel(A), 'Alpha');
});

test('a fresh rsid (RSI home is its only project) still gets "Create your first project"', () => {
  const link = { kind: 'connected' };
  const fresh = onboardingView(link, 0, 'mac', ownProjects([HOME]).length, false);
  assert.equal(fresh.id, 'no-projects');
  assert.equal(fresh.title, 'Welcome. Start by creating a project.');
  assert.equal(fresh.buttons[0].label, 'Create your first project');
  const withOwn = onboardingView(link, 0, 'mac', ownProjects([HOME, A]).length, false);
  assert.equal(withOwn.id, 'no-sessions', 'behaviour with a real project is unchanged');
});

test('the first-run, Issues, manager, graph and launch decisions count only the person\'s own projects', () => {
  assert.match(src('boot.ts'), /onboardingView\(link, state\.sessions\.length, currentPlatform\(\), ownProjects\(state\.projects\)\.length/);
  const issues = src('issues.ts');
  assert.match(issues, /hasProject: ws\.projectId !== null \|\| ownProjects\(state\.projects\)\.length > 0/);
  assert.match(issues, /sel\.replaceChildren\(\.\.\.ownProjects\(state\.projects\)\.map/, 'the Issues project picker leaves RSI home out (home_project_issue_refused)');
  assert.match(issues, /\?\? own\[0\]\?\.id \?\? null/, 'and never defaults to it');
  const managers = src('managers.ts');
  assert.match(managers, /hasProject: ownProjects\(state\.projects\)\.length > 0/);
  assert.match(managers, /\?\? own\[0\]\?\.id \?\? null/);
  assert.match(managers, /proj\.replaceChildren\(\.\.\.ownProjects\(state\.projects\)/);
  assert.match(src('graphs.ts'), /ownProjects\(state\.projects\)\[0\]\?\.id/);
  assert.match(src('manager-launch.ts'), /!ownProjects\(projects\)\.length/);
  assert.match(src('launch-model.ts'), /const own = ownProjects\(i\.projects\)/);
  assert.match(src('launch.ts'), /ownProjects\(state\.projects\)\.length > 0\) \{ hint\?\.remove\(\)/, 'the New session no-project hint shows with only RSI home');
});

test('the project list is stored sorted and every filter and picker label goes through projectLabel', () => {
  assert.match(src('sessions.ts'), /state\.projects = sortProjects\(await api\.listProjects\(\)\)/);
  assert.match(src('sessions.ts'), /el\('option', undefined, projectLabel\(p\)\)/);
  assert.match(src('launch.ts'), /el\('option', undefined, projectLabel\(p\)\)/);
  assert.match(src('actions/sessionlist-run.ts'), /label: projectLabel\(p\)/);
});

test('Edit and Delete on RSI home are refused with rsid\'s reason in words, and no worktree is offered there', () => {
  assert.match(HOME_LOCKED_WHY, /built in/);
  assert.match(HOME_WORKTREE_WHY, /home folder/);
  const list = src('actions/sessionlist-run.ts');
  assert.match(list, /function openProjectForm[^]*?if \(p && isHomeProjectId\(p\.id\)\) \{ toast\(HOME_LOCKED_WHY\); return; \}/);
  assert.match(list, /async function deleteProject[^]*?if \(isHomeProjectId\(p\.id\)\) \{ toast\(HOME_LOCKED_WHY\); return; \}/);
  const prompts = src('prompts.ts');
  assert.match(prompts, /sandbox: sandbox && !homeChosen\(\)/, 'the settings side reads Own git worktree as off');
  assert.match(prompts, /return \{ sandbox: sandbox && !homeChosen\(\) \}/, 'and the launch never sends it');
  assert.match(prompts, /if \(homeChosen\(\)\) \{ toast\(HOME_WORKTREE_WHY\); return; \}/, 'the key does not turn it on');
  assert.match(prompts, /box\.disabled = true;\s*box\.title = HOME_WORKTREE_WHY/, 'the checkbox is disabled with the reason');
});
