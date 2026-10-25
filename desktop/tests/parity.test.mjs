// Parser, status-sidecar and staleness tests for scripts/parity.mjs.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { closeSync, cpSync, existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, readSync, readdirSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { generate, writeAtomic } from '../scripts/parity.mjs';
import { describeParityState, firstDifference } from '../scripts/parity-diagnostics.mjs';
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
  const sidecarDir = join(desktop, 'parity-status.d');
  const { text } = generate({ root: repo, sidecarDir });
  const out = join(desktop, 'PARITY.md');
  assert.equal(readFileSync(out, 'utf8'), text, `run: node scripts/parity.mjs\n${describeParityState({ out, root: repo, sidecarDir, expected: text })}`);
});

test('PARITY.md lists every overlay heading of docs/keybindings.md', () => {
  const md = readFileSync(join(repo, 'docs/keybindings.md'), 'utf8');
  const out = join(desktop, 'PARITY.md');
  const parity = readFileSync(out, 'utf8');
  const state = () => describeParityState({ out, root: repo, sidecarDir: join(desktop, 'parity-status.d') });
  const headings = parseKeybindings(md).filter((i) => i.type === 'heading' && i.level === 3 && i.area.startsWith('overlay'));
  assert.ok(headings.length > 60, `expected many overlay headings, found ${headings.length}\n${state()}`);
  for (const h of headings) assert.ok(parity.includes(`| ${h.text}`) || parity.includes(`| ${h.text.replace(/\|/g, '\\|')}`), `missing overlay heading: ${h.text}\n${state()}`);
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
  const checked = run('--check');
  assert.equal(checked.status, 0, `--check on the repo: ${checked.stderr}${checked.stdout}`);

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
    assert.match(r.stderr, /have no entry in parity-status\.d/);
    assert.ok(existsSync(join(desktop, 'PARITY.md')), '--check never writes');
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

test('writeAtomic replaces the file in one rename: a reader holding the old file still sees all of it (#278)', () => {
  const dir = mkdtempSync(join(tmpdir(), 'parity-atomic-'));
  try {
    const target = join(dir, 'PARITY.md');
    const oldText = 'old line\n'.repeat(5000);
    const newText = 'new line\n'.repeat(7000);
    writeFileSync(target, oldText);
    const fd = openSync(target, 'r');
    try {
      writeAtomic(target, newText);
      // A truncate-then-write (writeFileSync on the target) would show this reader an empty or new file.
      const buf = Buffer.alloc(oldText.length + 16);
      const n = readSync(fd, buf, 0, buf.length, 0);
      assert.equal(buf.toString('utf8', 0, n), oldText, 'the open reader saw the complete old file');
    } finally {
      closeSync(fd);
    }
    assert.equal(readFileSync(target, 'utf8'), newText, 'a new reader sees the complete new file');
    assert.deepEqual(readdirSync(dir), ['PARITY.md'], 'no temp file is left behind');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('writeAtomic leaves the target and no temp file when the write fails (#278)', () => {
  const dir = mkdtempSync(join(tmpdir(), 'parity-atomic-'));
  try {
    const target = join(dir, 'PARITY.md');
    writeFileSync(target, 'kept\n');
    assert.throws(() => writeAtomic(join(dir, 'missing-dir', 'PARITY.md'), 'x'), /ENOENT/);
    assert.equal(readFileSync(target, 'utf8'), 'kept\n');
    assert.deepEqual(readdirSync(dir), ['PARITY.md']);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('parity diagnostics name the first differing line, the sizes and a truncated file (#278)', () => {
  assert.equal(firstDifference('a\nb\n', 'a\nb\n'), 'no differing line');
  assert.match(firstDifference('a\nx\n', 'a\nb\n'), /^line 2: file has "x", generator wants "b"/);
  assert.match(firstDifference('a\n', 'a\nb\n'), /line 2: file has "", generator wants "b"/);
  assert.match(firstDifference('a', 'a\nb'), /line 2: file has "\(end of file\)", generator wants "b"/);
  const dir = mkdtempSync(join(tmpdir(), 'parity-diag-'));
  try {
    const out = join(dir, 'PARITY.md');
    writeFileSync(out, '');
    const report = describeParityState({ out, root: repo, sidecarDir: join(desktop, 'parity-status.d'), expected: 'abc\n' });
    assert.match(report, /PARITY\.md: 0 bytes, mtime \d{4}-/);
    assert.match(report, /docs\/keybindings\.md: \d+ bytes/);
    assert.match(report, /\d+ status files, newest \S+\.json at /);
    assert.match(report, /expected 4 chars, file has 0/);
    assert.match(report, /truncate-then-write/);
    assert.match(describeParityState({ out: join(dir, 'gone.md'), root: repo, sidecarDir: join(dir, 'none') }), /gone\.md: missing/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
