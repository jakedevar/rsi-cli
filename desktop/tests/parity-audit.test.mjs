// Hub #3104: (1) every PARITY `substitute` row is justified in parity-audit.json
// and every partial or missing row has an owner; (2) the #3003 inventory guard
// (`parity.mjs --check`) fails when the TUI gains a keybinding, command or view
// that PARITY.md does not list. Each rule has a failing fixture; the last tests
// of each part are the positive state of the real tree.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { auditProblems, generate } from '../scripts/parity.mjs';
import { CLASSES, checkAudit, quotedOps } from '../scripts/parity-audit-lib.mjs';
import { checkStatus, countByStatus } from '../scripts/parity-lib.mjs';

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repo = resolve(desktop, '..');
const real = () => generate({ root: repo, sidecarDir: join(desktop, 'parity-status.d') }).rows;
const audit = () => JSON.parse(readFileSync(join(desktop, 'parity-audit.json'), 'utf8'));

// ---- fixtures for the pure checker -------------------------------------------------

const ctx = {
  desktopText: (p) => ({ 'tests/a.test.mjs': "test('the thing works', () => {})" })[p] ?? null,
  repoExists: (p) => p === 'crates/rsi/src/x.rs',
  desktopOps: new Set(['ListThings', 'PutThing']),
  tuiOps: new Set(['ListThings']),
};
const group = (extra = {}) => ({
  class: 'source-backed', what: 'a native control replaces the TUI line editor', tui: ['crates/rsi/src/x.rs'], rsid: ['ListThings'],
  confirmation: 'none: reading writes nothing', refusal: 'an rsid error is a toast and nothing changes',
  interaction: 'model-test', gap: 'no DOM key-event run in this tree',
  evidence: [{ file: 'tests/a.test.mjs', anchor: 'the thing works' }], ...extra,
});
const rowsOf = (...pairs) => pairs.map(([id, status]) => ({ id, status }));
const good = () => ({ version: 1, groups: { g: group() }, rows: { 'cmd:x': 'g' } });

test('fixture: a justified substitute passes', () => {
  assert.deepEqual(checkAudit(good(), rowsOf(['cmd:x', 'substitute'], ['cmd:y', 'done']), ctx), []);
});

test('fixture: a substitute row with no audit entry is rejected', () => {
  const p = checkAudit(good(), rowsOf(['cmd:x', 'substitute'], ['cmd:new', 'substitute']), ctx);
  assert.equal(p.length, 1);
  assert.match(p[0], /substitute row cmd:new has no entry in parity-audit\.json/);
});

test('fixture: an audit entry for a row that is no longer a substitute is rejected', () => {
  assert.match(checkAudit(good(), rowsOf(['cmd:x', 'partial']), ctx)[0], /cmd:x: the row is partial, not substitute/);
  assert.match(checkAudit(good(), rowsOf(['cmd:other', 'substitute']), ctx).join('\n'), /audit row cmd:x: not a PARITY row/);
});

test('fixture: every audit field is required, one problem each', () => {
  const run = (patch) => checkAudit({ ...good(), groups: { g: group(patch) } }, rowsOf(['cmd:x', 'substitute']), ctx);
  assert.match(run({ rsid: ['PutThing'] })[0], /the TUI never calls PutThing; tuiEquivalent\.PutThing must say/);
  assert.deepEqual(run({ rsid: ['PutThing'], tuiEquivalent: { PutThing: 'The TUI writes the same file with its own editor.' } }), []);
  assert.match(run({ tuiEquivalent: { ListThings: 'The TUI reads the file itself directly.' } })[0], /the TUI does call ListThings; remove tuiEquivalent/);
  assert.match(run({ rsid: ['Imaginary'] }).join('\n'), /the desktop backend never calls Imaginary/);
  assert.match(run({ rsid: [] })[0], /source-backed substitute names the rsid operation/);
  assert.match(run({ class: 'ui-adaptation', rsid: [] })[0], /rsidNone must say why no rsid operation is involved/);
  assert.deepEqual(run({ class: 'ui-adaptation', rsid: [], rsidNone: 'Client layout only; the TUI calls no daemon operation.' }), []);
  assert.match(run({ confirmation: '' })[0], /confirmation must say/);
  assert.match(run({ refusal: 'x' })[0], /refusal must say/);
  assert.match(run({ tui: ['crates/rsi/src/gone.rs'] })[0], /tui file does not exist: crates\/rsi\/src\/gone\.rs/);
  assert.match(run({ tui: [] })[0], /tui must list the TUI source files/);
  assert.match(run({ class: 'invented' })[0], /class must be one of/);
  assert.match(run({ evidence: [] })[0], /evidence must list at least one/);
  assert.match(run({ evidence: [{ file: 'tests/missing.test.mjs', anchor: 'x' }] })[0], /evidence file does not exist/);
  assert.match(run({ evidence: [{ file: 'tests/a.test.mjs', anchor: 'a test that was renamed' }] })[0], /does not contain the anchor "a test that was renamed"/);
  assert.match(run({ interaction: 'model-test', gap: '' })[0], /needs a gap/);
  assert.match(run({ interaction: 'telepathy' })[0], /interaction must be one of/);
  assert.deepEqual(run({ interaction: 'dom-e2e', gap: undefined }), [], 'a DOM run needs no gap');
});

