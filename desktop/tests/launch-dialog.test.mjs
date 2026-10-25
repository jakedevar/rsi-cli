// Local #211: the New session dialog's plain wording. Pure strings are pinned from the built
// module; the static markup and the submit path are pinned by reading their sources.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  EFFORT_HINT, FOLDER_HINT, MODEL_HINT, PROMPT_PLACEHOLDER, PROVIDER_INFO, launchedName, launchedToast, providerDescription, providerLabel,
} from '../dist/launch-model.js';

const read = (rel) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const html = read('../static/index.html');
const launchSrc = read('../src/launch.ts');

/** Every `Session.provider` (rsi-common `SessionProvider::CANONICAL_NAMES`) and its label. */
const EXPECTED = {
  Claude: 'Claude Code (Anthropic CLI)',
  Codex: 'Codex CLI',
  Pioneer: 'Pioneer',
  OpenRouter: 'OpenRouter',
  Bedrock: 'Amazon Bedrock',
  Local: 'Local model',
  Antigravity: 'Google Antigravity CLI',
  CodexAppServer: 'Codex app server',
  Harness: 'Direct API (Harness)',
};

test('providerLabel names every Session.provider value, and each has a description', () => {
  for (const [provider, label] of Object.entries(EXPECTED)) {
    assert.equal(providerLabel(provider), label, provider);
    assert.ok(providerDescription(provider).length > 20, `${provider} has a one-line description`);
  }
  assert.deepEqual(Object.keys(PROVIDER_INFO).sort(), Object.keys(EXPECTED).sort(), 'no provider missing or extra');
  // The enum list in the daemon is the source of truth: a provider added there fails here.
  const types = read('../../crates/rsi-common/src/types.rs');
  const names = /CANONICAL_NAMES: &'static \[&'static str\] = &\[([^\]]*)\]/.exec(types)?.[1] ?? '';
  const daemon = [...names.matchAll(/"(\w+)"/g)].map((m) => m[1]);
  assert.deepEqual([...daemon].sort(), Object.keys(EXPECTED).sort());
});

test('an unknown provider keeps its raw name and has no description', () => {
  assert.equal(providerLabel('Mystery'), 'Mystery');
  assert.equal(providerDescription('Mystery'), '');
});

test('the dialog explains Effort, Folder and Prompt in the markup', () => {
  assert.equal(EFFORT_HINT, 'How hard the model thinks: higher is slower and costs more. Default follows Settings › Models.');
  assert.equal(FOLDER_HINT, 'Blank: the project’s folder');
  assert.equal(PROMPT_PLACEHOLDER, 'Describe the task…');
  assert.ok(html.includes(`<label title="${EFFORT_HINT}">Effort`), 'Effort label tooltip');
  assert.ok(html.includes(`<label title="${MODEL_HINT}">Model`), 'Model label tooltip');
  assert.ok(html.includes('>Folder\n          <span class="folder-field"><input name="working_dir"'), 'the label says Folder');
  assert.ok(html.includes(`placeholder="${FOLDER_HINT}"`), 'the Folder hint');
  assert.ok(html.includes(`placeholder="${PROMPT_PLACEHOLDER}"`), 'the prompt placeholder');
  assert.ok(!html.includes('Working dir<'), 'no raw Working dir label left');
});

test('Provider options show the friendly name but submit the exact enum string', () => {
  assert.match(launchSrc, /o\.value = provider;/, 'option value is the raw provider');
  assert.match(launchSrc, /el\('option', undefined, providerLabel\(provider\)\)/, 'option text is the label');
  // The submitted request still reads the select's value and the raw effort string.
  assert.match(launchSrc, /provider: str\('provider'\),/);
  assert.match(launchSrc, /effort: str\('effort'\) \|\| undefined,/);
  // The Model field stays free text with a list button for the shared dropdown.
  assert.ok(html.includes('<input name="model" placeholder="provider default">'));
  assert.ok(html.includes('id="launch-model-pick"'));
  assert.match(launchSrc, /pickLaunchModel\(\)/);
  // The launch default still selects by value, so it keeps working with friendly text.
  assert.match(read('../src/actions/nav-run.ts'), /o\.defaultSelected = o\.value === d\.provider/);
});

test('the success toast says where the session is, and the new session is selected', () => {
  assert.equal(launchedToast({ title: 'Fix the login bug' }, 'x', false), 'Session launched: Fix the login bug');
  assert.equal(launchedToast({ session: { title: 'Nested' } }, 'x', false), 'Session launched: Nested');
  assert.equal(launchedToast({ id: 'abc' }, 'Add a README\nwith more detail', false), 'Session launched: Add a README');
  assert.equal(launchedToast(null, 'Do it', true), 'Session launched in its own git worktree: Do it');
  assert.equal(launchedToast(null, '   ', false), 'Session launched');
  const long = 'x'.repeat(100);
  assert.equal(launchedName(null, long).length, 60);
  assert.ok(launchedName(null, long).endsWith('…'));
  assert.match(launchSrc, /refreshSessions\(\)\.then\(\(\) => \{ if \(id\) select\(id\); \}\)/);
});

test('Ctrl/Cmd+Enter still submits and the Launch button is not disabled for a blank folder', () => {
  // rsid accepts a launch with no project and no folder, so only the saved-grant check blocks it.
  assert.match(launchSrc, /const blocked = launchBlocked\(\);/);
  assert.ok(!/working_dir[^\n]*disabled/.test(launchSrc));
  assert.match(read('../src/prompts.ts'), /sendChord\(/);
});
