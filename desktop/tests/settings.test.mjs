import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  EMPTY_STAGING, INITIAL_NAV, REDACTED, applyStaged, choiceOptions, currentRow, currentSection, cycleValueJson,
  edgeForOptions, goToSection, isDirty, isSecretField, moveRow, moveSection, nextMatch, optionText, overlayKeyFor, paneKey,
  parseTextValue, pendingCount, pendingUpdates, prune, redactConfig, rowValue, searchRows, sectionRows, stage,
  stepIndex, stepValue, viewValue, widgetFor,
} from '../dist/settings-model.js';
import { DAEMON_WIDGETS, SETTINGS_GROUPS, SETTINGS_ROWS, SETTINGS_SECTIONS } from '../dist/settings.generated.js';

/**
 * A fake daemon behind the SettingsClient interface: it keeps a config,
 * answers GetDaemonConfig and applies UpdateDaemonConfig {updates} all-or-none
 * (refusing an unknown field), and records every call.
 */
function fakeDaemon(initial) {
  let config = { ...initial };
  const calls = [];
  return {
    calls,
    get config() { return config; },
    async getConfig() { calls.push(['GetDaemonConfig']); return { ...config }; },
    async update(updates) {
      calls.push(['UpdateDaemonConfig', updates.map((u) => ({ ...u }))]);
      const unknown = updates.find((u) => !(u.field in config));
      if (unknown) throw new Error(`daemon error -32602: unknown field ${unknown.field}`);
      config = { ...config, ...Object.fromEntries(updates.map((u) => [u.field, u.value])) };
      return null;
    },
  };
}

/** A pane row by registry id or by the daemon field it edits. */
const row = (key) => {
  const spec = SETTINGS_ROWS.find((r) => r.id === key) ?? SETTINGS_ROWS.find((r) => r.owner.args?.[0] === key);
  assert.ok(spec, `no row ${key}`);
  return sectionRows(spec.section, {}).find((r) => r.id === spec.id);
};

test('the registry mirrors the TUI: 8 groups, every section in a group, rows in known sections', () => {
  assert.deepEqual(SETTINGS_GROUPS.map((g) => g.label), [
    'APPEARANCE', 'WORKSPACE', 'MODELS', 'SAFETY & SPEND', 'AGENT AUTOMATION', 'PROVIDERS & SANDBOXES', 'HOST & BUILD', 'INTEGRATIONS',
  ]);
  for (const s of SETTINGS_SECTIONS) assert.ok(SETTINGS_GROUPS.some((g) => g.id === s.group), s.id);
  assert.equal(SETTINGS_SECTIONS[0].label, 'Theme & Colors');
  assert.ok(SETTINGS_ROWS.length > 150);
  for (const r of SETTINGS_ROWS) assert.ok(SETTINGS_SECTIONS.some((s) => s.id === r.section), r.id);
  // Every daemon-owned row has the TUI's widget for its field.
  for (const r of SETTINGS_ROWS.filter((x) => x.owner.kind === 'daemon')) assert.ok(DAEMON_WIDGETS[r.owner.args[0]], r.id);
});