test('fixture: an unused group and an unknown group reference are rejected', () => {
  const a = { ...good(), groups: { g: group(), spare: group() } };
  assert.match(checkAudit(a, rowsOf(['cmd:x', 'substitute']), ctx)[0], /group spare: no row uses it/);
  const b = { ...good(), rows: { 'cmd:x': 'nope' } };
  assert.match(checkAudit(b, rowsOf(['cmd:x', 'substitute']), ctx).join('\n'), /unknown group "nope"/);
  assert.match(checkAudit({}, [], ctx)[0], /expected \{"version":1/);
});

test('fixture: a partial or missing row needs an owner Issue and a note; an owner elsewhere is an error', () => {
  const rows = [{ id: 'cmd:x', kind: 'cmd' }];
  const run = (e) => { try { checkStatus({ rows: { 'cmd:x': e } }, rows); return null; } catch (err) { return err.message; } };
  assert.match(run({ status: 'partial', note: 'file editing is not implemented' }), /a partial row needs "owner"/);
  assert.match(run({ status: 'missing', note: 'x', owner: [] }), /a missing row needs "owner"/);
  assert.match(run({ status: 'partial', note: 'x', owner: ['113'] }), /needs "owner"/);
  assert.match(run({ status: 'partial', owner: ['#113'] }), /needs a note saying what is missing/);
  assert.equal(run({ status: 'partial', note: 'file editing is not implemented', owner: ['#113'] }), null);
  assert.match(run({ status: 'done', owner: ['#113'] }), /owner is only for partial or missing rows/);
});

test('quotedOps reads rsid method names from source text', () => {
  const ops = quotedOps(['call("ListThings", Null); const X: &str = "PutThing"; "lowercase" "Ab"']);
  assert.deepEqual([...ops].sort(), ['ListThings', 'PutThing']);
});

// ---- the real tree -----------------------------------------------------------------

test('positive state: every substitute row is justified and every reference resolves', () => {
  const rows = real();
  assert.deepEqual(auditProblems({ root: repo, desktop, rows }), []);
  const subs = rows.filter((r) => r.status === 'substitute');
  assert.ok(subs.length >= 60, `substitute rows: ${subs.length}`);
  assert.equal(Object.keys(audit().rows).length, subs.length);
});

test('positive state: every audit group states its class, an rsid operation or why none, and the gap in interaction evidence', () => {
  for (const [gid, g] of Object.entries(audit().groups)) {
    assert.ok(CLASSES.includes(g.class), gid);
    assert.ok(g.rsid.length > 0 || g.rsidNone, `${gid} names an rsid operation or why none`);
    assert.ok(g.interaction === 'dom-e2e' || g.gap, `${gid} states its interaction gap`);
  }
});

test('positive state: any honest gap is partial with an owner, not substitute', () => {
  const byId = new Map(real().map((r) => [r.id, r]));
  const partial = [...byId.values()].filter((r) => r.status === 'partial' || r.status === 'missing');
  for (const r of partial) assert.ok(r.owner.length > 0 && r.owner.every((o) => /^#\d+$/.test(o)), `${r.id} has an owner`);
  // Read-only file viewing is not editing parity (#113): the explorer's file operations and the editor (with the
  // viewer's colours, search, folds, bracket match and git signs, #134) are done over rsid.
  for (const id of [
    'overlay:file-explorer-overlay-space-e', 'overlay-ref:file-explorer', 'module:ui/overlay/file_explorer.rs',
    'overlay:file-viewer-opened-from-file-explorer', 'overlay-ref:file-viewer',
  ]) {
    assert.equal(byId.get(id).status, 'done', id);
  }
  // The graph run now binds the session workflow (#124); only the info dashboard layout is a justified substitute.
  assert.equal(byId.get('overlay:graph-review-overlay-space-v-or-graph').status, 'substitute');
  assert.equal(audit().rows['overlay:graph-review-overlay-space-v-or-graph'], 'graph-review-dashboard');
  assert.ok(audit().groups['graph-review-dashboard'].rsid.includes('UpdateSessionWorkflow'));
  // Settings > Skills e/d run over SetSkillEnabled / DeleteSkill (#97).
  for (const id of ['overlay:settings-pane-space-or-set', 'module:ui/settings.rs', 'cmd:skills']) assert.equal(byId.get(id).status, 'done', id);
  const counts = countByStatus(real());
  assert.equal(counts.missing, 0);
  assert.equal(counts.partial, partial.length);
});

test('the rendered PARITY.md shows the editor rows as done', () => {
  const md = readFileSync(join(desktop, 'PARITY.md'), 'utf8');
  assert.match(md, /\| `overlay-ref:file-viewer` \|.*\| done \|/);
  assert.match(md, /\| `cmd:skills` \|.*\| done \|/);
});

// ---- the #3003 inventory guard: a TUI keybinding, command or view missing from PARITY fails --------

function scratchRoot() {
  const scratch = mkdtempSync(join(tmpdir(), 'inventory-'));
  mkdirSync(join(scratch, 'docs'));
  cpSync(join(repo, 'docs/keybindings.md'), join(scratch, 'docs/keybindings.md'));
  cpSync(join(repo, 'crates/rsi/src'), join(scratch, 'crates/rsi/src'), { recursive: true, filter: (s) => !s.endsWith('.png') });
  return scratch;
}
const check = (root) => spawnSync(process.execPath, [join(desktop, 'scripts/parity.mjs'), '--check', '--root', root], { encoding: 'utf8' });
const edit = (root, rel, fn) => { const p = join(root, rel); writeFileSync(p, fn(readFileSync(p, 'utf8'))); };

test('inventory guard: an unedited copy of the TUI sources passes', () => {
  const root = scratchRoot();
  try {
    const r = check(root);
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stdout, /PARITY\.md is current, 514 rows/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('inventory guard: a new Normal-mode keybinding the TUI documents but PARITY lacks fails CI', () => {
  const root = scratchRoot();
  try {
    edit(root, 'docs/keybindings.md', (t) => t.replace('| `j` | Move selection or scroll down |', '| `Ctrl-Alt-Z` | Brand new action | Does a brand new thing. |\n| `j` | Move selection or scroll down |'));
    const r = check(root);
    assert.equal(r.status, 1);
    assert.match(r.stderr, /PARITY\.md is stale \(515 rows \(missing 1,/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('inventory guard: a new : command the TUI documents but PARITY lacks fails CI', () => {
  const root = scratchRoot();
  try {
    edit(root, 'docs/keybindings.md', (t) => t.replace('| `:commands` |  | All commands |', '| `:brandnew` |  | Brand new | Does a brand new thing. |\n| `:commands` |  | All commands |'));
    const r = check(root);
    assert.equal(r.status, 1);
    assert.match(r.stderr, /PARITY\.md is stale \(515 rows/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('inventory guard: a new overlay heading the TUI documents but PARITY lacks fails CI', () => {
  const root = scratchRoot();
  try {
    edit(root, 'docs/keybindings.md', (t) => t.replace('## Overlay Key Reference (generated)', '## Overlay Key Reference (generated)\n\n### Brand New Overlay\n\n| Keys | Action | What it does |\n| --- | --- | --- |\n| `x` | Do | Does a thing. |'));
    const r = check(root);
    assert.equal(r.status, 1);
    assert.match(r.stderr, /stale/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('inventory guard: a new TUI view module (crates/rsi/src/ui) that PARITY lacks fails CI', () => {
  const root = scratchRoot();
  try {
    writeFileSync(join(root, 'crates/rsi/src/ui/brand_new_view.rs'), '//! A brand new view.\n');
    writeFileSync(join(root, 'crates/rsi/src/ui/overlay/brand_new_overlay.rs'), '//! A brand new overlay.\n');
    const r = check(root);
    assert.equal(r.status, 1);
    assert.match(r.stderr, /PARITY\.md is stale \(516 rows/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('inventory guard: the new rows start as missing, so the gap cannot be hidden', () => {
  const root = scratchRoot();
  try {
    writeFileSync(join(root, 'crates/rsi/src/ui/brand_new_view.rs'), '//! A brand new view.\n');
    const { rows } = generate({ root, sidecarDir: join(desktop, 'parity-status.d') });
    const row = rows.find((r) => r.id === 'module:ui/brand_new_view.rs');
    assert.ok(row, 'the module is a row');
    assert.equal(row.status, 'missing');
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('inventory guard: a substitute added without an audit entry fails the same --check', () => {
  const root = scratchRoot();
  const status = join(desktop, 'parity-status.d');
  const copy = mkdtempSync(join(tmpdir(), 'status-'));
  try {
    cpSync(status, copy, { recursive: true });
    const p = join(copy, 'nav.json');
    const d = JSON.parse(readFileSync(p, 'utf8'));
    const id = Object.keys(d.rows).find((k) => d.rows[k].status === 'done');
    d.rows[id] = { status: 'substitute', location: 'src/x.ts', note: 'a replacement that the audit does not know' };
    writeFileSync(p, `${JSON.stringify(d, null, 2)}\n`);
    assert.throws(() => {
      const { rows } = generate({ root, sidecarDir: copy });
      const problems = auditProblems({ root, desktop, rows });
      if (problems.length) throw new Error(problems.join('\n'));
    }, /substitute row \S+ has no entry in parity-audit\.json/);
  } finally {
    rmSync(root, { recursive: true, force: true });
    rmSync(copy, { recursive: true, force: true });
  }
});

test('the inventory guard runs in npm test and in the hub desktop lane', () => {
  const pkg = JSON.parse(readFileSync(join(desktop, 'package.json'), 'utf8'));
  assert.match(pkg.scripts.test, /node scripts\/parity\.mjs --check/);
  const gate = readFileSync(join(repo, 'scripts/desktop-test-gate.py'), 'utf8');
  assert.match(gate, /docs\/keybindings\.md/);
  assert.match(gate, /crates\/rsi\/src/);
});
