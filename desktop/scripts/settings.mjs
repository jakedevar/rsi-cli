// Generate src/settings.generated.ts from the TUI's own sources, so the
// desktop settings pane, theme picker and role editor mirror the TUI without
// a hand-copied table (hub #3009, slice 7):
//
// - crates/rsi/src/settings_registry.rs: the settings groups, sections and
//   every `SETTINGS` row (label, summary, kind, owner, apply class);
// - crates/rsi/src/settings.rs `DaemonFeatureEntry::defaults()`: each daemon
//   row's widget (switch, choice list, display);
// - crates/rsi/src/ui/theme.rs: the built-in palettes, `THEME_DEFINITIONS` and
//   the semantic role baselines (`semantic_baseline`);
// - crates/rsi/src/ui/theme_roles.rs: the 17 semantic roles and their
//   contrast rules.
//
//   node scripts/settings.mjs          # rewrite src/settings.generated.ts
//   node scripts/settings.mjs --check  # exit 1 when it is stale
//
// The parser reads only the shapes these files use today and throws on
// anything else, so a TUI refactor fails `npm test` instead of drifting.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
export const OUT_PATH = join(root, 'src', 'settings.generated.ts');

export const SETTINGS_UPDATE_HINT = 'From the repository root, run `node desktop/scripts/settings.mjs` to regenerate desktop/src/settings.generated.ts. '
  + 'Map new rows in `widgetFor` / `OVERLAY_ROW_IDS` in desktop/src/settings-model.ts; intentional TUI-only rows also need '
  + '`TUI_ONLY` in desktop/tests/settings-state.test.mjs and `tuiOnlyRows` in desktop/parity-audit.json.';

export function sourcePaths(repo = join(root, '..')) {
  const tui = join(repo, 'crates', 'rsi', 'src');
  return {
    repo,
    registry: join(tui, 'settings_registry.rs'),
    settings: join(tui, 'settings.rs'),
    theme: join(tui, 'ui', 'theme.rs'),
    roles: join(tui, 'ui', 'theme_roles.rs'),
    textEditor: join(tui, 'overlay', 'daemon_text_editor.rs'),
    catalog: join(repo, 'crates', 'rsi-common', 'src', 'daemon_config_catalog.rs'),
  };
}

// ---------- Rust text helpers ----------

/** The text between the brace that opens at or after `from` and its match. */
export function braceBody(src, from, open = '{', close = '}') {
  const start = src.indexOf(open, from);
  if (start < 0) throw new Error(`no ${open} after offset ${from}`);
  let depth = 0;
  let inStr = false;
  for (let i = start; i < src.length; i++) {
    const c = src[i];
    if (inStr) {
      if (c === '\\') { i++; continue; }
      if (c === '"') inStr = false;
      continue;
    }
    if (c === '"') { inStr = true; continue; }
    if (c === '/' && src[i + 1] === '/') { const nl = src.indexOf('\n', i); i = nl < 0 ? src.length : nl; continue; }
    if (c === open) depth++;
    else if (c === close && --depth === 0) return src.slice(start + 1, i);
  }
  throw new Error(`unbalanced ${open}${close} from offset ${start}`);
}

