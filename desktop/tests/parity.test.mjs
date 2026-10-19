// Parser, status-sidecar and staleness tests for scripts/parity.mjs.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { generate } from '../scripts/parity.mjs';
import { buildRows, checkStatus, parseKeybindings, renderParity, slug, splitRow, withStatus } from '../scripts/parity-lib.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const desktop = resolve(here, '..');
const repo = resolve(desktop, '..');
const fixture = readFileSync(join(here, 'fixtures', 'keybindings-excerpt.md'), 'utf8');
const rowsOf = (md, opts = {}) => buildRows(parseKeybindings(md), opts);
const byKind = (rows, kind) => rows.filter((r) => r.kind === kind);

test('splitRow handles escaped pipes and trims cells', () => {
  assert.deepEqual(splitRow('| `a\\|b` | x |  | y |'), ['`a|b`', 'x', '', 'y']);
});

test('slug is stable and ascii', () => {
  assert.equal(slug('Sort Picker Overlay (`<Space>s`)'), 'sort-picker-overlay-space-s');
});

test('fixture: normal keys come from the generated table only', () => {
  const rows = rowsOf(fixture);
  assert.deepEqual(byKind(rows, 'key').map((r) => r.id), ['key:?', 'key:<Space>gg', 'key:<Space>hs / :pause soft']);
  assert.ok(!rows.some((r) => r.binding === 'x'), 'hand-written Session Management table adds no key row');
});

test('fixture: leader rows are the Space sequences the generated table lacks', () => {
  assert.deepEqual(byKind(rowsOf(fixture), 'leader').map((r) => r.id), ['leader:<Space>zz']);
});

test('fixture: code fences are not parsed', () => {
  assert.ok(!rowsOf(fixture).some((r) => /ghost/i.test(r.id + r.binding)));
});

test('fixture: global rows are told apart by chord and context', () => {
  const ids = byKind(rowsOf(fixture), 'global').map((r) => r.id);
  assert.deepEqual(ids, ['global:ctrl-v/any-context', 'global:enter/quick-launch-input', 'global:enter/create-form']);
});

test('fixture: command ids use the command words and drop arguments', () => {
  const rows = byKind(rowsOf(fixture), 'cmd');
  assert.deepEqual(rows.map((r) => r.id), ['cmd:help', 'cmd:manual', 'cmd:manager-board', 'cmd:manager-inbox']);
  assert.equal(rows[0].binding, ':help (:keys)');
});

test('fixture: every overlay h3 is a row (h4 is not), with a key count', () => {
  const rows = rowsOf(fixture);
  const overlays = byKind(rows, 'overlay');
  assert.deepEqual(overlays.map((r) => r.id), ['overlay:question-panel', 'overlay:sort-picker-overlay-space-s']);
  assert.deepEqual(overlays.map((r) => r.keys), [3, 1]);
  assert.deepEqual(byKind(rows, 'overlay-ref').map((r) => [r.id, r.keys]), [['overlay-ref:sort-picker', 2]]);
});

test('fixture: section rows cover h3/h4 outside overlays, with the parent for h4', () => {
  const ids = byKind(rowsOf(fixture), 'section').map((r) => r.id);
  assert.deepEqual(ids, [
    'section:all-normal-mode-keys-generated',
    'section:session-management',
    'section:leader-key-and-g-chord-migration-space',
    'section:leader-key-and-g-chord-migration-space-nested-heading',
    'section:rsi-specific-commands',
  ]);
});

test('module rows and module hints', () => {
  const mods = ['crates/rsi/src/ui/overlay/question_modal.rs'];
  const rows = rowsOf(fixture, { modules: mods });
  assert.equal(rows.find((r) => r.id === 'overlay:question-panel').module, 'crates/rsi/src/ui/overlay/question_modal.rs');
  assert.deepEqual(byKind(rows, 'module').map((r) => r.id), ['module:ui/overlay/question_modal.rs']);
  assert.throws(() => rowsOf(fixture, { exists: () => false }), /missing/);
});

test('ids are unique', () => {
  const ids = rowsOf(fixture).map((r) => r.id);
  assert.equal(new Set(ids).size, ids.length);
});

