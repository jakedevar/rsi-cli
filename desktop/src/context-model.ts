// The context-budget rows of the session info panel (F3), pure: the TUI's
// ContextBudgetViewModel::detail_rows (crates/rsi/src/types/context_budget.rs)
// over the daemon-resolved budget it stamps on every Session. The daemon owns
// the math; this only labels its facts. Tested in tests/context.test.mjs.

export interface ContextCapacity {
  advertised_max_tokens?: number | null;
  provider_default_tokens?: number | null;
  provider_max_tokens?: number | null;
  effective_percent?: number | null;
  compaction_limit_tokens?: number | null;
  max_output_tokens?: number | null;
}

export interface ResolvedContextBudget {
  active_tokens: number;
  capacity?: ContextCapacity | null;
  evidence?: {
    source?: string;
    source_version?: string | null;
    source_digest?: string | null;
    observed_at?: string | null;
    confidence?: string;
  } | null;
}

export interface ContextFields {
  context_fill_pct?: number | null;
  input_tokens?: number | null;
  context_usage_confidence?: string | null;
  resolved_context_budget?: ResolvedContextBudget | null;
}

/** 34k, 1.05m (context_budget.rs format_tokens). */
export function formatTokens(tokens: number): string {
  if (tokens >= 1_000_000) return `${(tokens / 1_000_000).toFixed(2).replace(/0+$/, '').replace(/\.$/, '')}m`;
  if (tokens >= 1_000) return `${Math.floor((tokens + 500) / 1_000)}k`;
  return String(tokens);
}

const SOURCE_LABEL: Readonly<Record<string, string>> = {
  runtime_telemetry: 'runtime telemetry',
  configured: 'configured',
  provider_catalog: 'provider catalog',
  repository_fallback: 'repository fallback',
  legacy_unverified: 'legacy unverified',
  official_documentation: 'official documentation',
};

const ACTIVE_LABEL: Readonly<Record<string, string>> = {
  runtime_telemetry: 'runtime',
  configured: 'configured',
  provider_catalog: 'provider default',
  repository_fallback: 'repository fallback',
  legacy_unverified: 'legacy unverified',
  official_documentation: 'documented maximum',
};

function freshness(source: string, confidence: string): string {
  if ((source === 'runtime_telemetry' || source === 'configured') && confidence === 'authoritative') return 'fresh';
  if (source === 'provider_catalog' && confidence === 'verified') return 'cold';
  if (source === 'repository_fallback') return 'degraded';
  if (source === 'legacy_unverified') return 'legacy-unverified';
  if (confidence === 'authoritative') return 'fresh';
  if (confidence === 'verified') return 'verified';
  return 'degraded';
}

function usageLabel(c: string | null | undefined): string {
  switch (c) {
    case 'Counted': return 'counted';
    case 'Full': return 'full';
    case 'Partial': return 'partial';
    case 'Stale': return 'stale';
    case 'Missing': case undefined: case null: return 'missing';
    default: return 'unknown';
  }
}

function observed(at: string): string {
  const t = Date.parse(at);
  return Number.isFinite(t) ? new Date(t).toISOString().replace(/\.\d{3}Z$/, 'Z') : at;
}

/** The F3 context rows; `context: n/a` when the daemon reported nothing. */
export function contextRows(s: ContextFields): Array<[string, string]> {
  const r = s.resolved_context_budget;
  const pct = s.context_fill_pct;
  if (!r) {
    if (typeof pct === 'number') return [['context', `${Math.round(Math.max(0, Math.min(100, pct)))}% runtime meter`]];
    return [['context', 'n/a']];
  }
  const ev = r.evidence ?? {};
  const source = ev.source ?? 'legacy_unverified';
  const rows: Array<[string, string]> = [];
  const usage = typeof s.input_tokens === 'number' ? formatTokens(s.input_tokens) : '—';
  rows.push(['context', `${usage} / ${formatTokens(r.active_tokens)} ${ACTIVE_LABEL[source] ?? source}`]);
  const c = r.capacity ?? {};
  const provider = [
    c.provider_default_tokens != null ? `default ${formatTokens(c.provider_default_tokens)}` : null,
    c.provider_max_tokens != null ? `max ${formatTokens(c.provider_max_tokens)}` : null,
    c.effective_percent != null ? `effective factor ${c.effective_percent}%` : null,
  ].filter((x): x is string => x !== null);
  if (provider.length) rows.push(['provider', provider.join(' · ')]);
  const api = [
    c.advertised_max_tokens != null ? `context max ${formatTokens(c.advertised_max_tokens)}` : null,
    c.max_output_tokens != null ? `output max ${formatTokens(c.max_output_tokens)}` : null,
  ].filter((x): x is string => x !== null);
  if (api.length) rows.push(['API', api.join(' · ')]);
  if (c.compaction_limit_tokens != null) rows.push(['compaction', `limit ${formatTokens(c.compaction_limit_tokens)}`]);
  rows.push(['source', SOURCE_LABEL[source] ?? source]);
  if (ev.source_version) rows.push(['version', ev.source_version]);
  if (ev.source_digest) rows.push(['digest', ev.source_digest]);
  const fresh = [freshness(source, ev.confidence ?? 'degraded')];
  if (ev.observed_at) fresh.push(`observed ${observed(ev.observed_at)}`);
  fresh.push(`usage ${usageLabel(s.context_usage_confidence)}`);
  rows.push(['freshness', fresh.join(' · ')]);
  return rows;
}
