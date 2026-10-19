// scripts/source-guard.mjs (hub #3104): the shipped desktop client reaches
// project files and rsi state only through rsid. Each rule has a failing
// fixture here; the last tests are the positive state of the real tree.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { check, readClient, readFacilities } from '../scripts/source-guard.mjs';
import { cargoDependencies, judge, scanClient } from '../scripts/source-guard-lib.mjs';

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const hitsOf = (path, text) => scanClient([{ path, text }]).map((h) => h.rule);

test('fixture: TypeScript that reads files itself is rejected, one rule each', () => {
  assert.deepEqual(hitsOf('src/x.ts', "import { readTextFile } from '@tauri-apps/plugin-fs';"), ['ts-fs-plugin']);
  assert.deepEqual(hitsOf('src/x.ts', "const o = await window.__TAURI__.fs.readTextFile(p);"), ['ts-fs-plugin']);
  assert.deepEqual(hitsOf('src/x.ts', "import { readFileSync } from 'node:fs';"), ['ts-node-builtin']);
  assert.deepEqual(hitsOf('src/x.ts', "const fs = require('fs');"), ['ts-node-builtin']);
  assert.deepEqual(hitsOf('src/x.ts', 'const h = await window.showOpenFilePicker();'), ['ts-file-picker']);
  assert.deepEqual(hitsOf('src/x.ts', "const u = 'file:///Users/me/proj/src/lib.rs';"), ['ts-file-url']);
  assert.deepEqual(hitsOf('src/x.ts', "img.src = convertFileSrc(path);"), ['ts-file-url']);
});

test('fixture: Rust that reads files, the store or rsi state itself is rejected, one rule each', () => {
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'let t = std::fs::read_to_string(p)?;'), ['rs-fs-api']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'let f = File::open(p)?;'), ['rs-fs-api']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'for e in read_dir(p)? {}'), ['rs-fs-api']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'if p.exists() {}'), ['rs-path-probe']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'let c = rusqlite::Connection::open(db)?;'), ['rs-store']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'use rsid_store::Store;'), ['rs-store']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'let db = home.join(".rsi/rsi.db");'), ['rs-rsi-state']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'let s = home.join(".claude").join("settings.json");'), ['rs-rsi-state']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'Command::new("git").arg("status").output()?;'), ['rs-process']);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'let h = std::env::var("HOME")?;'), ['rs-env']);
});

test('fixture: forbidden crates, npm plugins and capabilities are rejected', () => {
  const toml = '[package]\nname = "x"\n\n[dependencies]\ntauri = "2"\nrsid-store = { path = "../../crates/rsid-store" }\ntauri-plugin-fs = "2"\n\n[dev-dependencies]\nrusqlite = "0.31"\n';
  assert.deepEqual(cargoDependencies(toml), ['tauri', 'rsid-store', 'tauri-plugin-fs']);
  assert.deepEqual(scanClient([{ path: 'src-tauri/Cargo.toml', text: toml }]).map((h) => h.text), ['rsid-store', 'tauri-plugin-fs']);
  assert.deepEqual(hitsOf('src-tauri/capabilities/default.json', '{"permissions": ["core:default", "fs:default"]}'), ['cfg-fs-permission']);
  assert.deepEqual(hitsOf('src-tauri/capabilities/default.json', '{"permissions": ["shell:allow-execute"]}'), ['cfg-fs-permission']);
  assert.deepEqual(hitsOf('src-tauri/tauri.conf.json', '"assetProtocol": {"enable": true}'), ['cfg-asset-protocol']);
  assert.deepEqual(hitsOf('package.json', '{"dependencies": {"@tauri-apps/plugin-fs": "2.0.0"}}'), ['npm-forbidden-plugin']);
});

