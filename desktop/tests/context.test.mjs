// Gap sweep G2 (hub #3000): the F3 session info context-budget rows mirror the
// TUI's ContextBudgetViewModel::detail_rows (types/context_budget.rs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { contextRows, formatTokens } from '../dist/context-model.js';

const budget = (source, confidence) => ({
  active_tokens: 258_400,
  capacity: {
    advertised_max_tokens: 1_050_000, provider_default_tokens: 272_000, provider_max_tokens: 872_000,
    effective_percent: 95, compaction_limit_tokens: 230_000, max_output_tokens: 128_000,
  },
  evidence: {
    source, confidence, source_version: 'codex-cli 0.155.1', source_digest: `sha256:${'a'.repeat(64)}`,
    observed_at: '2026-09-02T12:34:56.123456789Z',
  },
});

test('tokens are formatted like the TUI', () => {
  assert.equal(formatTokens(34_000), '34k');
  assert.equal(formatTokens(258_400), '258k');
  assert.equal(formatTokens(1_050_000), '1.05m');
  assert.equal(formatTokens(2_000_000), '2m');
  assert.equal(formatTokens(999), '999');
});

test('the detail rows keep active, provider, API and compaction semantics distinct', () => {
  const rows = contextRows({ input_tokens: 34_000, context_usage_confidence: 'Full', resolved_context_budget: budget('runtime_telemetry', 'authoritative') });
  const get = (k) => rows.find((r) => r[0] === k)?.[1];
  assert.deepEqual(rows.map((r) => r[0]), ['context', 'provider', 'API', 'compaction', 'source', 'version', 'digest', 'freshness']);
  assert.equal(get('context'), '34k / 258k runtime');
  assert.equal(get('provider'), 'default 272k · max 872k · effective factor 95%');
  assert.equal(get('API'), 'context max 1.05m · output max 128k');
  assert.equal(get('compaction'), 'limit 230k');
  assert.equal(get('source'), 'runtime telemetry');
  assert.equal(get('freshness'), 'fresh · observed 2026-09-02T12:34:56Z · usage full');
});

test('freshness and labels follow the provenance', () => {
  const f = (src, conf, usage) => contextRows({ context_usage_confidence: usage, resolved_context_budget: budget(src, conf) }).find((r) => r[0] === 'freshness')[1];
  assert.match(f('provider_catalog', 'verified', 'Partial'), /^cold · .* · usage partial$/);
  assert.match(f('repository_fallback', 'authoritative', 'Stale'), /^degraded · .* · usage stale$/);
  assert.match(f('legacy_unverified', 'degraded', null), /^legacy-unverified · .* · usage missing$/);
  const ctx = contextRows({ resolved_context_budget: budget('official_documentation', 'verified') })[0][1];
  assert.equal(ctx, '— / 258k documented maximum');
});

test('without a resolved budget the runtime meter or n/a', () => {
  assert.deepEqual(contextRows({ context_fill_pct: 41.6 }), [['context', '42% runtime meter']]);
  assert.deepEqual(contextRows({ context_fill_pct: 140 }), [['context', '100% runtime meter']]);
  assert.deepEqual(contextRows({}), [['context', 'n/a']]);
});