test('round trip against a fake daemon: read, stage, apply, re-read', async () => {
  const daemon = fakeDaemon({ retry_enabled: false, session_retention_window_hours: 24, context_rotation_global_pct: null });
  // Read.
  let config = await daemon.getConfig();
  let st = EMPTY_STAGING;
  // Stage: flip a switch, step a choice twice, and step back once.
  const retry = row('retry_enabled');
  st = stage(st, 'retry_enabled', config.retry_enabled, stepValue(retry, rowValue(retry, config, st), 'activate'));
  const hours = row('session_retention_window_hours');
  for (const step of ['next', 'next', 'previous']) {
    st = stage(st, 'session_retention_window_hours', config.session_retention_window_hours, stepValue(hours, rowValue(hours, config, st), step));
  }
  assert.equal(pendingCount(st, config), 2);
  assert.equal(viewValue(st, 'session_retention_window_hours', 24), 48);
  // Many keypresses send nothing.
  assert.deepEqual(daemon.calls, [['GetDaemonConfig']]);
  // Apply: ONE update carrying every final value, then a re-read.
  const out = await applyStaged(daemon, st, config);
  assert.equal(out.error, null);
  assert.deepEqual(daemon.calls, [
    ['GetDaemonConfig'],
    // In page order: Workspace › Session List comes before Agent Automation › Retries.
    ['UpdateDaemonConfig', [{ field: 'session_retention_window_hours', value: 48 }, { field: 'retry_enabled', value: true }]],
    ['GetDaemonConfig'],
  ]);
  config = out.config;
  assert.equal(config.retry_enabled, true);
  assert.equal(config.session_retention_window_hours, 48);
  assert.equal(pendingCount(out.staging, config), 0);
  assert.match(out.message, /^Applied 2 settings: Archive idle hours → 48; .+ → true$/);
});

test('a refused apply changes nothing and keeps every row pending', async () => {
  const daemon = fakeDaemon({ retry_enabled: false });
  const config = await daemon.getConfig();
  let st = stage(EMPTY_STAGING, 'retry_enabled', false, true);
  st = stage(st, 'not_a_field', undefined, 3);
  const out = await applyStaged(daemon, st, config);
  assert.match(out.error, /none of .* were applied: .*unknown field not_a_field/);
  assert.equal(out.staging, st);
  assert.equal(daemon.config.retry_enabled, false);
  assert.equal(pendingCount(out.staging, out.config), 2);
});

test('nothing staged sends nothing', async () => {
  const daemon = fakeDaemon({ retry_enabled: false });
  const out = await applyStaged(daemon, EMPTY_STAGING, { retry_enabled: false });
  assert.equal(out.message, 'No pending settings changes');
  assert.deepEqual(daemon.calls, []);
});

test('a restart-class field says when it applies', async () => {
  const daemon = fakeDaemon({ queue_enabled: false });
  const out = await applyStaged(daemon, stage(EMPTY_STAGING, 'queue_enabled', false, true), { queue_enabled: false });
  assert.match(out.message, /· applies after daemon restart$/);
});

test('staging back to the live value unstages; a refresh that lands on it settles it', () => {
  let st = stage(EMPTY_STAGING, 'retry_enabled', false, true);
  assert.equal(isDirty(st, 'retry_enabled', false), true);
  st = stage(st, 'retry_enabled', false, false);
  assert.equal(st.staged.size, 0);
  st = stage(EMPTY_STAGING, 'retry_enabled', false, true);
  assert.equal(prune(st, { retry_enabled: true }).staged.size, 0);
  assert.deepEqual(pendingUpdates(st, { retry_enabled: true }), []);
});

test('numeric choices clamp, named choices wrap (settings_staging.rs)', () => {
  assert.equal(edgeForOptions(['Default', '1', '2']), 'clamp');
  assert.equal(edgeForOptions(['read-only', 'workspace-write']), 'wrap');
  assert.equal(stepIndex(2, 3, true, 'clamp'), 2);
  assert.equal(stepIndex(0, 3, false, 'clamp'), 0);
  assert.equal(stepIndex(2, 3, true, 'wrap'), 0);
  assert.equal(stepIndex(0, 3, false, 'wrap'), 2);
  assert.equal(stepIndex(-1, 3, true, 'clamp'), 0);
  assert.equal(stepIndex(-1, 3, false, 'clamp'), 2);
  const hours = row('session_retention_window_hours');
  assert.equal(stepValue(hours, 720, 'next'), 720);
  assert.equal(stepValue(hours, 1, 'previous'), 1);
  const rot = row('context_rotation_global_pct');
  assert.equal(stepValue(rot, null, 'next'), 1);
  assert.equal(stepValue(rot, 1, 'previous'), null);
  const mode = row('editing_mode');
  assert.deepEqual(mode.widget, { type: 'choice', options: ['unset', 'standard', 'vim'] });
  assert.equal(stepValue(mode, 'vim', 'next'), 'unset');
});