/** Decode one Rust string literal body (between the quotes). */
export function rustString(body) {
  return body
    .replace(/\\\r?\n\s*/g, '')
    .replace(/\\(["\\nt'])/g, (_, c) => ({ n: '\n', t: '\t' })[c] ?? c);
}

const STR = '"((?:[^"\\\\]|\\\\.)*)"';

/** The body of `fn <name>` inside `impl <type> {` (first match). */
function implFn(src, type, name) {
  const impl = src.search(new RegExp(`impl ${type} \\{`));
  if (impl < 0) throw new Error(`no impl ${type}`);
  const at = src.slice(impl).search(new RegExp(`fn ${name}\\(`));
  if (at < 0) throw new Error(`no fn ${name} in impl ${type}`);
  return braceBody(src, impl + at);
}

/** `Self::Variant => "text"` arms (an arm may wrap its string in a block). */
function stringArms(body) {
  const out = new Map();
  for (const m of body.matchAll(new RegExp(`Self::(\\w+)\\s*=>\\s*\\{?\\s*${STR}`, 'g'))) {
    out.set(m[1], rustString(m[2]));
  }
  return out;
}

/** The variants of `pub enum <name> { ... }` in order. */
function enumVariants(src, name) {
  const at = src.search(new RegExp(`pub enum ${name} \\{`));
  if (at < 0) throw new Error(`no enum ${name}`);
  return braceBody(src, at)
    .split('\n')
    .map((l) => l.replace(/\/\/.*$/, '').trim())
    .filter((l) => /^\w+,?$/.test(l))
    .map((l) => l.replace(',', ''));
}

const snake = (s) => s.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toLowerCase();

// ---------- settings registry ----------

export function parseRegistry(src) {
  const groupLabels = stringArms(implFn(src, 'SettingsGroup', 'label'));
  const groupSummaries = stringArms(implFn(src, 'SettingsGroup', 'summary'));
  const groups = enumVariants(src, 'SettingsGroup').map((v) => {
    if (!groupLabels.has(v) || !groupSummaries.has(v)) throw new Error(`group ${v} has no label or summary`);
    return { id: snake(v), label: groupLabels.get(v), summary: groupSummaries.get(v) };
  });

  const groupBody = implFn(src, 'SettingsSection', 'group');
  const sectionGroup = new Map();
  for (const m of groupBody.matchAll(/((?:Self::\w+\s*\|?\s*)+)=>\s*\{?\s*SettingsGroup::(\w+)/g)) {
    for (const v of m[1].matchAll(/Self::(\w+)/g)) sectionGroup.set(v[1], snake(m[2]));
  }
  const sectionLabels = stringArms(implFn(src, 'SettingsSection', 'label'));
  const sectionSummaries = stringArms(implFn(src, 'SettingsSection', 'summary'));
  const sections = enumVariants(src, 'SettingsSection').map((v) => {
    const group = sectionGroup.get(v);
    if (!group || !sectionLabels.has(v) || !sectionSummaries.has(v)) throw new Error(`section ${v} is incomplete`);
    return { id: snake(v), group, label: sectionLabels.get(v), summary: sectionSummaries.get(v) };
  });

  const at = src.indexOf('pub static SETTINGS: &[SettingSpec] = &[');
  if (at < 0) throw new Error('no SETTINGS table');
  const table = braceBody(src, at + 'pub static SETTINGS: &[SettingSpec] = &'.length, '[', ']');
  const rows = [];
  for (const chunk of table.split(/SettingSpec \{/).slice(1)) {
    const block = chunk.replace(/\s+/g, ' ');
    const one = (re, what) => {
      const m = block.match(re);
      if (!m) throw new Error(`a SETTINGS row has no ${what}: ${block.slice(0, 120)}`);
      return m;
    };
    const id = one(/id: SettingId::(\w+)/, 'id')[1];
    const section = snake(one(/section: SettingsSection::(\w+)/, 'section')[1]);
    const label = rustString(one(new RegExp(`label: ${STR}`), 'label')[1]);
    const summary = rustString(one(new RegExp(`summary: ${STR}`), 'summary')[1]);
    const detailM = block.match(new RegExp(`detail: Some\\( ?${STR}`));
    const kind = snake(one(/kind: SettingKind::(\w+)/, 'kind')[1]);
    const ownerM = one(/owner: SettingOwner::(\w+)(?: ?\(([^)]*)\))?/, 'owner');
    const ownerArgs = [...(ownerM[2] ?? '').matchAll(new RegExp(STR, 'g'))].map((m) => rustString(m[1]));
    const owner = { kind: snake(ownerM[1]), ...(ownerArgs.length ? { args: ownerArgs } : {}) };
    const applyM = one(/apply: SettingApply::(\w+)(?: ?\( ?ApplyClass::(\w+) ?\))?/, 'apply');
    const apply = applyM[2] ? snake(applyM[2]) : snake(applyM[1]);
    const destructive = one(/destructive: (true|false)/, 'destructive')[1] === 'true';
    const keywords = [...(block.match(/keywords: &\[([^\]]*)\]/)?.[1] ?? '').matchAll(new RegExp(STR, 'g'))].map((m) => rustString(m[1]));
    if (!sections.some((s) => s.id === section)) throw new Error(`row ${id} names unknown section ${section}`);
    rows.push({
      id: snake(id), section, label, summary,
      ...(detailM ? { detail: rustString(detailM[1]) } : {}),
      keywords, kind, owner, apply, destructive,
    });
  }
  if (rows.length === 0) throw new Error('SETTINGS has no rows');
  return { groups, sections, rows };
}

// ---------- daemon feature widgets ----------

/**
 * Resolve `rsi_common::<module path>::<CONST>`: a `&str` const to its string,
 * or (with `list`) a `&[&str]` const to its strings, whose items may name
 * `&str` consts of the same module. Null when the const is not of that shape.
 */
export function commonConstResolver(repo = join(root, '..')) {
  const moduleText = (parts) => {
    const mod = parts.slice(1, -1).join('/');
    const base = join(repo, 'crates', 'rsi-common', 'src');
    for (const file of [join(base, `${mod}.rs`), join(base, mod, 'mod.rs')]) {
      try { return readFileSync(file, 'utf8'); } catch { /* next */ }
    }
    return null;
  };
  const strConst = (text, name) => {
    const m = text.match(new RegExp(`pub const ${name}: &str = ${STR};`));
    return m ? rustString(m[1]) : null;
  };
  const resolve = (path, list = false) => {
    const parts = path.split('::');
    if (parts[0] !== 'rsi_common' || parts.length < 3) throw new Error(`cannot resolve ${path}`);
    const name = parts[parts.length - 1];
    const text = moduleText(parts);
    if (text === null) {
      if (list) return null;
      throw new Error(`cannot resolve ${path}`);
    }
    if (!list) {
      const v = strConst(text, name);
      if (v === null) throw new Error(`cannot resolve ${path}`);
      return v;
    }
    const m = text.match(new RegExp(`pub const ${name}: &\\[&str\\] = &\\[([^\\]]*)\\];`));
    if (!m) return null;
    const items = [];
    for (const item of m[1].split(',').map((t) => t.trim()).filter(Boolean)) {
      const lit = item.match(new RegExp(`^${STR}$`));
      const v = lit ? rustString(lit[1]) : /^[A-Z0-9_]+$/.test(item) ? strConst(text, item) : null;
      if (v === null) return null;
      items.push(v);
    }
    return items;
  };
  return resolve;
}

export function parseDaemonDefaults(src, resolve = commonConstResolver()) {
  const at = src.indexOf('pub fn defaults() -> Vec<Self>');
  if (at < 0) throw new Error('no DaemonFeatureEntry::defaults()');
  const body = braceBody(src, at);
  const out = {};
  // `Self::rotation_threshold("field", "label")`: "Default" then 1..=99.
  const rot = [...body.matchAll(new RegExp(`Self::rotation_threshold\\(\\s*${STR},\\s*${STR}\\s*,?\\s*\\)`, 'g'))];
  if (rot.length > 0 && !/fn rotation_threshold[\s\S]*?"Default"[\s\S]*?\(1\.\.=99\)/.test(src)) {
    throw new Error('rotation_threshold no longer builds "Default" + 1..=99');
  }
  for (const m of rot) {
    out[rustString(m[1])] = { label: rustString(m[2]), widget: 'choice', options: ['Default', ...Array.from({ length: 99 }, (_, i) => String(i + 1))] };
  }
  for (const chunk of body.split(/Self \{\s*(?=field:)/).slice(1)) {
    const field = chunk.match(new RegExp(`field: ${STR}`));
    const fieldConst = chunk.match(/field: (rsi_common(?:::\w+)+)\.to_string\(\)/);
    const label = chunk.match(new RegExp(`label: ${STR}`));
    if (!(field || fieldConst) || !label) throw new Error(`a daemon default has no field or label: ${chunk.slice(0, 120)}`);
    const name = field ? rustString(field[1]) : resolve(fieldConst[1]);
    let entry;
    if (/value:\s*DaemonFeatureValue::Bool\(/.test(chunk)) entry = { widget: 'switch' };
    else if (/value:\s*DaemonFeatureValue::Display\(/.test(chunk)) entry = { widget: 'display' };
    else if (/value:\s*DaemonFeatureValue::Cycle\s*\{/.test(chunk)) {
      const opts = chunk.slice(chunk.indexOf('options:'), chunk.indexOf('current:'));
      const literal = /^options:\s*(?:vec!)?\[/.test(opts.trim());
      const options = literal ? [...opts.matchAll(new RegExp(STR, 'g'))].map((m) => rustString(m[1])) : [];
      const constList = opts.match(/^options:\s*(rsi_common(?:::\w+)+)\s*\.iter\(\)/);
      if (options.length === 0 && constList) options.push(...(resolve(constList[1], true) ?? []));
      entry = options.length > 0 ? { widget: 'choice', options } : { widget: 'choice-dynamic' };
    } else throw new Error(`daemon default ${name} has an unknown value shape`);
    out[name] = { label: rustString(label[1]), ...entry };
  }
  return out;
}

// ---------- themes ----------

function rgbHex(r, g, b) {
  return `#${[r, g, b].map((n) => Number(n).toString(16).padStart(2, '0')).join('')}`;
}

/**
 * A terminal's named ANSI colour (the terminal decides it; these are the xterm
 * defaults the desktop shows for a "truly transparent" theme's named slots).
 */
const ANSI = {
  'Color::Black': '#000000', 'Color::Red': '#cd0000', 'Color::Green': '#00cd00', 'Color::Yellow': '#cdcd00',
  'Color::Blue': '#0000ee', 'Color::Magenta': '#cd00cd', 'Color::Cyan': '#00cdcd', 'Color::Gray': '#e5e5e5',
  'Color::DarkGray': '#7f7f7f', 'Color::LightRed': '#ff0000', 'Color::LightGreen': '#00ff00',
  'Color::LightYellow': '#ffff00', 'Color::LightBlue': '#5c5cff', 'Color::LightMagenta': '#ff00ff',
  'Color::LightCyan': '#00ffff', 'Color::White': '#ffffff',
};

function ansiOrNull(name) {
  if (name === 'Color::Reset') return null;
  if (!ANSI[name]) throw new Error(`unknown colour ${name}`);
  return ANSI[name];
}

export function parseThemes(src) {
  const palettes = {};
  for (const m of src.matchAll(/const (\w+_PALETTE): CustomPalette = CustomPalette \{/g)) {
    const body = braceBody(src, m.index + m[0].length - 1);
    const slots = {};
    for (const s of body.replace(/\/\/.*$/gm, '').matchAll(/(\w+):\s*(rgb\([^)]*\)|[^,]+),/g)) {
      const rgb = s[2].match(/^rgb\((\d+),\s*(\d+),\s*(\d+)\)$/);
      if (rgb) slots[s[1]] = rgbHex(rgb[1], rgb[2], rgb[3]);
      else if (s[2].trim() === 'Color::Reset') slots[s[1]] = null;
      else if (ANSI[s[2].trim()]) slots[s[1]] = ANSI[s[2].trim()];
      else throw new Error(`${m[1]}.${s[1]} is not rgb(..) or Color::Reset: ${s[2]}`);
    }
    palettes[m[1]] = slots;
  }
  const at = src.search(/const THEME_DEFINITIONS: \[ThemeDefinition; \d+\] = \[/);
  if (at < 0) throw new Error('no THEME_DEFINITIONS');
  const defs = braceBody(src, src.indexOf('=', at) + 1, '[', ']');
  const themes = [];
  const re = new RegExp(
    `ThemeDefinition::new\\(\\s*${STR},\\s*${STR},\\s*&(\\w+),\\s*(\\d+),\\s*(?:rgb\\((\\d+),\\s*(\\d+),\\s*(\\d+)\\)|(Color::\\w+)),\\s*BorderPolicy::(\\w+),\\s*TransparencyPolicy::(\\w+),?\\s*\\)`,
    'g',
  );
  for (const m of defs.matchAll(re)) {
    const palette = palettes[m[3]];
    if (!palette) throw new Error(`theme ${m[1]} names unknown palette ${m[3]}`);
    themes.push({
      key: rustString(m[1]), name: rustString(m[2]), palette: m[3],
      primary: m[8] ? ansiOrNull(m[8]) : rgbHex(m[5], m[6], m[7]),
      transparency: snake(m[10]),
    });
  }
  const declared = Number(src.match(/const THEME_DEFINITIONS: \[ThemeDefinition; (\d+)\]/)[1]);
  if (themes.length !== declared) throw new Error(`parsed ${themes.length} of ${declared} THEME_DEFINITIONS`);

  // semantic_baseline: role -> palette slot, the theme primary, or the border rule.
  const base = braceBody(src, src.indexOf('fn semantic_baseline('));
  const baseline = {};
  for (const m of base.matchAll(/((?:ThemeRole::\w+\s*\|?\s*)+)=>\s*(\{[\s\S]*?\n {8}\}|[^,\n]+),?/g)) {
    const arm = m[2];
    let source;
    const slot = arm.match(/^palette\.(\w+)$/);
    if (slot) source = { slot: slot[1] };
    else if (/^active_primary_color\(\)$/.test(arm.trim())) source = { primary: true };
    else {
      const t = arm.match(/is_transparent_theme\(\)\s*\{\s*Color::White\s*\}\s*else\s*\{\s*palette\.(\w+)\s*\}/);
      if (!t) throw new Error(`semantic_baseline arm not understood: ${arm}`);
      source = { slot: t[1], transparent: '#ffffff' };
    }
    for (const r of m[1].matchAll(/ThemeRole::(\w+)/g)) baseline[snake(r[1])] = source;
  }
  return { palettes, themes, baseline };
}

export function parseRoles(src) {
  const order = (src.match(/pub const ALL: \[Self; \d+\] = \[([\s\S]*?)\];/)?.[1] ?? '')
    .match(/Self::\w+/g)?.map((s) => s.slice(6)) ?? [];
  if (order.length === 0) throw new Error('no ThemeRole::ALL');
  const body = braceBody(src, src.indexOf('pub const fn descriptor(self)'));
  const flat = body.replace(/\s+/g, ' ');
  const roles = order.map((v) => {
    const m = flat.match(new RegExp(`Self::${v} => \\{? ?descriptor\\( ?self, ${STR}, ${STR}, (\\w+), (None|Some\\(Self::(\\w+)\\)), ([\\d.]+),? ?\\)`));
    if (!m) throw new Error(`role ${v} has no descriptor`);
    return {
      key: rustString(m[1]), label: rustString(m[2]), kind: snake(m[3]),
      against: m[5] ? snake(m[5]) : null, minimum: Number(m[6]),
    };
  });
  return roles;
}

// ---------- apply labels and text editors ----------

/** When a change applies, as the TUI's info card says it (`SettingApply::label`). */
export function parseApplyLabels(registrySrc, catalogSrc) {
  const out = {};
  for (const [k, v] of stringArms(implFn(catalogSrc, 'ApplyClass', 'label'))) out[snake(k)] = v;
  for (const [k, v] of stringArms(implFn(registrySrc, 'SettingApply', 'label'))) out[snake(k)] = v;
  if (!out.live || !out.immediate) throw new Error('apply labels are incomplete');
  return out;
}

/** The TUI's Daemon Text Setting editors (`DAEMON_TEXT_FIELDS`). */
export function parseTextEditors(src, resolve) {
  const consts = new Map();
  for (const m of src.matchAll(new RegExp(`pub const (\\w+): &str = (?:${STR}|(rsi_common(?:::\\w+)+));`, 'g'))) {
    consts.set(m[1], m[2] !== undefined ? rustString(m[2]) : resolve(m[3]));
  }
  const at = src.indexOf('pub const DAEMON_TEXT_FIELDS: &[DaemonTextField] = &[');
  if (at < 0) throw new Error('no DAEMON_TEXT_FIELDS');
  const table = braceBody(src, at + 'pub const DAEMON_TEXT_FIELDS: &[DaemonTextField] = &'.length, '[', ']');
  const out = [];
  for (const chunk of table.split(/DaemonTextField \{/).slice(1)) {
    const fieldRef = chunk.match(/field: (\w+),/);
    const field = fieldRef ? consts.get(fieldRef[1]) : undefined;
    if (!field) throw new Error(`a DAEMON_TEXT_FIELDS entry has an unknown field: ${chunk.slice(0, 80)}`);
    const get = (name) => {
      const m = chunk.match(new RegExp(`${name}: ${STR}`));
      if (!m) throw new Error(`text editor ${field} has no ${name}`);
      return rustString(m[1]);
    };
    out.push({ field, title: get('title'), prompt: get('prompt'), hint: get('typing_hint'), defaultLabel: get('default_label') });
  }
  return out;
}

// ---------- output ----------

export function buildModel(paths = sourcePaths()) {
  const read = (p) => readFileSync(p, 'utf8');
  const registry = parseRegistry(read(paths.registry));
  const daemon = parseDaemonDefaults(read(paths.settings), commonConstResolver(paths.repo));
  const themes = parseThemes(read(paths.theme));
  const roles = parseRoles(read(paths.roles));
  const applyLabels = parseApplyLabels(read(paths.registry), read(paths.catalog));
  const textEditors = parseTextEditors(read(paths.textEditor), commonConstResolver(paths.repo));
  for (const role of roles) if (!themes.baseline[role.key]) throw new Error(`role ${role.key} has no semantic baseline`);
  return { registry, daemon, themes, roles, applyLabels, textEditors };
}

const json = (v) => JSON.stringify(v);
const lines = (items) => items.map((i) => `  ${json(i)},`).join('\n');

export function renderSettings(model) {
  const { registry, daemon, themes, roles, applyLabels, textEditors } = model;
  const palettes = Object.entries(themes.palettes).map(([k, v]) => `  ${json(k)}: ${json(v)},`).join('\n');
  const widgets = Object.entries(daemon).map(([k, v]) => `  ${json(k)}: ${json(v)},`).join('\n');
  const baseline = Object.entries(themes.baseline).map(([k, v]) => `  ${json(k)}: ${json(v)},`).join('\n');
  return `// Generated by desktop/scripts/settings.mjs from crates/rsi/src/settings_registry.rs,
// settings.rs (DaemonFeatureEntry::defaults), ui/theme.rs and ui/theme_roles.rs;
// do not edit. Regenerate with \`cd desktop && node scripts/settings.mjs\` after
// the TUI's settings or themes change (npm test fails while this file is stale).

export type SettingKind = 'bool' | 'cycle' | 'edit' | 'action' | 'read_only' | 'dynamic_list';
export type OwnerKind = 'tui' | 'daemon' | 'daemon_fields' | 'daemon_state' | 'file';
export type ApplyKind =
  | 'immediate' | 'on_apply' | 'bridge_restart'
  | 'live' | 'next_spawn' | 'partial_live' | 'live_off_restart_on' | 'daemon_restart' | 'not_applied';

export interface GroupRow { id: string; label: string; summary: string }
export interface SectionRow { id: string; group: string; label: string; summary: string }
export interface SettingRow {
  id: string; section: string; label: string; summary: string; detail?: string;
  keywords: string[]; kind: SettingKind; owner: { kind: OwnerKind; args?: string[] };
  apply: ApplyKind; destructive: boolean;
}
export type DaemonWidget =
  | { label: string; widget: 'switch' | 'display' | 'choice-dynamic' }
  | { label: string; widget: 'choice'; options: string[] };
export interface ThemeRow { key: string; name: string; palette: string; primary: string | null; transparency: string }
export type RoleSource = { slot: string; transparent?: string } | { primary: true };
export interface RoleRow { key: string; label: string; kind: string; against: string | null; minimum: number }

export const SETTINGS_GROUPS: readonly GroupRow[] = [
${lines(registry.groups)}
];

export const SETTINGS_SECTIONS: readonly SectionRow[] = [
${lines(registry.sections)}
];

export const SETTINGS_ROWS: readonly SettingRow[] = [
${lines(registry.rows)}
];

/** The TUI's widget for each daemon config field it lists. */
export const DAEMON_WIDGETS: Readonly<Record<string, DaemonWidget>> = {
${widgets}
};

/** Built-in palettes (null: the terminal's default colour). */
export const PALETTES: Readonly<Record<string, Readonly<Record<string, string | null>>>> = {
${palettes}
};

export const THEMES: readonly ThemeRow[] = [
${lines(themes.themes)}
];

/** Where each semantic role's colour comes from before an override. */
export const ROLE_BASELINE: Readonly<Record<string, RoleSource>> = {
${baseline}
};

export const THEME_ROLES: readonly RoleRow[] = [
${lines(roles)}
];

/** When a change applies (\`SettingApply::label\` / \`ApplyClass::label\`). */
export const APPLY_LABELS: Readonly<Record<ApplyKind, string>> = ${json(applyLabels)};

export interface TextEditorRow { field: string; title: string; prompt: string; hint: string; defaultLabel: string }

/** The TUI's Daemon Text Setting editors: the value is sent as a string, "" for the default. */
export const TEXT_EDITORS: readonly TextEditorRow[] = [
${lines(textEditors)}
];
`;
}

export function settingsIsFresh(paths = sourcePaths(), out = OUT_PATH) {
  const expected = renderSettings(buildModel(paths));
  let current = '';
  try { current = readFileSync(out, 'utf8'); } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  return current === expected;
}

export function checkSettings(paths = sourcePaths(), out = OUT_PATH) {
  if (!settingsIsFresh(paths, out)) {
    throw new Error(`desktop/src/settings.generated.ts is stale. ${SETTINGS_UPDATE_HINT}`);
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  if (process.argv.includes('--check')) {
    try { checkSettings(); } catch (error) {
      console.error(error.message);
      process.exit(1);
    }
  } else {
    writeFileSync(OUT_PATH, renderSettings(buildModel()));
    console.log(`wrote ${OUT_PATH}`);
  }
}
