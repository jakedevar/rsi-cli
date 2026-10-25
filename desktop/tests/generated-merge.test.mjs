// #60 (folding in #94): the per-slice registration outputs (index.generated.ts,
// runtime.generated.ts, handlers.generated.rs, PARITY.md) are build products, so
// two slices built from the same base that each add a module merge with no
// conflict and no hand regeneration, and the next build regenerates and checks
// the merged result. parity.mjs tolerates a mid-stack duplicate row only when
// asked (--allow-duplicates), loudly, and never under --check.
//
// Every git command here runs in a scratch repository under the OS temp dir
// through `git()`, which pins `-C <scratch>`, scrubs inherited GIT_* variables,
// disables hooks and refuses a directory whose top level is not the scratch.
// It also turns off git's automatic maintenance (#303): after a commit or merge
// git starts `git maintenance run --auto --detach`, which repacks and deletes
// loose objects in the background. That detached process raced the test's own
// git commands ("fatal: bad object HEAD") and its temp-dir cleanup, so the
// scratch repository now has no background writer and the cleanup needs no retry.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve, sep } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { generateAll } from '../scripts/slices.mjs';

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repo = resolve(desktop, '..');
const GENERATED = ['PARITY.md', 'src/actions/index.generated.ts', 'src/actions/runtime.generated.ts', 'src-tauri/src/handlers.generated.rs', 'src-tauri/src/modules.generated.rs'];
const SKIP = new Set(['node_modules', 'dist', 'target', 'gen', '.rsi-tmp']);

/** No auto maintenance or gc (a detached repack deletes objects under the test, #303), no fsmonitor daemon. */
const NO_BACKGROUND_GIT = ['-c', 'maintenance.auto=false', '-c', 'gc.auto=0', '-c', 'gc.autoDetach=false', '-c', 'core.fsmonitor=false'];

function scrubbedEnv(hooks) {
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('GIT_')));
  return { ...env, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: join(hooks, 'gitconfig'), GIT_TERMINAL_PROMPT: '0' };
}

/** A scratch repository: `root/desktop` is a copy of the tracked desktop sources. */
function scratchRepo() {
  const scratch = realpathSync(mkdtempSync(join(tmpdir(), 'generated-merge-')));
  const root = join(scratch, 'repo');
  const hooks = join(scratch, 'no-hooks');
  mkdirSync(hooks);
  writeFileSync(join(hooks, 'gitconfig'), '');
  cpSync(desktop, join(root, 'desktop'), {
    recursive: true,
    filter: (src) => {
      const rel = relative(desktop, src);
      return !rel.split(sep).some((part) => SKIP.has(part)) && !GENERATED.includes(rel.split(sep).join('/'));
    },
  });
  const git = (...args) => {
    const r = spawnSync('git', ['-C', root, '-c', `core.hooksPath=${hooks}`, '-c', 'user.name=t', '-c', 'user.email=t@example.invalid',
      '-c', 'commit.gpgsign=false', '-c', 'init.defaultBranch=base', '-c', 'core.autocrlf=false',
      ...NO_BACKGROUND_GIT, ...args], { encoding: 'utf8', env: scrubbedEnv(hooks) });
    if (r.error) throw r.error;
    return r;
  };
  const ok = (...args) => { const r = git(...args); assert.equal(r.status, 0, `git ${args.join(' ')}: ${r.stderr}`); return r.stdout; };
  ok('init', '-q');
  assert.equal(realpathSync(ok('rev-parse', '--show-toplevel').trim()), root, 'git runs only in the scratch repository');
  ok('add', '--', 'desktop');
  ok('commit', '-q', '-m', 'base');
  const node = (script, ...args) => spawnSync(process.execPath, [join(root, 'desktop/scripts', script), ...args], { encoding: 'utf8' });
  const cleanup = () => rmSync(scratch, { recursive: true, force: true });
  return { root, dir: join(root, 'desktop'), git, ok, node, cleanup };
}

/** One slice: a new action module, a new Tauri command module and its own parity-status.d note. */
function addSlice({ dir, ok }, name, sidecar, rowId) {
  const ts = `src/actions/zz-${name}.ts`;
  const rs = `src-tauri/src/zz_${name}.rs`;
  writeFileSync(join(dir, ts), `export const SLICE = { order: 999, actions: [] } as const;\nexport const RUNTIME = { run: {} };\n`);
  writeFileSync(join(dir, rs), `#[tauri::command]\npub fn zz_${name}_ping() {}\n`);
  const p = join(dir, 'parity-status.d', sidecar);
  const data = JSON.parse(readFileSync(p, 'utf8'));
  assert.ok(data.rows[rowId], `${sidecar} owns ${rowId}`);
  data.rows[rowId].note = `${data.rows[rowId].note} (slice ${name})`;
  writeFileSync(p, `${JSON.stringify(data, null, 2)}\n`);
  ok('add', '--', `desktop/${ts}`, `desktop/${rs}`, `desktop/parity-status.d/${sidecar}`);
  ok('commit', '-q', '-m', `slice ${name}`);
}