test('a live number the options lack is merged in order', () => {
  assert.deepEqual(choiceOptions(['1', '6', '24'], 12), ['1', '6', '12', '24']);
  assert.deepEqual(choiceOptions(['a', 'b'], 'c'), ['a', 'b']);
});

test('choice values are sent as the TUI sends them', () => {
  assert.equal(cycleValueJson('Default'), null);
  assert.equal(cycleValueJson('48'), 48);
  assert.equal(cycleValueJson('0.75'), 0.75);
  assert.equal(cycleValueJson('workspace-write'), 'workspace-write');
  assert.equal(optionText(null), 'Default');
  assert.equal(optionText(true), 'true');
});

test('widgets: switches, choices, text editors, read-only and partial rows', () => {
  assert.equal(row('retry_enabled').widget.type, 'switch');
  assert.equal(row('session_retention_window_hours').widget.type, 'choice');
  assert.equal(row('cold_archive_dir').widget.type, 'text-editor');
  assert.equal(row('built_in_theme').widget.type, 'local');
  assert.equal(row('theme_roles').widget.type, 'local');
  assert.equal(row('cloud_spend_status').widget.type, 'display');
  // Gap sweep G4: the slots row opens the admin Provider keys overlay (values stay write-only there).
  assert.deepEqual(row('provider_credential_slots').widget, { type: 'local', local: 'provider-keys' });
  assert.deepEqual(row('budget_policies').widget, { type: 'local', local: 'budgets' });
  assert.equal(row('model_control_mode').widget.type, 'choice');
  assert.equal(row('submit_on_enter').widget.type, 'tui-only');
  // A choice the TUI builds at run time is typed as text.
  const profile = SETTINGS_ROWS.find((r) => r.owner.args?.[0] === 'provider_profile');
  assert.deepEqual(widgetFor(profile, 'all'), { type: 'text', valueType: 'string' });
});

test('the Vim keys toggle lives in Workspace › Editing Mode', () => {
  const rows = sectionRows('editing_mode', {});
  assert.deepEqual(rows.map((r) => r.label), ['Editing mode', 'Vim keys (this desktop)']);
  assert.deepEqual(rows[1].widget, { type: 'local', local: 'vim-keys' });
});

test('text values: numbers are parsed, editors trim, oversize is refused', () => {
  const num = { widget: { type: 'text', valueType: 'number' } };
  assert.deepEqual(parseTextValue(num, ' 12 '), { ok: true, value: 12 });
  assert.equal(parseTextValue(num, 'x').ok, false);
  const ed = row('claude_max_budget_usd');
  assert.deepEqual(parseTextValue(ed, ' 2.50 '), { ok: true, value: '2.50' });
  assert.deepEqual(parseTextValue(ed, ''), { ok: true, value: '' });
  assert.equal(parseTextValue(ed, 'x'.repeat(70000)).ok, false);
});

test('credential values are never shown', () => {
  for (const f of ['openrouter_api_key', 'vault.secret', 'github_token', 'bridge_password']) assert.ok(isSecretField(f), f);
  for (const f of ['openrouter_context_budget_tokens', 'coordinator_context_cap_tokens', 'vault.env_compat', 'retry_enabled']) assert.ok(!isSecretField(f), f);
  // No field the TUI's settings page edits looks like a credential.
  for (const field of Object.keys(DAEMON_WIDGETS)) assert.ok(!isSecretField(field), field);
  const out = redactConfig({ some_api_key: 'sk-1', harness_searxng_url: 'https://u:p@h/x', ok: 1 });
  assert.equal(out.some_api_key, REDACTED);
  assert.equal(out.harness_searxng_url, `https://${REDACTED}@h/x`);
  assert.equal(out.ok, 1);
});