test('checkStatus rejects unknown ids, bad status and a note-less substitute', () => {
  const rows = rowsOf(fixture);
  const ok = { version: 1, rows: { 'key:?': { status: 'done', location: 'src/x.ts' } } };
  assert.doesNotThrow(() => checkStatus(ok, rows));
  assert.throws(() => checkStatus({ rows: { 'key:nope': { status: 'done' } } }, rows), /unknown row id.*key:nope/);
  assert.throws(() => checkStatus({ rows: { 'key:?': { status: 'finished' } } }, rows), /status must be/);
  assert.throws(() => checkStatus({ rows: { 'key:?': { status: 'substitute' } } }, rows), /substitute needs a note/);
  assert.throws(() => checkStatus({ rows: { 'key:?': { status: 'done', extra: 1 } } }, rows), /unknown field/);
});

test('status survives regeneration and defaults to missing', () => {
  const rows = rowsOf(fixture);
  const sidecar = { rows: { 'cmd:help': { status: 'partial', location: 'src/p.ts', note: 'palette only' } } };
  const text = renderParity(withStatus(rows, sidecar));
  assert.match(text, /\| `cmd:help` \| cmd \| `:help \(:keys\)` \| `commands\.rs` \| partial \| `src\/p\.ts` \| palette only \|/);
  assert.match(text, /\| `cmd:manual` \| cmd \|.*\| missing \|/);
  assert.equal(renderParity(withStatus(rows, sidecar)), text, 'rendering is deterministic');
});

test('PARITY.md matches the generator (the --check contract)', () => {
  const { text } = generate({ root: repo, sidecarDir: join(desktop, 'parity-status.d') });
  assert.equal(readFileSync(join(desktop, 'PARITY.md'), 'utf8'), text, 'run: node scripts/parity.mjs');
});

test('PARITY.md lists every overlay heading of docs/keybindings.md', () => {
  const md = readFileSync(join(repo, 'docs/keybindings.md'), 'utf8');
  const parity = readFileSync(join(desktop, 'PARITY.md'), 'utf8');
  const headings = parseKeybindings(md).filter((i) => i.type === 'heading' && i.level === 3 && i.area.startsWith('overlay'));
  assert.ok(headings.length > 60, `expected many overlay headings, found ${headings.length}`);
  for (const h of headings) assert.ok(parity.includes(`| ${h.text}`) || parity.includes(`| ${h.text.replace(/\|/g, '\\|')}`), `missing overlay heading: ${h.text}`);
});

test('every row of the real keybindings has a unique id and a known kind', () => {
  const { rows } = generate({ root: repo, sidecarDir: join(desktop, 'parity-status.d') });
  assert.equal(new Set(rows.map((r) => r.id)).size, rows.length);
  for (const r of rows) assert.match(r.id, /^(section|global|key|leader|cmd|overlay|overlay-ref|module):/);
  assert.ok(rows.some((r) => r.id === 'key:<Space>gg'));
  assert.ok(rows.some((r) => r.id === 'cmd:help'));
});

test('--check exits 0 on the repo and non-zero on an edited keybindings.md', () => {
  const run = (...args) => spawnSync(process.execPath, [join(desktop, 'scripts/parity.mjs'), ...args], { encoding: 'utf8' });
  assert.equal(run('--check').status, 0);

  const scratch = mkdtempSync(join(tmpdir(), 'parity-'));
  try {
    mkdirSync(join(scratch, 'docs'));
    cpSync(join(repo, 'docs/keybindings.md'), join(scratch, 'docs/keybindings.md'));
    symlinkSync(join(repo, 'crates'), join(scratch, 'crates'), 'junction');
    assert.equal(run('--check', '--root', scratch).status, 0, 'an unedited copy is still current');
    const p = join(scratch, 'docs/keybindings.md');
    writeFileSync(p, `${readFileSync(p, 'utf8')}\n### Brand New Overlay\n`.replace('## Overlay Key Reference (generated)', '## Overlay Key Reference (generated)\n\n### Brand New Overlay'));
    const r = run('--check', '--root', scratch);
    assert.equal(r.status, 1);
    assert.match(r.stderr, /stale/);
    assert.ok(existsSync(join(desktop, 'PARITY.md')), '--check never writes');
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});
