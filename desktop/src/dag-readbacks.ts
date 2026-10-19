// The recursive DAG inspector's readbacks (gap sweep G4, hub #3000), pure (no
// DOM): which ids a row carries and how a preview reads. The DOM side is
// dag.ts; the daemon calls are src-tauri/src/dag_inspect.rs.

type Row = Record<string, unknown>;

const num = (v: unknown): number | null => (typeof v === 'number' && Number.isInteger(v) ? v : typeof v === 'string' && /^\d+$/.test(v) ? Number(v) : null);
const id = (v: unknown): string | null => (typeof v === 'string' && /^[0-9a-f-]{36}$/i.test(v) ? v : null);

/** An artifact summary's id (`artifact_id`, else `id`) and graph. */
export function artifactRef(row: Row | null, hydrated: string | null): { graphId: string; artifactId: number } | null {
  if (!row) return null;
  const artifactId = num(row.artifact_id) ?? num(row.id);
  const graphId = id(row.graph_id) ?? hydrated;
  return artifactId !== null && graphId ? { graphId, artifactId } : null;
}

/** The validation a row names (a validation artifact, or a live validation readback). */
export function validationOf(row: Row | null): string | null {
  if (!row) return null;
  return id(row.validation_id) ?? (row.kind === 'validation' || row.kind === 'Validation' ? id(row.source_id) : null);
}

/** A live-attempt row's id (the readback nests it under `summary`). */
export function liveAttemptOf(row: Row | null): string | null {
  if (!row) return null;
  const sum = row.summary && typeof row.summary === 'object' ? row.summary as Row : row;
  return id(sum.live_attempt_id) ?? id(sum.id) ?? id(row.live_attempt_id);
}

/** The preview's state line: state, shown / total lines and bytes, and what was cut. */
export function previewHead(p: Row): string {
  const parts = [String(p.content_state ?? 'unknown')];
  if (typeof p.content_type === 'string') parts.push(p.content_type);
  const lines = `${p.shown_lines ?? 0}${p.total_lines != null ? ` of ${p.total_lines}` : ''} lines`;
  const bytes = `${p.shown_bytes ?? 0}${p.total_bytes != null ? ` of ${p.total_bytes}` : ''} bytes`;
  parts.push(lines, bytes);
  if (p.truncated) parts.push(`truncated by ${[p.truncated_by_lines ? 'lines' : '', p.truncated_by_bytes ? 'bytes' : ''].filter(Boolean).join(' and ') || 'the bounds'} (max ${p.applied_max_lines} lines, ${p.applied_max_bytes} bytes)`);
  return parts.join(' · ');
}

/** The preview body: its text, else why there is none. */
export function previewBody(p: Row): string {
  if (typeof p.text === 'string') return p.text;
  if (typeof p.binary_unavailable_reason === 'string') return `binary or unavailable: ${p.binary_unavailable_reason}`;
  return '(no text in this preview)';
}

/** The buckets a live attempt's artifact readback carries, with a size each. */
export function bucketLines(b: Row): string[] {
  return Object.entries(b).filter(([k]) => k !== 'live_attempt_id').map(([k, v]) => {
    if (v === null || v === undefined) return `${k}: none`;
    if (Array.isArray(v)) return `${k}: ${v.length} item${v.length === 1 ? '' : 's'}`;
    if (typeof v === 'object') return `${k}: ${Object.keys(v as Row).length} field${Object.keys(v as Row).length === 1 ? '' : 's'}`;
    return `${k}: ${String(v).slice(0, 80)}`;
  });
}