test('navigation: rail, tabs wrap inside a group, rows clamp, remembered per section', () => {
  let nav = INITIAL_NAV;
  assert.equal(currentSection(nav).id, 'theme_colors');
  nav = moveSection(nav, 1);
  assert.equal(currentSection(nav).id, 'screen');
  assert.equal(nav.focus, 'items');
  nav = moveSection(nav, 1);
  assert.equal(currentSection(nav).id, 'theme_colors');
  nav = moveRow(nav, 99, 3);
  assert.equal(currentRow(nav, 3), 2);
  nav = goToSection(nav, 'claude_hooks');
  assert.equal(currentSection(nav).id, 'claude_hooks');
  assert.equal(SETTINGS_GROUPS[nav.group].id, 'providers_and_sandboxes');
});

test('pane keys follow the TUI table', () => {
  const items = { ...INITIAL_NAV, focus: 'items' };
  assert.equal(paneKey(INITIAL_NAV, 'j', false, 3).nav.group, 1);
  assert.equal(paneKey(INITIAL_NAV, 'l', false, 3).nav.focus, 'items');
  assert.equal(paneKey(items, 'h', false, 3).nav.focus, 'rail');
  assert.deepEqual(paneKey(items, 'Enter', false, 3), { kind: 'activate' });
  assert.deepEqual(paneKey(items, ' ', false, 3), { kind: 'activate' });
  assert.deepEqual(paneKey(items, '+', false, 3), { kind: 'step', step: 'next' });
  assert.deepEqual(paneKey(items, '-', false, 3), { kind: 'step', step: 'previous' });
  assert.deepEqual(paneKey(items, 'w', false, 3), { kind: 'apply' });
  assert.deepEqual(paneKey(items, 'R', false, 3), { kind: 'refresh' });
  assert.deepEqual(paneKey(items, 'Escape', false, 3), { kind: 'close' });
  assert.deepEqual(paneKey(items, 'q', false, 3), { kind: 'close' });
  assert.deepEqual(paneKey(items, '/', false, 3), { kind: 'search' });
  assert.equal(currentSection(paneKey(items, 'Tab', true, 3).nav).id, 'screen');
  assert.deepEqual(paneKey(items, 'a', false, 3), { kind: 'item', op: 'add' });
  assert.equal(paneKey(items, 'e', false, 3).kind, 'missing', 'e is the skills row overlay key, nothing on other rows');
});

test('a row overlay runs its own keys before the pane list keys', () => {
  const items = { ...INITIAL_NAV, focus: 'items' };
  const hooks = { a: 'Add hook' };
  assert.equal(overlayKeyFor(paneKey(items, 'a', false, 3), hooks), 'a');
  assert.equal(overlayKeyFor(paneKey(items, 'd', false, 3), hooks), null, 'd stays the pane list key');
  assert.equal(overlayKeyFor(paneKey(items, 'a', false, 3), undefined), null, 'Budgets has no overlay: the pane adds');
  assert.equal(overlayKeyFor(paneKey(items, 's', false, 3), { s: 'Set' }), 's');
  assert.equal(overlayKeyFor(paneKey(items, 'e', false, 3), { e: 'Toggle' }), 'e');
  assert.equal(overlayKeyFor(paneKey(items, '>', false, 3), { '>': 'x' }), null, 'resize is the pane');
});

test('search needs every term and wraps', () => {
  const hits = searchRows('retry stall');
  assert.ok(hits.length >= 1);
  const labels = hits.map((m) => sectionRows(m.section, {})[m.row].label);
  assert.ok(labels.includes('Retry on stall'), labels.join(', '));
  assert.deepEqual(searchRows('   '), []);
  const all = searchRows('theme');
  const last = all[all.length - 1];
  assert.equal(nextMatch(all, last, 1).wrapped, true);
  assert.deepEqual(nextMatch(all, last, 1).match, all[0]);
});
