import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  DEFAULT_CHOICE, DESKTOP_DEFAULT_VARS, DESKTOP_VARS, FIRST_RUN_CHOICE, allVarNames, assessRole, colorSchemeOf,
  contrastRatio, cssVars, decodeChoice, effectiveChoice, encodeChoice, findTheme, isMatchSystemName, onColor,
  parseHex, pickerEntries, pickerIndexOf, pickerKey, readableOn, resolveRoles, roleBaseline, setOverride, systemSwatch, themeByKey,
  unmappedRoles,
} from '../dist/theme-model.js';
import { PALETTES, THEMES, THEME_ROLES } from '../dist/settings.generated.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

test('the TUI themes and roles are generated from ui/theme.rs and theme_roles.rs', () => {
  assert.equal(THEMES.length, 14);
  assert.equal(THEMES[0].key, 'goth');
  assert.equal(THEMES[0].name, 'Goth');
  assert.equal(THEME_ROLES.length, 17);
  assert.deepEqual(THEME_ROLES.find((r) => r.key === 'primary_text'), {
    key: 'primary_text', label: 'Primary text', kind: 'text', against: 'canvas', minimum: 4.5,
  });
  for (const t of THEMES) assert.ok(PALETTES[t.palette], t.key);
});

test('a theme maps its palette onto the desktop CSS variables through the roles', () => {
  const goth = themeByKey('goth');
  const p = PALETTES[goth.palette];
  const vars = cssVars({ theme: 'goth', overrides: {} });
  assert.equal(vars['--base'], p.base); // canvas
  assert.equal(vars['--mantle'], p.tier_panel); // panel
  assert.equal(vars['--crust'], p.tier_raised); // elevated surface
  assert.equal(vars['--text'], p.text); // primary text
  assert.equal(vars['--subtext'], p.subtext1); // muted text
  assert.equal(vars['--blue'], goth.primary); // accent = the theme's primary
  assert.equal(vars['--green'], p.green); // running
  assert.equal(vars['--red'], readableOn(p.red, ['--base', '--mantle', '--crust', '--surface0', '--surface1'].map((v) => vars[v]))); // error, lifted to 4.5:1 on every surface (local #283)
  assert.equal(vars['--green'], p.green); // already reads: untouched
  assert.equal(vars['--yellow'], p.yellow); // a palette slot
  assert.equal(vars['--role-waiting'], p.pink);
  for (const v of Object.keys(DESKTOP_VARS)) assert.match(vars[v], /^#[0-9a-f]{6}$/, v);
});

test('a role override replaces its role and every variable it drives', () => {
  const choice = setOverride({ theme: 'goth', overrides: {} }, 'accent', '#123456');
  assert.equal(resolveRoles(choice).accent, '#123456');
  assert.equal(cssVars(choice)['--blue'], '#123456');
  assert.equal(resolveRoles(choice).focused_border, themeByKey('goth').primary);
  assert.deepEqual(setOverride(choice, 'accent', null).overrides, {});
});

test('the desktop default keeps the stylesheet and sets only overridden roles', () => {
  assert.deepEqual(cssVars(DEFAULT_CHOICE), {});
  const vars = cssVars(setOverride(DEFAULT_CHOICE, 'canvas', '#000000'));
  assert.deepEqual(vars, { '--base': '#000000', '--role-canvas': '#000000' });
  assert.equal(roleBaseline(undefined, 'primary_text'), DESKTOP_DEFAULT_VARS['--text']);
});

test('DESKTOP_DEFAULT_VARS match static/styles.css', () => {
  const css = readFileSync(here('../static/styles.css'), 'utf8');
  const root = css.slice(css.indexOf(':root'), css.indexOf('}', css.indexOf(':root')));
  for (const [v, hex] of Object.entries(DESKTOP_DEFAULT_VARS)) {
    const m = root.match(new RegExp(`${v}:\\s*(#[0-9a-fA-F]{6})`));
    assert.ok(m, `${v} is not in :root`);
    assert.equal(m[1].toLowerCase(), hex, v);
  }
  for (const v of Object.keys(DESKTOP_VARS)) assert.ok(v in DESKTOP_DEFAULT_VARS, v);
});

test('transparent themes use white borders (semantic_baseline)', () => {
  assert.equal(roleBaseline(themeByKey('transparent'), 'border'), '#ffffff');
  assert.equal(roleBaseline(themeByKey('goth'), 'border'), PALETTES.GOTH_PALETTE.dark_purple);
});

test('contrast is the WCAG ratio and a low one is flagged', () => {
  assert.ok(Math.abs(contrastRatio('#000000', '#ffffff') - 21) < 1e-9);
  assert.equal(contrastRatio('#777777', '#777777'), 1);
  const light = { theme: 'light', overrides: {} };
  assert.equal(assessRole(light, 'primary_text', '#ffffff').kind, 'low');
  assert.equal(assessRole(light, 'primary_text', '#000000').kind, 'readable');
  assert.equal(assessRole(light, 'canvas', '#ffffff').kind, 'none');
  assert.deepEqual(assessRole(light, 'accent', 'nope'), { kind: 'invalid', reason: 'expected #RRGGBB' });
  assert.equal(parseHex(' #ABCDEF '), '#abcdef');
  assert.equal(parseHex('abc'), null);
});

test('the choice persists, and a bad stored value falls back', () => {
  const choice = setOverride({ theme: 'ruby', overrides: {} }, 'error', '#ff0000');
  assert.deepEqual(decodeChoice(encodeChoice(choice)), choice);
  // Nothing stored (a new install) or unreadable text matches the system.
  assert.deepEqual(decodeChoice(null), FIRST_RUN_CHOICE);
  assert.deepEqual(decodeChoice('{'), FIRST_RUN_CHOICE);
  assert.deepEqual(decodeChoice('{"theme":"nope","overrides":{"error":"#zzzzzz","nope":"#000000","pin":"#AbCdEf"}}'), {
    theme: null, overrides: { pin: '#abcdef' },
  });
});

test('picker: desktop default first, 1-9 pick the TUI themes, Esc cancels', () => {
  const entries = pickerEntries();
  assert.equal(entries.length, THEMES.length + 2);
  assert.equal(entries[0].system, true);
  assert.equal(entries[0].name, 'Match system (light/dark)');
  assert.equal(entries[1].key, null);
  assert.equal(entries[2].key, 'goth');
  assert.deepEqual(pickerKey(0, entries.length, 'j'), { kind: 'move', index: 1 });
  assert.deepEqual(pickerKey(0, entries.length, 'k'), { kind: 'move', index: 0 });
  assert.deepEqual(pickerKey(3, entries.length, 'Enter'), { kind: 'apply', index: 3 });
  assert.deepEqual(pickerKey(0, entries.length, '1'), { kind: 'apply', index: 2 });
  assert.deepEqual(pickerKey(0, entries.length, 'Escape'), { kind: 'cancel' });
  assert.deepEqual(pickerKey(0, entries.length, 'G'), { kind: 'move', index: entries.length - 1 });
});

test(':theme finds a theme by key or name', () => {
  assert.equal(findTheme('Junk Yard').key, 'junkyard');
  assert.equal(findTheme('cup-a-joe').key, 'cup-a-joe');
  assert.equal(findTheme('GOTH').key, 'goth');
  assert.equal(findTheme('nope'), undefined);
});

test('every role has a variable or is reported as unmapped', () => {
  const mapped = new Set(Object.values(DESKTOP_VARS).filter((s) => 'role' in s).map((s) => s.role));
  const unmapped = unmappedRoles();
  for (const r of THEME_ROLES) assert.ok(mapped.has(r.key) !== unmapped.includes(r.key), r.key);
  assert.ok(allVarNames().includes('--role-toast'));
});

// ---------- Match system (#274) ----------

test('Match system resolves to Light when the system is light and the desktop default when it is dark', () => {
  assert.equal(effectiveChoice(FIRST_RUN_CHOICE, true).theme, 'light');
  assert.equal(effectiveChoice(FIRST_RUN_CHOICE, false).theme, null);
  // An explicit theme ignores the system, in both directions.
  const goth = { theme: 'goth', overrides: {} };
  assert.equal(effectiveChoice(goth, true), goth);
  assert.equal(effectiveChoice(goth, false), goth);
  assert.equal(effectiveChoice({ theme: null, overrides: {} }, true).theme, null);
});

test('a stored choice keeps its theme; only an explicit "system" follows the system', () => {
  assert.deepEqual(decodeChoice('{"theme":"ruby","overrides":{}}'), { theme: 'ruby', overrides: {} });
  // A stored Desktop default is an explicit pick (a user who chose it keeps it).
  assert.deepEqual(decodeChoice('{"theme":null,"overrides":{}}'), { theme: null, overrides: {} });
  const sys = { theme: null, overrides: { pin: '#abcdef' }, system: true };
  assert.deepEqual(decodeChoice(encodeChoice(sys)), sys);
  assert.ok(!encodeChoice({ theme: 'ruby', overrides: {} }).includes('system'));
  // "system" wins over a stale theme key.
  assert.deepEqual(decodeChoice('{"theme":"goth","overrides":{},"system":true}'), { theme: null, overrides: {}, system: true });
});

test('the picker offers Match system first and marks the choice it sits on', () => {
  assert.equal(pickerIndexOf(FIRST_RUN_CHOICE), 0);
  assert.equal(pickerIndexOf({ theme: null, overrides: {} }), 1);
  assert.equal(pickerIndexOf({ theme: 'goth', overrides: {} }), 2);
  assert.equal(systemSwatch().length, 8);
  for (const n of ['system', 'Match system', 'match-system', ' SYSTEM ']) assert.ok(isMatchSystemName(n), n);
  assert.ok(!isMatchSystemName('systems'));
});

test('color-scheme follows the canvas the theme paints', () => {
  assert.equal(colorSchemeOf({ theme: null, overrides: {} }), 'dark');
  assert.equal(colorSchemeOf({ theme: 'light', overrides: {} }), 'light');
  assert.equal(colorSchemeOf({ theme: 'high-contrast', overrides: {} }), 'dark');
  for (const t of THEMES) {
    const canvas = resolveRoles({ theme: t.key, overrides: {} }).canvas;
    assert.equal(colorSchemeOf({ theme: t.key, overrides: {} }), contrastRatio(canvas, '#000000') > contrastRatio(canvas, '#ffffff') ? 'light' : 'dark', t.key);
  }
  // A canvas override flips it, under any theme.
  assert.equal(colorSchemeOf(setOverride({ theme: null, overrides: {} }, 'canvas', '#ffffff')), 'light');
});

test('onColor keeps the dark surface when it reads, else the text colour, else black or white', () => {
  assert.equal(onColor('#89b4fa', '#11111b', '#cdd6f4'), '#11111b');
  assert.equal(onColor('#946300', '#e6e3db', '#1a1a1a'), '#ffffff'); // the Light badge: crust fails (4.05)
  assert.equal(onColor('#c82828', '#e6e3db', '#ffffff'), '#ffffff');
  for (const fill of ['#777777', '#808080', '#767676', '#7f7f7f', '#948f4a']) {
    assert.ok(contrastRatio(onColor(fill, '#777777', '#808080'), fill) >= 4.5, fill);
  }
});
