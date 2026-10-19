// Pure logic behind the substitute-row audit (hub #3104). A PARITY row may be
// `substitute` only when desktop/parity-audit.json says, for that exact row,
// which rsid operation (or none, and why) replaces the TUI behaviour, what the
// confirmation and the refusal are, and what interaction evidence exists. The
// check is mechanical: every reference must resolve, so an audit entry cannot
// quietly rot. Node built-ins only; file access goes through callbacks.

export const CLASSES = ['ui-adaptation', 'source-backed'];
export const INTERACTIONS = ['dom-e2e', 'model-test'];

const MIN_TEXT = 12;

const text = (v) => typeof v === 'string' && v.trim().length >= MIN_TEXT;

/**
 * Problems in `audit` against the merged PARITY `rows` (each { id, status }).
 * ctx: {
 *   desktopText(path) -> string | null     a file under desktop/
 *   repoExists(path)  -> boolean           a path under the repository root
 *   desktopOps: Set<string>                rsid methods quoted in src-tauri/src
 *   tuiOps: Set<string>                    rsid methods quoted in crates/rsi/src
 * A group may carry tuiEquivalent {op: text} for an op the TUI does not call.
 * }
 */
export function checkAudit(audit, rows, ctx) {
  const problems = [];
  if (!audit || typeof audit !== 'object' || typeof audit.groups !== 'object' || typeof audit.rows !== 'object' || Array.isArray(audit.rows)) {
    return ['parity-audit.json: expected {"version":1,"groups":{...},"rows":{...}}'];
  }
  const byId = new Map(rows.map((r) => [r.id, r]));

  for (const [gid, g] of Object.entries(audit.groups)) {
    const at = `group ${gid}`;
    if (!g || typeof g !== 'object') { problems.push(`${at}: must be an object`); continue; }
    if (!CLASSES.includes(g.class)) problems.push(`${at}: class must be one of ${CLASSES.join('|')}`);
    if (!text(g.what)) problems.push(`${at}: what must say what replaces the TUI behaviour`);
    if (!Array.isArray(g.tui) || g.tui.length === 0) problems.push(`${at}: tui must list the TUI source files`);
    else for (const p of g.tui) if (!ctx.repoExists(p)) problems.push(`${at}: tui file does not exist: ${p}`);
    if (!Array.isArray(g.rsid)) problems.push(`${at}: rsid must be a list of rsid methods ([] when none)`);
    else if (g.rsid.length === 0) {
      if (g.class !== 'ui-adaptation') problems.push(`${at}: a source-backed substitute names the rsid operation`);
      if (!text(g.rsidNone)) problems.push(`${at}: rsidNone must say why no rsid operation is involved`);
    } else {
      for (const op of g.rsid) {
        if (!ctx.desktopOps.has(op)) problems.push(`${at}: the desktop backend never calls ${op}`);
        // The TUI sometimes reads or writes the same data itself (a file, a different method);
        // the group then says what the TUI does in tuiEquivalent[op], and that is all it may say.
        if (!ctx.tuiOps.has(op) && !text(g.tuiEquivalent?.[op])) problems.push(`${at}: the TUI never calls ${op}; tuiEquivalent.${op} must say what the TUI does instead`);
        else if (ctx.tuiOps.has(op) && g.tuiEquivalent?.[op] !== undefined) problems.push(`${at}: the TUI does call ${op}; remove tuiEquivalent.${op}`);
      }
    }
    if (!text(g.confirmation)) problems.push(`${at}: confirmation must say what confirms (or "none: why")`);
    if (!text(g.refusal)) problems.push(`${at}: refusal must say what is refused and how it is shown (or "none: why")`);
    if (!INTERACTIONS.includes(g.interaction)) problems.push(`${at}: interaction must be one of ${INTERACTIONS.join('|')}`);
    if (g.interaction !== 'dom-e2e' && !text(g.gap)) problems.push(`${at}: a ${g.interaction ?? 'missing'} interaction needs a gap saying what no DOM key-event run covers`);
    if (!Array.isArray(g.evidence) || g.evidence.length === 0) problems.push(`${at}: evidence must list at least one {file, anchor}`);
    else {
      for (const ev of g.evidence) {
        const body = ev && typeof ev.file === 'string' ? ctx.desktopText(ev.file) : null;
        if (body === null) problems.push(`${at}: evidence file does not exist: ${ev?.file}`);
        else if (typeof ev.anchor !== 'string' || !ev.anchor || !body.includes(ev.anchor)) problems.push(`${at}: ${ev.file} does not contain the anchor ${JSON.stringify(ev.anchor)}`);
      }
    }
  }

  const used = new Set();
  for (const [id, ref] of Object.entries(audit.rows)) {
    const gid = typeof ref === 'string' ? ref : ref?.group;
    const row = byId.get(id);
    if (!row) problems.push(`audit row ${id}: not a PARITY row`);
    else if (row.status !== 'substitute') problems.push(`audit row ${id}: the row is ${row.status}, not substitute; remove it from the audit`);
    if (!(gid in audit.groups)) problems.push(`audit row ${id}: unknown group ${JSON.stringify(gid)}`);
    else used.add(gid);
  }
  for (const r of rows) {
    if (r.status === 'substitute' && !(r.id in audit.rows)) problems.push(`substitute row ${r.id} has no entry in parity-audit.json (rsid operation, confirmation, refusal, evidence)`);
  }
  for (const gid of Object.keys(audit.groups)) if (!used.has(gid)) problems.push(`group ${gid}: no row uses it`);
  problems.push(...checkTuiOnly(audit.tuiOnlyRows, ctx));
  return problems;
}

/**
 * Local #125: the Settings rows that are TUI-local presentation preferences
 * (the TUI's own state.json, no rsid state) are documented here. Structure is
 * checked here; tests/settings-state.test.mjs checks the ids against the pane.
 */
export function checkTuiOnly(doc, ctx) {
  if (doc === undefined) return [];
  const at = 'tuiOnlyRows';
  if (!doc || typeof doc !== 'object' || Array.isArray(doc)) return [`${at}: must be an object`];
  const problems = [];
  if (!text(doc.reason)) problems.push(`${at}: reason must say these are TUI-local preferences with no rsid state`);
  if (!Array.isArray(doc.tui) || doc.tui.length === 0) problems.push(`${at}: tui must list the TUI source files`);
  else for (const p of doc.tui) if (!ctx.repoExists(p)) problems.push(`${at}: tui file does not exist: ${p}`);
  if (!doc.rows || typeof doc.rows !== 'object' || Array.isArray(doc.rows) || Object.keys(doc.rows).length === 0) problems.push(`${at}: rows must map each Settings row id to its reason`);
  else for (const [id, why] of Object.entries(doc.rows)) if (!text(why)) problems.push(`${at}: row ${id} needs its reason`);
  return problems;
}

/** Quoted rsid-style method names (`"ListSessions"`) in `texts`. */
export function quotedOps(texts) {
  const ops = new Set();
  for (const t of texts) for (const m of t.matchAll(/"([A-Z][A-Za-z0-9]{3,})"/g)) ops.add(m[1]);
  return ops;
}
