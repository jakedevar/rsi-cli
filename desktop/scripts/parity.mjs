#!/usr/bin/env node
// Generate desktop/PARITY.md from docs/keybindings.md and the crates/rsi view
// modules, merging the hand-kept status in desktop/parity-status.d/*.json (one
// file per slice; a row id in two files fails).
//
// PARITY.md is a build product, not committed (#60, desktop/.gitignore): each
// slice only edits its own parity-status.d file, so parallel slices merge
// cleanly. Every run fails when a status file names an unknown row id, two
// status files name the same row, a generated row has no status entry, or
// parity-audit.json does not justify a substitute row.
//
//   node scripts/parity.mjs           write PARITY.md (part of `npm test`)
//   node scripts/parity.mjs --print   write it, then print it to stdout (`npm run parity`)
//   node scripts/parity.mjs --check   write nothing; also exit 1 when PARITY.md
//                                     is missing or stale
//   --allow-duplicates   (write mode only) mid-stack, while a row moves between
//                two slices' files, write PARITY.md anyway: each pair is warned
//                on stderr and named in a banner atop PARITY.md; `npm test`
//                never passes it, so the pair cannot land
//   --root DIR   read docs/ and crates/ from DIR instead of the repo root
//                (used to try an edited scratch copy; PARITY.md stays in desktop/)
//
// Node built-ins only; runs the same on macOS and Linux.
import { existsSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { checkAudit, quotedOps } from './parity-audit-lib.mjs';
import { describeParityState } from './parity-diagnostics.mjs';
import { buildRows, checkStatus, countByStatus, mergeSidecars, parseKeybindings, renderParity, unstatusedRows, withStatus } from './parity-lib.mjs';

const desktopDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function argValue(args, name) {
  const i = args.indexOf(name);
  if (i < 0) return null;
  const v = args[i + 1];
  if (!v || v.startsWith('--')) throw new Error(`${name} needs a value`);
  return v;
}

/** `crates/rsi/src/ui/*.rs` and `ui/overlay/*.rs`, sorted bytewise, minus mod.rs/tests.rs. */
export function listModules(root) {
  const out = [];
  for (const sub of ['crates/rsi/src/ui', 'crates/rsi/src/ui/overlay']) {
    const dir = join(root, sub);
    if (!existsSync(dir)) continue;
    for (const f of readdirSync(dir)) {
      if (f.endsWith('.rs') && f !== 'mod.rs' && f !== 'tests.rs') out.push(`${sub}/${f}`);
    }
  }
  return out.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

/** The parity-status.d/*.json files, sorted bytewise, merged into one sidecar. */
export function readSidecars(dir, opts = {}) {
  const names = readdirSync(dir).filter((f) => f.endsWith('.json')).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  return mergeSidecars(names.map((name) => {
    try {
      return { name, data: JSON.parse(readFileSync(join(dir, name), 'utf8')) };
    } catch (e) {
      throw new Error(`parity-status.d/${name}: ${e.message}`);
    }
  }), opts);
}

/**
 * Replace `path` with `text` in one rename (#278). A plain writeFileSync truncates the file first, so a
 * test or `--check` running at the same moment (a second `npm test`, `npm run parity` or smoke run in
 * the same tree) read an empty or half-written PARITY.md and failed with "stale"/"missing". With a
 * rename a reader sees the whole old file or the whole new one. The temp file sits next to the target
 * (same filesystem, so the rename is atomic) under a name desktop/.gitignore covers.
 */
export function writeAtomic(path, text) {
  const tmp = `${path}.${process.pid}.tmp`;
  try {
    writeFileSync(tmp, text);
    renameSync(tmp, path);
  } catch (err) {
    rmSync(tmp, { force: true });
    throw err;
  }
}

export function generate({ root, sidecarDir, allowDuplicates = false }) {
  const md = readFileSync(join(root, 'docs/keybindings.md'), 'utf8');
  const items = parseKeybindings(md);
  const rows = buildRows(items, {
    modules: listModules(root),
    exists: (rel) => existsSync(join(root, rel)),
  });
  const sidecar = readSidecars(sidecarDir, { allowDuplicates });
  checkStatus(sidecar, rows);
  if (sidecar.duplicates.length) checkStatus(sidecar.shadowed, rows);
  const full = withStatus(rows, sidecar);
  return {
    text: renderParity(full, { duplicates: sidecar.duplicates }),
    rows: full,
    duplicates: sidecar.duplicates,
    unstatused: unstatusedRows(sidecar, rows),
  };
}

function walkFiles(dir, ext, out = []) {
  if (!existsSync(dir)) return out;
  for (const name of readdirSync(dir).sort()) {
    const p = join(dir, name);
    if (name === 'node_modules' || name === 'target') continue;
    if (statSync(p).isDirectory()) walkFiles(p, ext, out);
    else if (p.endsWith(ext)) out.push(p);
  }
  return out;
}

/**
 * Hub #3104: problems in desktop/parity-audit.json against the generated rows.
 * `root` is the repository root (crates/rsi/src is read from it), `desktop` the
 * desktop/ directory.
 */
export function auditProblems({ root, desktop, rows }) {
  const audit = JSON.parse(readFileSync(join(desktop, 'parity-audit.json'), 'utf8'));
  const read = (files) => files.map((f) => readFileSync(f, 'utf8'));
  return checkAudit(audit, rows, {
    desktopText: (rel) => { const p = join(desktop, rel); return existsSync(p) ? readFileSync(p, 'utf8') : null; },
    repoExists: (rel) => existsSync(join(root, rel)),
    desktopOps: quotedOps(read(walkFiles(join(desktop, 'src-tauri/src'), '.rs'))),
    tuiOps: quotedOps(read(walkFiles(join(root, 'crates/rsi/src'), '.rs'))),
  });
}

function main() {
  const args = process.argv.slice(2);
  const check = args.includes('--check');
  const print = args.includes('--print');
  if (check && print) throw new Error('--print writes and prints PARITY.md; --check writes nothing');
  const allowDuplicates = args.includes('--allow-duplicates');
  if (check && allowDuplicates) throw new Error('--allow-duplicates is for writing a mid-stack PARITY.md; --check never tolerates a duplicate row');
  const root = resolve(argValue(args, '--root') ?? join(desktopDir, '..'));
  const out = join(desktopDir, 'PARITY.md');
  const { text, rows, duplicates, unstatused } = generate({ root, sidecarDir: join(desktopDir, 'parity-status.d'), allowDuplicates });
  // The #3003 inventory guard: a row the TUI gained that no slice has triaged.
  if (unstatused.length) {
    throw new Error(`${unstatused.length} row(s) have no entry in parity-status.d/*.json; add each to the owning slice's file (\`missing\` with an owner while the desktop lacks it): ${unstatused.join(', ')}`);
  }
  for (const d of duplicates) console.error(`parity: warning: in-flight duplicate: row ${d.id} is in both parity-status.d/${d.files[0]} and parity-status.d/${d.files[1]}; PARITY.md uses ${d.files[0]}; this cannot land (npm test refuses it)`);
  const problems = auditProblems({ root, desktop: desktopDir, rows });
  if (problems.length) throw new Error(`parity-audit.json does not justify every substitute row:\n  ${problems.join('\n  ')}`);
  const counts = countByStatus(rows);
  const summary = `${rows.length} rows (${Object.entries(counts).map(([k, v]) => `${k} ${v}`).join(', ')})`;
  if (check) {
    const current = existsSync(out) ? readFileSync(out, 'utf8') : '';
    if (current !== text) {
      console.error(`parity: PARITY.md is ${current ? 'stale' : 'missing'} (${summary} expected). Run: node scripts/parity.mjs`);
      console.error(describeParityState({ out, root, sidecarDir: join(desktopDir, 'parity-status.d'), expected: text }));
      process.exit(1);
    }
    console.log(`parity: PARITY.md is current, ${summary}`);
    return;
  }
  writeAtomic(out, text);
  console.log(`parity: wrote PARITY.md, ${summary}${duplicates.length ? `, ${duplicates.length} in-flight duplicate(s)` : ''}`);
  if (print) process.stdout.write(text);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { main(); } catch (e) { console.error(`parity: ${e.message}`); process.exit(1); }
}
