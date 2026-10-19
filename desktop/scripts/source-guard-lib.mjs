// Pure logic behind scripts/source-guard.mjs (hub #3104): the shipped desktop
// client must reach project files and rsi state only through rsid. This module
// scans source text for the ways a client could bypass the daemon and reports
// each hit that is not covered by a documented facility in source-guard.json.
//
// Scope (the shipped client): src/**/*.ts (not *.generated.ts), static/,
// src-tauri/src/**/*.rs up to the first `#[cfg(test)]`, src-tauri/Cargo.toml
// dependencies, src-tauri/tauri.conf.json and src-tauri/capabilities/*.json.
// scripts/ and tests/ are build and test tools that run on the developer host
// and are not shipped, so they are out of scope.
//
// Line based on purpose: a comment-only line is skipped, nothing else is
// parsed, so a hit is always a line a reviewer can read. Node built-ins only.

/**
 * Rules for TypeScript client code. A string that merely names a daemon-host
 * path ("~/.claude/settings.json" in a dialog) is display text, not access, so
 * the rules ban the capabilities that could read a path: Tauri plugins, Node
 * built-ins, the File System Access API and file or asset URLs. Which local paths
 * the shell may touch is the Rust side's job (RS_RULES).
 */
export const TS_RULES = [
  ['ts-fs-plugin', /@tauri-apps\/plugin-(fs|shell|dialog|opener)|__TAURI__\??\.(fs|shell|dialog|path)\b/, 'a Tauri plugin that reads files or runs programs from the webview'],
  ['ts-node-builtin', /\bfrom\s+['"]node:|\brequire\s*\(|\bimport\s*\(\s*['"]node:/, 'a Node built-in in browser code'],
  ['ts-file-picker', /\bshow(Open|Save)FilePicker\b|\bshowDirectoryPicker\b|\bwebkitdirectory\b|\bFileSystemHandle\b/, 'the File System Access API'],
  ['ts-file-url', /['"`]file:\/\/|\bconvertFileSrc\b|['"`]asset:/, 'a file or asset URL'],
];

/** Rules for Rust client code (production part only). */
export const RS_RULES = [
  ['rs-fs-api', /\b(std|tokio|async_std)::fs\b|\bfs::[a-z_]+\s*\(|\bFile::(open|create|options)\b|\bOpenOptions\b|\bread_dir\b|\bread_to_string\b|\bcreate_dir(_all)?\b|\bremove_(file|dir)(_all)?\b/, 'a direct file-system call'],
  ['rs-path-probe', /\.(is_dir|is_file|exists|try_exists|metadata|canonicalize)\s*\(/, 'a probe of the local file system'],
  ['rs-store', /\brusqlite\b|\bsqlite\b|\brsid_store\b|\brsid_core\b|\brsid_remote_read\b|\brsid::/i, 'direct access to the rsid store or daemon internals'],
  ['rs-rsi-state', /["'](~|\$HOME)?\/?\.rsi\b|\.rsi\/|\brsi\.db\b|\bstate\.json\b|["']\.claude["']|\.claude\/|\bsignal\.toml\b|\bimessage\.toml\b/, 'a path into rsi state or the operator\'s config'],
  ['rs-process', /\b(std::process::)?Command::new\b|\bCommandBuilder::new\b/, 'a spawned local process'],
  ['rs-env', /\benv::var(_os)?\b/, 'an environment lookup'],
];

/** Dependencies the desktop crate must never name. */
export const FORBIDDEN_CRATES = [
  'rsid', 'rsid-store', 'rsid-core', 'rsid-remote-read', 'rusqlite', 'sqlx', 'libsqlite3-sys',
  'tauri-plugin-fs', 'tauri-plugin-shell', 'tauri-plugin-dialog', 'tauri-plugin-opener',
];

/** Capability and config text that grants the webview file or shell access. */
export const CONFIG_RULES = [
  ['cfg-fs-permission', /["'](fs|shell|dialog|opener):/, 'a capability that grants file, shell, dialog or opener access'],
  ['cfg-asset-protocol', /assetProtocol|dangerousDisableAssetCspModification|["']fs["']\s*:|["']shell["']\s*:/, 'an asset protocol or fs/shell config'],
];

function isCommentLine(line) {
  const t = line.trim();
  return t.startsWith('//') || t.startsWith('*') || t.startsWith('/*');
}

/** The production lines of a Rust file: everything before the first `#[cfg(test)]`. */
function rustProduction(text) {
  const lines = text.split('\n');
  const cut = lines.findIndex((l) => l.trim() === '#[cfg(test)]');
  return cut < 0 ? lines : lines.slice(0, cut);
}

/** Hits of `rules` on `lines`: [{ rule, line, text, why }]. */
function scanLines(lines, rules) {
  const hits = [];
  lines.forEach((line, i) => {
    if (isCommentLine(line)) return;
    for (const [rule, re, why] of rules) {
      if (re.test(line)) hits.push({ rule, line: i + 1, text: line.trim().slice(0, 140), why });
    }
  });
  return hits;
}

/** Dependency names in a Cargo.toml `[dependencies]` / `[build-dependencies]` table. */
export function cargoDependencies(toml) {
  const names = [];
  let inDeps = false;
  for (const raw of toml.split('\n')) {
    const line = raw.trim();
    const table = /^\[(.+)\]$/.exec(line);
    if (table) { inDeps = /(^|\.)(dependencies|build-dependencies)$/.test(table[1]); continue; }
    if (!inDeps || line.startsWith('#')) continue;
    const m = /^([A-Za-z0-9_-]+)\s*=/.exec(line);
    if (m) names.push(m[1]);
  }
  return names;
}

/**
 * Scan the shipped client. `files` is [{ path, text }] with paths relative to
 * desktop/ and `/` separators. Returns every hit, allowed or not.
 */
export function scanClient(files) {
  const hits = [];
  for (const { path, text } of files) {
    let found = [];
    if (path.startsWith('src/') && path.endsWith('.ts') && !path.endsWith('.generated.ts')) found = scanLines(text.split('\n'), TS_RULES);
    else if (path.startsWith('static/') && /\.(js|html)$/.test(path)) found = scanLines(text.split('\n'), TS_RULES);
    else if (path.startsWith('src-tauri/src/') && path.endsWith('.rs')) found = scanLines(rustProduction(text), RS_RULES);
    else if (path === 'src-tauri/Cargo.toml') {
      for (const name of cargoDependencies(text)) {
        if (FORBIDDEN_CRATES.includes(name)) found.push({ rule: 'cargo-forbidden-crate', line: 0, text: name, why: 'a crate that reads rsi state or files directly' });
      }
    } else if (path === 'src-tauri/tauri.conf.json' || (path.startsWith('src-tauri/capabilities/') && path.endsWith('.json'))) {
      found = scanLines(text.split('\n'), CONFIG_RULES);
    } else if (path === 'package.json') {
      let deps = {};
      try { const j = JSON.parse(text); deps = { ...j.dependencies, ...j.devDependencies }; } catch { /* reported by npm itself */ }
      for (const name of Object.keys(deps)) {
        if (/^@tauri-apps\/plugin-(fs|shell|dialog|opener)$/.test(name)) found.push({ rule: 'npm-forbidden-plugin', line: 0, text: name, why: 'a Tauri plugin that reads files or runs programs from the webview' });
      }
    }
    for (const h of found) hits.push({ file: path, ...h });
  }
  return hits;
}

/**
 * Judge `hits` against the documented facilities. Each facility is
 * { file, rule, max, reason }. A hit is allowed while its file+rule has a
 * facility with a reason and no more than `max` hits; a facility nothing uses
 * is stale and reported, so the document cannot drift from the code.
 * Returns { violations: string[] }.
 */
export function judge(hits, facilities) {
  const violations = [];
  const byKey = new Map();
  for (const f of facilities ?? []) {
    if (!f || typeof f.file !== 'string' || typeof f.rule !== 'string') { violations.push(`facility needs file and rule: ${JSON.stringify(f)}`); continue; }
    if (typeof f.reason !== 'string' || f.reason.trim().length < 20) violations.push(`facility ${f.file} ${f.rule}: a reason of at least 20 characters is required`);
    if (!Number.isInteger(f.max) || f.max < 1) violations.push(`facility ${f.file} ${f.rule}: max must be a positive integer`);
    byKey.set(`${f.file}\0${f.rule}`, { ...f, used: 0 });
  }
  for (const h of hits) {
    const key = `${h.file}\0${h.rule}`;
    const f = byKey.get(key);
    if (!f) { violations.push(`${h.file}:${h.line} ${h.rule}: ${h.why}: ${h.text}`); continue; }
    f.used += 1;
    if (f.used === f.max + 1) violations.push(`${h.file} ${h.rule}: more than the ${f.max} documented use(s) (${f.reason.trim()})`);
  }
  for (const f of byKey.values()) {
    if (f.used === 0) violations.push(`stale facility ${f.file} ${f.rule}: nothing uses it any more; remove it from source-guard.json`);
  }
  return { violations };
}
