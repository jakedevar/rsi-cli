#!/usr/bin/env node
// Generate desktop/PARITY.md from docs/keybindings.md and the crates/rsi view
// modules, merging the hand-kept status in desktop/parity-status.d/*.json (one
// file per slice; a row id in two files fails).
//
//   node scripts/parity.mjs           write PARITY.md
//   node scripts/parity.mjs --check   exit 1 when PARITY.md is stale, a status
//                                     file names an unknown row id, or two
//                                     status files name the same row
//   --root DIR   read docs/ and crates/ from DIR instead of the repo root
//                (used to try an edited scratch copy; PARITY.md stays in desktop/)
//
// Node built-ins only; runs the same on macOS and Linux.
import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { checkAudit, quotedOps } from './parity-audit-lib.mjs';
import { buildRows, checkStatus, countByStatus, mergeSidecars, parseKeybindings, renderParity, withStatus } from './parity-lib.mjs';

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
export function readSidecars(dir) {
  const names = readdirSync(dir).filter((f) => f.endsWith('.json')).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  return mergeSidecars(names.map((name) => {
    try {
      return { name, data: JSON.parse(readFileSync(join(dir, name), 'utf8')) };
    } catch (e) {
      throw new Error(`parity-status.d/${name}: ${e.message}`);
    }
  }));
}

export function generate({ root, sidecarDir }) {
  const md = readFileSync(join(root, 'docs/keybindings.md'), 'utf8');
  const items = parseKeybindings(md);
  const rows = buildRows(items, {
    modules: listModules(root),
    exists: (rel) => existsSync(join(root, rel)),
  });
  const sidecar = readSidecars(sidecarDir);
  checkStatus(sidecar, rows);
  const full = withStatus(rows, sidecar);
  return { text: renderParity(full), rows: full };
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
  const root = resolve(argValue(args, '--root') ?? join(desktopDir, '..'));
  const out = join(desktopDir, 'PARITY.md');
  const { text, rows } = generate({ root, sidecarDir: join(desktopDir, 'parity-status.d') });
  const problems = auditProblems({ root, desktop: desktopDir, rows });
  if (problems.length) throw new Error(`parity-audit.json does not justify every substitute row:\n  ${problems.join('\n  ')}`);
  const counts = countByStatus(rows);
  const summary = `${rows.length} rows (${Object.entries(counts).map(([k, v]) => `${k} ${v}`).join(', ')})`;
  if (check) {
    const current = existsSync(out) ? readFileSync(out, 'utf8') : '';
    if (current !== text) {
      console.error(`parity: PARITY.md is stale (${summary} expected). Run: node scripts/parity.mjs`);
      process.exit(1);
    }
    console.log(`parity: PARITY.md is current, ${summary}`);
    return;
  }
  writeFileSync(out, text);
  console.log(`parity: wrote PARITY.md, ${summary}`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { main(); } catch (e) { console.error(`parity: ${e.message}`); process.exit(1); }
}