test('comment lines, the Rust test module and generated files are not scanned', () => {
  assert.deepEqual(hitsOf('src/x.ts', "// import fs from 'node:fs';\n/* showOpenFilePicker */\n * file:// is not used"), []);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', '/// reads std::fs::read_to_string\n// File::open\nfn f() {}\n'), []);
  assert.deepEqual(hitsOf('src-tauri/src/x.rs', 'fn f() {}\n#[cfg(test)]\nmod tests { fn t() { std::fs::write(p, b"").unwrap(); } }\n'), []);
  assert.deepEqual(hitsOf('src/keymap.generated.ts', "require('x')"), []);
  assert.deepEqual(hitsOf('scripts/x.mjs', "import fs from 'node:fs';"), [], 'build and test tools are out of scope');
  assert.deepEqual(hitsOf('src/x.ts', "toast('the daemon host keeps ~/.claude/settings.json');"), [], 'display text naming a daemon-host path is not access');
});

test('judge: a hit needs a documented facility with a reason; unused and over-used ones fail', () => {
  const hits = scanClient([{ path: 'src-tauri/src/open.rs', text: 'let c = Command::new(p);\nlet d = Command::new(q);\n' }]);
  assert.equal(hits.length, 2);
  const undocumented = judge(hits, []).violations;
  assert.equal(undocumented.length, 2);
  assert.match(undocumented[0], /src-tauri\/src\/open\.rs:1 rs-process/);

  const fac = { file: 'src-tauri/src/open.rs', rule: 'rs-process', max: 2, reason: 'Hands one checked URL to the platform opener.' };
  assert.deepEqual(judge(hits, [fac]).violations, []);
  assert.match(judge(hits, [{ ...fac, max: 1 }]).violations[0], /more than the 1 documented use/);
  assert.match(judge(hits, [{ ...fac, reason: 'short' }]).violations[0], /reason of at least 20 characters/);
  assert.match(judge([], [fac]).violations[0], /stale facility/);
  assert.match(judge([], [{ file: 'a', rule: 'b', reason: 'x'.repeat(30), max: 0 }]).violations[0], /max must be a positive integer/);
});

test('the CLI fails on a scratch client with a violation and names the file and rule', () => {
  const scratch = mkdtempSync(join(tmpdir(), 'srcguard-'));
  try {
    mkdirSync(join(scratch, 'src'), { recursive: true });
    mkdirSync(join(scratch, 'src-tauri/src'), { recursive: true });
    writeFileSync(join(scratch, 'source-guard.json'), '{"facilities": []}');
    writeFileSync(join(scratch, 'src/ok.ts'), 'export const x = 1;\n');
    const run = () => spawnSync(process.execPath, [join(desktop, 'scripts/source-guard.mjs'), '--root', scratch], { encoding: 'utf8' });
    assert.equal(run().status, 0, 'a clean client passes');
    writeFileSync(join(scratch, 'src-tauri/src/leak.rs'), 'fn leak(p: &str) -> String { std::fs::read_to_string(p).unwrap() }\n');
    const r = run();
    assert.equal(r.status, 1);
    assert.match(r.stderr, /src-tauri\/src\/leak\.rs:1 rs-fs-api/);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

test('positive state: the real client has no undocumented file, store or rsi-state access', () => {
  const { hits, violations } = check(desktop);
  assert.deepEqual(violations, []);
  const files = readClient(desktop).map((f) => f.path);
  assert.ok(files.some((f) => f === 'src-tauri/src/daemon.rs'), 'the Rust client is scanned');
  assert.ok(files.some((f) => f === 'src/api.ts'), 'the TypeScript client is scanned');
  assert.ok(files.includes('src-tauri/Cargo.toml') && files.includes('src-tauri/capabilities/default.json'));
  assert.ok(files.length > 100, `scanned ${files.length} client files`);
  assert.ok(hits.length > 0 && hits.length <= readFacilities(desktop).reduce((n, f) => n + f.max, 0), 'every hit is a documented facility use');
});

test('the documented facilities are exactly the socket, the URL opener and the terminal PTY', () => {
  const fac = readFacilities(desktop);
  assert.deepEqual([...new Set(fac.map((f) => f.file))].sort(), ['src-tauri/src/daemon.rs', 'src-tauri/src/nav.rs', 'src-tauri/src/pty.rs']);
  for (const f of fac) assert.ok(f.reason.length >= 20);
});

test('npm test runs the source guard', () => {
  const pkg = JSON.parse(readFileSync(join(desktop, 'package.json'), 'utf8'));
  assert.match(pkg.scripts.test, /scripts\/source-guard\.mjs --check/);
});
