#!/usr/bin/env node
// Bounded source check (hub #3104): the shipped desktop client reaches project
// files and rsi state only through rsid. Rejects direct file-system, store and
// state-path access in desktop/src, desktop/static and desktop/src-tauri, except
// the facilities documented in desktop/source-guard.json.
//
//   node scripts/source-guard.mjs [--check]  scan and exit 1 on a violation
//   node scripts/source-guard.mjs --list     print every hit and whether it is allowed
//   --root DIR   scan DIR (laid out like desktop/) instead of desktop/
//
// Node built-ins only; runs the same on macOS and Linux.
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { judge, scanClient } from './source-guard-lib.mjs';

const desktopDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const ROOTS = ['src', 'static', 'src-tauri/src', 'src-tauri/capabilities'];
const FILES = ['src-tauri/Cargo.toml', 'src-tauri/tauri.conf.json', 'package.json'];

function walk(dir, out) {
  if (!existsSync(dir)) return;
  for (const name of readdirSync(dir).sort()) {
    const p = join(dir, name);
    if (name === 'node_modules' || name === 'target' || name === 'gen') continue;
    if (statSync(p).isDirectory()) walk(p, out);
    else out.push(p);
  }
}

/** The client files under `root`: [{ path, text }] with `/` separators. */
export function readClient(root) {
  const paths = [];
  for (const r of ROOTS) walk(join(root, r), paths);
  for (const f of FILES) if (existsSync(join(root, f))) paths.push(join(root, f));
  return paths.map((p) => ({ path: relative(root, p).split(sep).join('/'), text: readFileSync(p, 'utf8') }));
}

export function readFacilities(root) {
  const p = join(root, 'source-guard.json');
  const data = JSON.parse(readFileSync(p, 'utf8'));
  if (!Array.isArray(data.facilities)) throw new Error('source-guard.json: expected {"facilities": [...]}');
  return data.facilities;
}

export function check(root) {
  const hits = scanClient(readClient(root));
  const { violations } = judge(hits, readFacilities(root));
  return { hits, violations };
}

function main() {
  const args = process.argv.slice(2);
  const i = args.indexOf('--root');
  const root = i >= 0 ? resolve(args[i + 1]) : desktopDir;
  const { hits, violations } = check(root);
  if (args.includes('--list')) for (const h of hits) console.log(`${h.file}:${h.line} ${h.rule} ${h.text}`);
  if (violations.length) {
    console.error(`source-guard: ${violations.length} violation(s). The desktop client must reach files and rsi state through rsid (or add a documented facility to source-guard.json):`);
    for (const v of violations) console.error(`  ${v}`);
    process.exit(1);
  }
  console.log(`source-guard: ok (${hits.length} documented facility use(s), 0 violations)`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