const firstRow = (dir, sidecar) => Object.keys(JSON.parse(readFileSync(join(dir, 'parity-status.d', sidecar), 'utf8')).rows)[0];

test('the per-slice registration outputs are ignored build products, not tracked files', (t) => {
  const r = spawnSync('git', ['-C', repo, 'ls-files', '--', ...GENERATED.map((g) => `desktop/${g}`)], { encoding: 'utf8' });
  if (r.status !== 0) { t.skip(`not a git checkout: ${r.stderr.trim()}`); return; }
  assert.equal(r.stdout, '', 'none of the generated files is committed');
  const ignored = spawnSync('git', ['-C', repo, 'check-ignore', '--no-index', ...GENERATED.map((g) => `desktop/${g}`)], { encoding: 'utf8' });
  assert.deepEqual(ignored.stdout.trim().split('\n').sort(), GENERATED.map((g) => `desktop/${g}`).sort());
});

test('the scratch repository has no background git writer: a commit starts no maintenance and leaves no pack', () => {
  const s = scratchRepo();
  try {
    s.ok('commit', '-q', '--allow-empty', '-m', 'second');
    const packDir = join(s.root, '.git/objects/pack');
    const packs = readdirSync(packDir).filter((f) => f.endsWith('.pack'));
    assert.deepEqual(packs, [], 'nothing repacked the loose objects');
    // The traced commit must not start `git maintenance`.
    const trace = join(s.root, '..', 'trace.log');
    const r = spawnSync('git', ['-C', s.root, '-c', 'user.name=t', '-c', 'user.email=t@example.invalid', '-c', 'core.hooksPath=/dev/null',
      ...NO_BACKGROUND_GIT, 'commit', '-q', '--allow-empty', '-m', 'third'], { encoding: 'utf8', env: { ...scrubbedEnv(join(s.root, '..', 'no-hooks')), GIT_TRACE: trace } });
    assert.equal(r.status, 0, r.stderr);
    assert.doesNotMatch(readFileSync(trace, 'utf8'), /maintenance|gc --auto|repack/, 'git started no background maintenance');
  } finally {
    s.cleanup();
  }
});

test('two slices from the same base each add a module and merge cleanly; the build regenerates both', () => {
  const s = scratchRepo();
  try {
    const viewsRow = firstRow(s.dir, 'views.json');
    const settingsRow = firstRow(s.dir, 'settings.json');
    s.ok('checkout', '-q', '-b', 'slice-alpha');
    addSlice(s, 'alpha', 'views.json', viewsRow);
    // Each slice builds on its own branch: the outputs it writes stay out of git.
    assert.equal(s.node('slices.mjs').status, 0);
    s.ok('checkout', '-q', 'base');
    s.ok('checkout', '-q', '-b', 'slice-beta');
    addSlice(s, 'beta', 'settings.json', settingsRow);
    assert.equal(s.node('slices.mjs').status, 0);

    s.ok('checkout', '-q', 'slice-alpha');
    const merge = s.git('merge', '--no-edit', 'slice-beta');
    assert.equal(merge.status, 0, `the merge is clean: ${merge.stdout}${merge.stderr}`);
    assert.equal(s.ok('diff', '--name-only', '--diff-filter=U'), '', 'no conflicted path');

    const slices = s.node('slices.mjs');
    assert.equal(slices.status, 0, slices.stderr);
    const parity = s.node('parity.mjs', '--root', repo);
    assert.equal(parity.status, 0, parity.stderr);
    assert.equal(s.node('slices.mjs', '--check').status, 0, 'the regenerated registration is current');
    assert.equal(s.node('parity.mjs', '--check', '--root', repo).status, 0, 'the regenerated PARITY.md is current');

    const read = (rel) => readFileSync(join(s.dir, rel), 'utf8');
    for (const name of ['alpha', 'beta']) {
      assert.match(read('src/actions/index.generated.ts'), new RegExp(`import \\{ SLICE as zz_${name} \\} from './zz-${name}\\.js';`));
      assert.match(read('src/actions/runtime.generated.ts'), new RegExp(`import \\{ RUNTIME as zz_${name} \\} from './zz-${name}\\.js';`));
      assert.match(read('src-tauri/src/handlers.generated.rs'), new RegExp(`^    zz_${name}::zz_${name}_ping,$`, 'm'));
      assert.match(read('src-tauri/src/modules.generated.rs'), new RegExp(`^pub mod zz_${name};$`, 'm'), 'the module declaration is generated, not a lib.rs edit');
      assert.match(read('PARITY.md'), new RegExp(`\\(slice ${name}\\) \\|`));
    }
    assert.equal(s.ok('status', '--porcelain'), '', 'the regenerated outputs leave the merge commit clean');
    assert.doesNotMatch(read('src-tauri/src/lib.rs'), /^pub mod zz_/m, 'no slice edited a pub mod line in lib.rs');
  } finally {
    s.cleanup();
  }
});

