// Local #125 (hub #3009): every rsid-backed Settings row has a real desktop
// control; the TUI-only preferences are named and documented. The model-control
// mode applies through its own RPC after the config batch, as the TUI does.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  EMPTY_STAGING, MODE_FIELD, OVERLAY_ROW_IDS, TUI_ONLY_WHY, applyStaged, pendingUpdates, sectionRows, stage, stepValue, widgetFor,
} from '../dist/settings-model.js';
import { checkTuiOnly } from '../scripts/parity-audit-lib.mjs';
import { SETTINGS_ROWS } from '../dist/settings.generated.js';
import {
  MODEL_CONTROL_MODES, allowedWork, budgetAlerts, cancelMessage, deniedInvocations, formatBinaryBytes, liveInvocations, modeMessage,
  modeOfStatus, modeSlug, poolSummary, reclaimMessage, storageSummary, usageOverview, usagePerModel, usageRowValue, workTime,
} from '../dist/settings-state-model.js';

const here = dirname(fileURLToPath(import.meta.url));
const desktop = join(here, '..');
const src = (rel) => readFileSync(join(desktop, rel), 'utf8');

/** The 15 rows the Issue lists as TUI preferences in the TUI's own state.json. */
const TUI_ONLY = [
  'formulation_animation', 'formulation_speed', 'activity_indicator', 'detail_column', 'navigator_preset', 'navigator_columns',
  'card_fields', 'show_system_events', 'show_thinking_events', 'hide_tool_results', 'submit_on_enter', 'auto_open_question_panel',
  'prompt_compiler', 'default_model',
];

const widgetOf = (id) => widgetFor(SETTINGS_ROWS.find((r) => r.id === id), undefined);

test('no registry row is a placeholder: each is a control, an overlay or a documented TUI-only preference', () => {
  const partial = SETTINGS_ROWS.filter((r) => widgetFor(r, undefined).type === 'partial').map((r) => r.id);
  assert.deepEqual(partial, [], 'a "not in the desktop yet" placeholder is left');
  const tuiOnly = SETTINGS_ROWS.filter((r) => widgetFor(r, undefined).type === 'tui-only').map((r) => r.id);
  assert.deepEqual([...tuiOnly].sort(), [...TUI_ONLY].sort());
  for (const id of tuiOnly) {
    assert.equal(SETTINGS_ROWS.find((r) => r.id === id).owner.kind, 'tui', `${id} must be a TUI-owned row`);
    assert.equal(widgetOf(id).why, TUI_ONLY_WHY);
  }
});

test('parity-audit.json documents exactly the TUI-only rows, each a TUI-local preference with no rsid state', () => {
  const audit = JSON.parse(src('parity-audit.json'));
  const doc = audit.tuiOnlyRows;
  assert.ok(doc, 'parity-audit.json needs a tuiOnlyRows section');
  assert.match(doc.reason, /state\.json/);
  assert.match(doc.reason, /rsid has no state/i);
  const tuiOnly = SETTINGS_ROWS.filter((r) => widgetFor(r, undefined).type === 'tui-only').map((r) => r.id);
  assert.deepEqual(Object.keys(doc.rows).sort(), [...tuiOnly].sort());
  for (const [id, why] of Object.entries(doc.rows)) assert.ok(typeof why === 'string' && why.length > 10, `${id} needs its reason`);
});

test('every overlay row is registered by a desktop view over its rsid operation', () => {
  const files = ['src/settings-state.ts', 'src/actions/operatorconfig.ts', 'src/actions/promptcreator-run.ts'].map(src).join('\n');
  for (const id of OVERLAY_ROW_IDS) {
    const kind = id.match(/^(signal|imessage)_bridge$/)?.[1];
    if (kind) { assert.ok(files.includes('`${kind}_bridge`') && files.includes(`'${kind}'`), `${id} is not registered`); continue; }
    assert.ok(files.includes(`'${id}'`), `${id} has no registerRowOverlay`);
  }
  const state = src('src/settings-state.ts');
  for (const id of OVERLAY_ROW_IDS) {
    if (/^(signal|imessage)_bridge$|^claude_(hooks|skills)$|^api_providers$/.test(id)) continue;
    assert.ok(state.includes(`'${id}'`), `${id} is a daemon-state row settings-state.ts must register`);
  }
  // Every registry row that is an overlay row exists in the registry.
  const ids = new Set(SETTINGS_ROWS.map((r) => r.id));
  for (const id of OVERLAY_ROW_IDS) assert.ok(ids.has(id), `${id} is not a registry row`);
});

