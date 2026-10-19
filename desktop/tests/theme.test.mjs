import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  DEFAULT_CHOICE, DESKTOP_DEFAULT_VARS, DESKTOP_VARS, allVarNames, assessRole, contrastRatio, cssVars, decodeChoice,
  encodeChoice, findTheme, parseHex, pickerEntries, pickerKey, resolveRoles, roleBaseline, setOverride, themeByKey,
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
  assert.equal(vars['--red'], p.red); // error
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
  assert.deepEqual(decodeChoice(null), DEFAULT_CHOICE);
  assert.deepEqual(decodeChoice('{'), DEFAULT_CHOICE);
  assert.deepEqual(decodeChoice('{"theme":"nope","overrides":{"error":"#zzzzzz","nope":"#000000","pin":"#AbCdEf"}}'), {
    theme: null, overrides: { pin: '#abcdef' },
  });
});

test('picker: desktop default first, 1-9 pick the TUI themes, Esc cancels', () => {
  const entries = pickerEntries();
  assert.equal(entries.length, THEMES.length + 1);
  assert.equal(entries[0].key, null);
  assert.equal(entries[1].key, 'goth');
  assert.deepEqual(pickerKey(0, entries.length, 'j'), { kind: 'move', index: 1 });
  assert.deepEqual(pickerKey(0, entries.length, 'k'), { kind: 'move', index: 0 });
  assert.deepEqual(pickerKey(3, entries.length, 'Enter'), { kind: 'apply', index: 3 });
  assert.deepEqual(pickerKey(0, entries.length, '1'), { kind: 'apply', index: 1 });
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