test('parity.mjs refuses a duplicate row, writes it only with --allow-duplicates and loudly, never under --check', () => {
  const s = scratchRepo();
  try {
    const row = firstRow(s.dir, 'views.json');
    const entry = JSON.parse(readFileSync(join(s.dir, 'parity-status.d/views.json'), 'utf8')).rows[row];
    const claim = (file) => writeFileSync(join(s.dir, 'parity-status.d', file), `${JSON.stringify({ version: 1, rows: { [row]: entry } }, null, 2)}\n`);
    claim('zz-moved.json');

    const refused = s.node('parity.mjs', '--root', repo);
    assert.equal(refused.status, 1);
    assert.match(refused.stderr, new RegExp(`row ${row} is in both parity-status.d/views.json and parity-status.d/zz-moved.json.*--allow-duplicates`));

    const allowed = s.node('parity.mjs', '--allow-duplicates', '--root', repo);
    assert.equal(allowed.status, 0, allowed.stderr);
    assert.match(allowed.stderr, new RegExp(`warning: in-flight duplicate: row ${row} is in both parity-status.d/views.json and parity-status.d/zz-moved.json`));
    assert.match(readFileSync(join(s.dir, 'PARITY.md'), 'utf8'), new RegExp(`^> \\*\\*In-flight duplicate claims.*\`${row}\` in views.json and zz-moved.json`, 'm'));

    const checked = s.node('parity.mjs', '--check', '--allow-duplicates', '--root', repo);
    assert.equal(checked.status, 1);
    assert.match(checked.stderr, /--check never tolerates a duplicate row/);
    assert.equal(s.node('parity.mjs', '--check', '--root', repo).status, 1, '--check refuses the duplicate');

    claim('zz-third.json');
    const third = s.node('parity.mjs', '--allow-duplicates', '--root', repo);
    assert.equal(third.status, 1);
    assert.match(third.stderr, /tolerates two files, never more/);
  } finally {
    s.cleanup();
  }
});

test('lib.rs declares no module by hand and every module file is in the generated list', () => {
  const src = join(desktop, 'src-tauri/src');
  const lib = readFileSync(join(src, 'lib.rs'), 'utf8');
  assert.doesNotMatch(lib, /^\s*pub mod \w+;/m, 'a hand-written `pub mod` line is the last shared per-slice edit (#181)');
  assert.match(lib, /^include!\("modules\.generated\.rs"\);$/m);
  const gen = generateAll(desktop).find((o) => o.path.endsWith('modules.generated.rs')).text;
  const declared = [...gen.matchAll(/^pub mod (\w+);$/gm)].map((m) => m[1]);
  assert.ok(declared.includes('admin') && declared.includes('manager_ops'));
  assert.ok(!declared.includes('manager_ops_tests') && !declared.includes('lib') && !declared.includes('handlers'));
});

test('npm run parity regenerates the registration, writes PARITY.md and prints it', () => {
  const pkg = JSON.parse(readFileSync(join(desktop, 'package.json'), 'utf8'));
  assert.match(pkg.scripts.parity, /slices\.mjs/);
  assert.match(pkg.scripts.parity, /parity\.mjs --print/);
  const s = scratchRepo();
  try {
    const r = s.node('parity.mjs', '--print', '--root', repo);
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stdout, /^# Desktop parity with the TUI$/m);
    assert.equal(r.stdout.slice(r.stdout.indexOf('# Desktop parity')), readFileSync(join(s.dir, 'PARITY.md'), 'utf8'));
    assert.equal(s.node('parity.mjs', '--check', '--print', '--root', repo).status, 1, '--check never prints');
  } finally {
    s.cleanup();
  }
});