test('the row placeholder wording is gone from the model', () => {
  const model = src('src/settings-model.ts');
  assert.ok(!model.includes('does not call yet'));
  assert.ok(!model.includes('not in the desktop yet'));
  assert.ok(!src('src/settings.ts').includes("'Not in the desktop yet'"));
});

test('the mode row is a choice over the TUI cycle and the Rust side maps the same slugs', () => {
  const w = widgetOf('model_control_mode');
  assert.equal(w.type, 'choice');
  assert.deepEqual([...w.options], [...MODEL_CONTROL_MODES]);
  const rust = src('src-tauri/src/settings_state.rs');
  for (const slug of MODEL_CONTROL_MODES) assert.ok(rust.includes(`("${slug}", "${slug.replace(/-/g, '_')}")`), `${slug} is not in the Rust table`);
  const rows = sectionRows('model_control', {});
  const row = rows.find((r) => r.id === 'model_control_mode');
  assert.equal(row.field, MODE_FIELD);
  assert.equal(stepValue(row, 'normal', 'next'), 'pause-background');
  assert.equal(stepValue(row, 'stop-all', 'next'), 'normal', 'enum lists wrap');
});

test('every new Tauri command is registered and takes the arguments the TS side sends', () => {
  const rust = src('src-tauri/src/settings_state.rs');
  const gen = src('src-tauri/src/handlers.generated.rs');
  for (const cmd of ['model_control_overview', 'set_model_control_mode', 'usage_stats', 'cancel_model_invocation', 'sandbox_storage_status', 'target_pool_status', 'sandbox_cache_reclaim']) {
    assert.ok(rust.includes(`pub async fn ${cmd}`), `${cmd} missing in Rust`);
    assert.ok(gen.includes(cmd), `${cmd} is not in the generated invoke handler (npm run build regenerates it)`);
  }
  const ts = src('src/settings-state.ts') + src('src/settings-api.ts');
  assert.match(ts, /cancel_model_invocation', \{ invocationId/);
  assert.match(ts, /sandbox_cache_reclaim', \{ dryRun/);
  assert.match(ts, /set_model_control_mode', \{ mode/);
});

// ---------- apply: the mode goes through its own RPC ----------

function fake({ failUpdate = false, failMode = false, withMode = true } = {}) {
  const calls = [];
  let config = { retry_enabled: false, [MODE_FIELD]: 'normal' };
  const client = {
    async getConfig() { calls.push(['GetDaemonConfig']); return { ...config }; },
    async update(updates) {
      calls.push(['UpdateDaemonConfig', updates.map((u) => ({ ...u }))]);
      if (failUpdate) throw new Error('daemon error -32602: refused');
      config = { ...config, ...Object.fromEntries(updates.map((u) => [u.field, u.value])) };
      return null;
    },
  };
  if (withMode) {
    client.setModelControlMode = async (slug) => {
      calls.push(['UpdateModelControlPolicy', slug]);
      if (failMode) throw new Error('daemon error: mode refused');
      config = { ...config, [MODE_FIELD]: slug };
      return { previous_mode: 'normal', current_mode: slug.replace(/-/g, '_') };
    };
  }
  return { calls, client, get config() { return config; } };
}

test('applying only the mode sends UpdateModelControlPolicy and no UpdateDaemonConfig', async () => {
  const d = fake();
  const st = stage(EMPTY_STAGING, MODE_FIELD, 'normal', 'deny-paid');
  assert.deepEqual(pendingUpdates(st, d.config), [{ field: MODE_FIELD, value: 'deny-paid' }]);
  const out = await applyStaged(d.client, st, d.config);
  assert.deepEqual(d.calls.map((c) => c[0]), ['UpdateModelControlPolicy', 'GetDaemonConfig']);
  assert.deepEqual(d.calls[0], ['UpdateModelControlPolicy', 'deny-paid']);
  assert.equal(out.error, null);
  assert.deepEqual(out.applied, [MODE_FIELD]);
  assert.equal(out.message, 'Model control mode: deny-paid');
  assert.equal(out.staging.staged.size, 0);
  assert.equal(out.config[MODE_FIELD], 'deny-paid');
});

test('a mode and a config row apply together: the batch first, then the mode', async () => {
  const d = fake();
  let st = stage(EMPTY_STAGING, MODE_FIELD, 'normal', 'local-only');
  st = stage(st, 'retry_enabled', false, true);
  const out = await applyStaged(d.client, st, d.config);
  assert.deepEqual(d.calls.map((c) => c[0]).filter((c) => c !== 'GetDaemonConfig'), ['UpdateDaemonConfig', 'UpdateModelControlPolicy']);
  assert.deepEqual(d.calls.find((c) => c[0] === 'UpdateDaemonConfig')[1], [{ field: 'retry_enabled', value: true }], 'the mode is not in the batch');
  assert.equal(out.error, null);
  assert.equal(out.staging.staged.size, 0);
  assert.match(out.message, /Retry on failure|retry_enabled/);
  assert.match(out.message, /Model control mode: local-only/);
});

test('a refused mode stays pending alone while the batch is applied; a refused batch does not stop the mode', async () => {
  const a = fake({ failMode: true });
  let st = stage(EMPTY_STAGING, MODE_FIELD, 'normal', 'stop-all');
  st = stage(st, 'retry_enabled', false, true);
  const out = await applyStaged(a.client, st, a.config);
  assert.ok(out.error?.includes('mode refused'), out.error);
  assert.deepEqual([...out.staging.staged.keys()], [MODE_FIELD]);
  assert.deepEqual(out.applied, ['retry_enabled']);

  const b = fake({ failUpdate: true });
  const out2 = await applyStaged(b.client, st, b.config);
  assert.ok(out2.error?.includes('none of'), out2.error);
  assert.deepEqual(out2.applied, [MODE_FIELD]);
  assert.deepEqual([...out2.staging.staged.keys()], ['retry_enabled']);
});

test('a client that cannot set the mode refuses it and keeps it pending', async () => {
  const d = fake({ withMode: false });
  const st = stage(EMPTY_STAGING, MODE_FIELD, 'normal', 'deny-paid');
  const out = await applyStaged(d.client, st, d.config);
  assert.ok(out.error?.includes('cannot set the mode'));
  assert.equal(out.staging.staged.size, 1);
  assert.ok(!d.calls.some((c) => c[0] === 'UpdateModelControlPolicy'));
});

// ---------- wording ----------

test('mode helpers follow the TUI slugs', () => {
  assert.equal(modeSlug('pause_background'), 'pause-background');
  assert.equal(modeSlug('nope'), null);
  assert.equal(modeOfStatus({ mode: 'stop_all' }), 'stop-all');
  assert.equal(modeOfStatus(null), null);
  assert.equal(allowedWork('local-only'), 'Local models only');
  assert.equal(modeMessage({ current_mode: 'deny_paid' }, 'x'), 'Model control mode: deny-paid');
});

test('sandbox storage, reclaim and pool lines are the TUI strings', () => {
  const report = { candidates_considered: 7, eligible_candidates: 3, filesystem_before: { used_percent: 61, available_bytes: 5 * 1024 ** 3 },
    fully_removed_count: 2, newly_staged_count: 1, pending_count: 4, reclaimed_bytes: 3 * 1024 ** 2 };
  assert.equal(storageSummary(report), '7 checked · 3 eligible · 61% used · 5.0 GiB free · capacity estimate unavailable');
  assert.equal(storageSummary({ report }), storageSummary(report), 'a v2 wrapper is unwrapped');
  assert.equal(reclaimMessage(report, true), 'Preview: 3 eligible of 7 checked; capacity estimate unavailable');
  assert.equal(reclaimMessage({ report }, false), 'Reclaim: 2 removed, 1 staged, 4 pending, 3.0 MiB measured free');
  assert.equal(formatBinaryBytes(12), '12 B');
  assert.equal(poolSummary({ pools: 2, total_bytes: 22 * 2 ** 30, newest: { sha: '0123456789abcdef', age_commits: 3, degraded: false }, failed_newer: 1 }),
    '01234567 3 commits behind · 2 pool(s), 22.0 GiB · 1 newer build(s) failed');
  assert.equal(poolSummary({ pools: 0, total_bytes: 0 }), 'No pool yet');
  assert.equal(poolSummary({ pools: 1, total_bytes: 2 ** 30, newest: { sha: 'abcdef0123', age_commits: 0, degraded: true } }), 'abcdef01 (libraries only) at tip · 1 pool(s), 1.0 GiB');
  assert.equal(poolSummary({ pools: 0, total_bytes: 0, last_reclaim: { reason: 'pool_reclaimed_for_disk_floor', removed: 2 } }),
    'No pool yet · pool_reclaimed_for_disk_floor: 2 pool(s)');
  assert.match(storageSummary(null), /^Unavailable/);
});

test('usage rows list spend, live work and only a running invocation can be cancelled', () => {
  const stats = { total_cost_usd: 12.345, lifetime_chats: 9, total_work_time_ms: 3_725_000, per_model: [{ model: 'm1', cost_usd: 1, chats: 1, input_tokens: 10, output_tokens: 5 }] };
  assert.deepEqual(usageOverview(stats), [
    { label: 'Total spend', value: '$12.35' }, { label: 'Chats', value: '9' }, { label: 'Work time', value: '1h 2m' },
  ]);
  assert.equal(workTime(59_000), '59s');
  assert.equal(usagePerModel(stats)[0].value, '$1.00 · 1 chat · 10 in / 5 out');
  const rec = (id, status) => ({ record: { id, status, purpose: 'worker_turn', model: 'm1' }, owner_summary: `owner ${id}` });
  const status = { mode: 'deny_paid', active_invocations: [rec('a', 'running'), rec('a', 'running'), rec('b', 'cancellation_requested')],
    recent_denials: [{ owner_summary: 'x', denial_reason: 'paid work blocked' }],
    recent_budget_alerts: [{ scope_kind: 'project', scope_id: 'p', metric: 'total_tokens', remaining: 5, limit: 100 }] };
  const live = liveInvocations(status);
  assert.deepEqual(live.map((l) => [l.id, l.running]), [['a', true], ['b', false]], 'duplicates collapse; only running rows cancel');
  assert.deepEqual(deniedInvocations(status), [{ label: 'x', value: 'paid work blocked' }]);
  assert.equal(budgetAlerts(status)[0].value, 'total tokens: 5 left of 100');
  assert.equal(usageRowValue(stats, status), '$12.35 · 9 chats · Paid work blocked · 2 live');
  assert.equal(cancelMessage({ invocation_id: '12345678-aaaa', cancelled: true, mechanism: 'process' }), 'Invocation 12345678 cancelled via process');
  assert.equal(cancelMessage({ invocation_id: '12345678-aaaa', cancelled: false, message: 'already done' }), 'Invocation 12345678 unchanged: already done');
});

test('settings.ts draws overlay and TUI-only rows without a placeholder class', () => {
  const pane = src('src/settings.ts');
  assert.match(pane, /w\.type === 'tui-only'/);
  assert.match(pane, /type === 'overlay'/);
  const files = readdirSync(join(desktop, 'src')).filter((f) => f.endsWith('.ts'));
  assert.ok(files.includes('settings-state.ts'));
});

test('the audit check rejects a malformed tuiOnlyRows section and accepts the real one', () => {
  const ctx = { repoExists: (p) => p === 'crates/rsi/src/state.rs' };
  const good = { reason: 'A TUI-local preference in state.json: rsid has no state for it.', tui: ['crates/rsi/src/state.rs'], rows: { submit_on_enter: 'Enter submits in the TUI input bar (state.json).' } };
  assert.deepEqual(checkTuiOnly(good, ctx), []);
  assert.deepEqual(checkTuiOnly(undefined, ctx), []);
  assert.match(checkTuiOnly({ ...good, reason: '' }, ctx).join(), /reason must say/);
  assert.match(checkTuiOnly({ ...good, tui: ['crates/rsi/src/nope.rs'] }, ctx).join(), /does not exist/);
  assert.match(checkTuiOnly({ ...good, rows: {} }, ctx).join(), /rows must map/);
  assert.match(checkTuiOnly({ ...good, rows: { submit_on_enter: '' } }, ctx).join(), /needs its reason/);
  const real = JSON.parse(src('parity-audit.json')).tuiOnlyRows;
  assert.deepEqual(checkTuiOnly(real, { repoExists: (p) => readFileSync(join(desktop, '..', p)) !== null }), []);
});
