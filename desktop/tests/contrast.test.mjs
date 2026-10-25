// WCAG AA contrast of the secondary-text token (#233): --muted must clear 4.5:1
// on the surfaces small text sits on, in the desktop default and every built-in
// theme, and no text rule may use --overlay0 (borders, dots and strokes only).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ADJUSTED_TEXT_VARS, DEFAULT_CHOICE, DESKTOP_DEFAULT_VARS, DESKTOP_VARS, ON_ACCENT_VARS, TEXT_SURFACES, contrastRatio, cssVars, readableOn, resolveRoles } from '../dist/theme-model.js';
import { PALETTES, THEMES } from '../dist/settings.generated.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const SURFACES = ['--base', '--mantle', '--surface0'];
const AA_TEXT = 4.5;

/** The effective CSS variables of a theme (`null` = the stylesheet's own). */
function varsOf(theme) {
  return theme === null
    ? { ...DESKTOP_DEFAULT_VARS, ...cssVars(DEFAULT_CHOICE) }
    : cssVars({ theme, overrides: {} });
}

for (const theme of [null, ...THEMES.map((t) => t.key)]) {
  test(`--muted text clears 4.5:1 on base, mantle and surface0 (${theme ?? 'desktop default'})`, () => {
    const vars = varsOf(theme);
    for (const surface of SURFACES) {
      const ratio = contrastRatio(vars['--muted'], vars[surface]);
      assert.ok(ratio >= AA_TEXT, `--muted on ${surface}: ${ratio.toFixed(2)}:1`);
    }
  });
}

test('the old --overlay0 pairing is what the audit measured (below 4.5:1 on surface0)', () => {
  assert.ok(contrastRatio(DESKTOP_DEFAULT_VARS['--overlay0'], DESKTOP_DEFAULT_VARS['--surface0']) < AA_TEXT);
});

test('no text rule uses --overlay0 (borders, dots and strokes only)', () => {
  const srcDir = here('../src/');
  const files = [
    ...readdirSync(srcDir).filter((f) => f.endsWith('.ts') && f !== 'settings.generated.ts' && f !== 'theme-model.ts').map((f) => srcDir + f),
    ...readdirSync(here('../static/')).filter((f) => f.endsWith('.css')).map((f) => here('../static/') + f),
  ];
  const offenders = [];
  for (const file of files) {
    readFileSync(file, 'utf8').split('\n').forEach((line, i) => {
      if (/(?<![-\w])color:\s*var\(--overlay0/.test(line)) offenders.push(`${file}:${i + 1}`);
    });
  }
  assert.deepEqual(offenders, []);
});

// The manager panes' cursor row (local #244): the old tint (--surface0 on --base) was 1.30:1. The row now
// carries a --blue bar, and the focused list a 2px --blue ring; a state indicator needs 3:1 (WCAG 1.4.11)
// against the surface it sits on, in the desktop default and every built-in theme.
const AA_UI = 3;

test('the old manager cursor tint was below 3:1 (what the audit measured)', () => {
  assert.ok(contrastRatio(DESKTOP_DEFAULT_VARS['--surface0'], DESKTOP_DEFAULT_VARS['--base']) < AA_UI);
});

for (const theme of [null, ...THEMES.map((t) => t.key)]) {
  test(`the manager cursor indicator (--blue) clears 3:1 on base and on the selected row (${theme ?? 'desktop default'})`, () => {
    const vars = varsOf(theme);
    for (const against of ['--base', '--surface0']) {
      const ratio = contrastRatio(vars['--blue'], vars[against]);
      assert.ok(ratio >= AA_UI, `--blue on ${against}: ${ratio.toFixed(2)}:1`);
    }
  });
}

test('the manager list draws that indicator from theme tokens, not the faint background alone', () => {
  const css = readFileSync(here('../src/managers.ts'), 'utf8');
  assert.match(css, /\.mgr-item\.sel \{[^}]*box-shadow: inset 3px 0 var\(--blue\)/);
  assert.match(css, /\.mgr-list:focus \.mgr-item\.sel \{[^}]*outline: 2px solid var\(--blue\)/);
});

// ---------- Light and High Contrast (#274) ----------
// Audit 4 measured badges and red buttons at 4.05 and 4.32:1 in Light and 4.41:1 in High Contrast: text on a
// filled accent used the dark surface (--crust). Text on a fill now uses --on-<accent>, which a theme sets to
// the colour that reaches 4.5:1. Every text pair below must clear WCAG AA for normal text.

const GATED = [null, 'light', 'high-contrast'];
const ACCENTS = ['--blue', '--green', '--yellow', '--red', '--mauve', '--peach', '--teal'];

/** A theme's variables with the stylesheet's `--on-*: var(--crust)` fallback applied when the theme sets none. */
function withOn(theme) {
  const vars = varsOf(theme);
  for (const on of Object.keys(ON_ACCENT_VARS)) vars[on] ??= vars['--crust'];
  return vars;
}

for (const theme of GATED) {
  test(`body and secondary text clear 4.5:1 on every surface they sit on (${theme ?? 'desktop default'})`, () => {
    const vars = withOn(theme);
    for (const text of ['--text', '--subtext', '--muted']) {
      for (const surface of ['--base', '--mantle', '--crust', '--surface0']) {
        const ratio = contrastRatio(vars[text], vars[surface]);
        assert.ok(ratio >= AA_TEXT, `${text} on ${surface}: ${ratio.toFixed(2)}:1`);
      }
    }
    const onSelected = contrastRatio(vars['--text'], vars['--surface1']);
    assert.ok(onSelected >= AA_TEXT, `--text on --surface1: ${onSelected.toFixed(2)}:1`);
  });

  test(`accent text (links, status words, headings) clears 4.5:1 on the panes (${theme ?? 'desktop default'})`, () => {
    const vars = withOn(theme);
    for (const accent of ACCENTS) {
      for (const surface of ['--base', '--mantle', '--surface0']) {
        const ratio = contrastRatio(vars[accent], vars[surface]);
        assert.ok(ratio >= AA_TEXT, `${accent} on ${surface}: ${ratio.toFixed(2)}:1`);
      }
    }
    // The top bar and the key-hint box are --crust; they draw the brand, the key names and the hint head.
    for (const accent of ['--blue', '--mauve']) {
      const ratio = contrastRatio(vars[accent], vars['--crust']);
      assert.ok(ratio >= AA_TEXT, `${accent} on --crust: ${ratio.toFixed(2)}:1`);
    }
  });
}

for (const theme of [null, ...THEMES.map((t) => t.key)]) {
  test(`badge and button text on a filled accent clears 4.5:1 for every accent (${theme ?? 'desktop default'})`, () => {
    const vars = withOn(theme);
    for (const [on, fill] of Object.entries(ON_ACCENT_VARS)) {
      const ratio = contrastRatio(vars[on], vars[fill]);
      assert.ok(ratio >= AA_TEXT, `${on} on ${fill}: ${ratio.toFixed(2)}:1`);
    }
  });
}

test('what the audit measured: the dark surface on the Light and High Contrast fills was below 4.5:1', () => {
  const light = rawVars('light'); // the palette's own fills, before the contrast adjustment
  assert.ok(Math.abs(contrastRatio(light['--crust'], light['--yellow']) - 4.05) < 0.01); // .attn-badge
  assert.ok(Math.abs(contrastRatio(light['--crust'], light['--red']) - 4.32) < 0.01); // .btn.danger
  const hc = rawVars('high-contrast');
  assert.ok(Math.abs(contrastRatio(hc['--crust'], hc['--red']) - 4.41) < 0.01);
});

test('a role override of an accent or the dark surface re-derives the text colours', () => {
  const vars = cssVars({ theme: null, overrides: { error: '#c82828' } });
  assert.ok(contrastRatio(vars['--on-red'], '#c82828') >= AA_TEXT);
  // Nothing in --on-* moves when only an unrelated role changes.
  assert.deepEqual(Object.keys(cssVars({ theme: null, overrides: { canvas: '#000000' } })).filter((k) => k.startsWith('--on-')), []);
});

test('the stylesheet defines every --on-* variable and no filled accent uses --crust or --base for its text', () => {
  const root = readFileSync(here('../static/styles.css'), 'utf8');
  for (const on of Object.keys(ON_ACCENT_VARS)) assert.match(root, new RegExp(`${on}: var\\(--crust\\)`), on);
  const srcDir = here('../src/');
  const files = [
    ...readdirSync(srcDir).filter((f) => f.endsWith('.ts') && f !== 'settings.generated.ts').map((f) => srcDir + f),
    ...readdirSync(here('../static/')).filter((f) => f.endsWith('.css')).map((f) => here('../static/') + f),
  ];
  const offenders = [];
  for (const file of files) {
    readFileSync(file, 'utf8').split('\n').forEach((line, i) => {
      if (/(?<![-\w])background:\s*var\(--(blue|green|yellow|red|mauve|peach|teal)\)/.test(line)
        && /(?<![-\w])color:\s*var\(--(crust|base|mantle)\)/.test(line)) offenders.push(`${file}:${i + 1}`);
    });
  }
  assert.deepEqual(offenders, []);
});

// ---------- Every text pair on every surface (local #283) ----------
// #274 gated the pairs the UI drew; the leftovers were Light accents on --crust (4.05 to 4.32:1) and --surface1 (3.4 to 3.9),
// and Truly Transparent red and mauve (2.1 to 4.1). A built-in theme now moves its accent and secondary text colours toward
// white or black, the least that clears 4.5:1 on all five surfaces text sits on. The desktop default keeps its Catppuccin
// values: every pair clears 4.5:1 on the four pane surfaces, and --text on the selected row (--surface1), where the
// stylesheet draws no accent or secondary text (the headless walker finds 0 under AA on the list and the dialog).

const TEXT_VARS = ['--text', ...ADJUSTED_TEXT_VARS];

for (const theme of THEMES.map((t) => t.key)) {
  test(`every text colour clears 4.5:1 on base, mantle, crust, surface0 and the selected row (${theme})`, () => {
    const vars = withOn(theme);
    const low = [];
    for (const text of TEXT_VARS) for (const surface of TEXT_SURFACES) {
      const ratio = contrastRatio(vars[text], vars[surface]);
      if (ratio < AA_TEXT) low.push(`${text} on ${surface} ${ratio.toFixed(2)}`);
    }
    assert.deepEqual(low, []);
  });
}

test('the desktop default clears 4.5:1 on the pane surfaces, and --text on the selected row', () => {
  const vars = withOn(null);
  const low = [];
  for (const text of TEXT_VARS) for (const surface of TEXT_SURFACES) {
    if (surface === '--surface1' && text !== '--text') continue;
    const ratio = contrastRatio(vars[text], vars[surface]);
    if (ratio < AA_TEXT) low.push(`${text} on ${surface} ${ratio.toFixed(2)}`);
  }
  assert.deepEqual(low, []);
});

// PM decision on #283: the default dark's secondary and accent text should reach 4.5:1 on the selected row by lightening
// nothing in Catppuccin's text colours, only by darkening the row (--surface1, the `selected_row` role). Measured: the worst
// pair (--muted, 3.53:1) needs --surface1 near #36374a. That is 1.08:1 against --surface0 (#313244) and 1.41:1 against
// --base (was 1.80), so the row, every --surface1 border and every hover fill would sink into the pane: a visible identity
// change, so the default is left as is. These are the five pairs still under 4.5:1, pinned so none can get worse or spread.
test('the desktop default keeps exactly five text pairs under 4.5:1, all on the selected row, none under 3.5:1', () => {
  const vars = withOn(null);
  const low = [];
  for (const text of TEXT_VARS) for (const surface of TEXT_SURFACES) {
    const ratio = contrastRatio(vars[text], vars[surface]);
    if (ratio < AA_TEXT) low.push(`${text} on ${surface}`);
    if (ratio < 3.5) assert.fail(`${text} on ${surface}: ${ratio.toFixed(2)}:1`);
  }
  assert.deepEqual(low, ['--subtext on --surface1', '--muted on --surface1', '--blue on --surface1', '--red on --surface1', '--mauve on --surface1']);
});

test('darkening --surface1 to reach 4.5:1 for every pair would cost the row its edge against --surface0 and --base', () => {
  const vars = withOn(null);
  const needed = '#36374a';
  const worst = Math.min(...TEXT_VARS.map((text) => contrastRatio(vars[text], needed)));
  assert.ok(worst >= AA_TEXT, worst.toFixed(2));
  assert.ok(contrastRatio(needed, vars['--surface0']) < 1.1);
  assert.ok(contrastRatio(vars['--surface1'], vars['--base']) > 1.75);
  assert.ok(contrastRatio(needed, vars['--base']) < 1.45);
});

// #296: the selected Settings group's summary ("How does it look?") is a pair the UI really draws, on --surface1 while the rail
// has focus. The rule moves it to --text; the unselected rows and the unfocused selected row (--surface0) keep --muted.
const SETTINGS_CSS = readFileSync(here('../src/settings-css.ts'), 'utf8');
const ruleColor = (selector) => {
  const m = new RegExp(`^${selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')} \\{[^}]*?color: var\\((--[a-z0-9-]+)\\)`, 'm').exec(SETTINGS_CSS);
  return m ? m[1] : null;
};

test('the focused rail draws the selected group summary in --text, and the other rows keep --muted', () => {
  assert.equal(ruleColor('.settings-rail.focused .settings-group.sel .sg-summary'), '--text');
  assert.equal(ruleColor('.settings-group .sg-summary'), '--muted', 'unselected rows keep their look');
});

for (const theme of [null, ...THEMES.map((t) => t.key)]) {
  test(`the selected Settings group summary reads at 4.5:1 on every surface the rail draws it on (${theme ?? 'desktop default'})`, () => {
    const vars = withOn(theme);
    const selected = ruleColor('.settings-rail.focused .settings-group.sel .sg-summary');
    const plain = ruleColor('.settings-group .sg-summary');
    // Rail focused: the row is --surface1 and the summary --text. Rail not focused: the row is --surface0 and the summary --muted.
    const pairs = [[selected, '--surface1'], [plain, '--surface0'], [plain, '--mantle']];
    for (const [text, surface] of pairs) {
      const ratio = contrastRatio(vars[text], vars[surface]);
      assert.ok(ratio >= AA_TEXT, `${text} on ${surface}: ${ratio.toFixed(2)}:1`);
    }
  });
}

/** A theme's colours as its palette gives them, before the contrast adjustment. */
function rawVars(theme) {
  const roles = resolveRoles({ theme, overrides: {} });
  const palette = PALETTES[THEMES.find((t) => t.key === theme).palette];
  return Object.fromEntries(Object.entries(DESKTOP_VARS).map(([v, src]) => [v, 'role' in src ? roles[src.role] : palette[src.slot]]));
}

test('what #274 left: the Light and Transparent pairs under 4.5:1 now clear it, with the numbers before and after', () => {
  const rows = [
    // [theme, text variable, surface, ratio before (the palette's own colour), ratio now]
    ['light', '--green', '--crust', 4.10], ['light', '--yellow', '--crust', 4.05], ['light', '--red', '--crust', 4.32],
    ['light', '--peach', '--crust', 4.09], ['light', '--teal', '--crust', 4.08], ['light', '--blue', '--surface1', 3.84],
    ['light', '--yellow', '--surface1', 3.44], ['truly-transparent', '--red', '--mantle', 4.12],
    ['truly-transparent', '--red', '--crust', 3.25], ['truly-transparent', '--mauve', '--mantle', 3.51],
    ['truly-transparent', '--mauve', '--surface1', 2.07], ['transparent', '--red', '--surface1', 4.34],
  ];
  for (const [theme, text, surface, before] of rows) {
    const vars = withOn(theme);
    const was = rawVars(theme);
    assert.ok(Math.abs(contrastRatio(was[text], was[surface]) - before) < 0.01, `${theme} ${text} on ${surface} was ${contrastRatio(was[text], was[surface]).toFixed(2)}`);
    const now = contrastRatio(vars[text], vars[surface]);
    assert.ok(now >= AA_TEXT, `${theme} ${text} on ${surface}: ${now.toFixed(2)} (was ${before})`);
    assert.ok(before < AA_TEXT);
  }
});

test('a role override the operator typed is left as typed, and a colour that already reads is not touched', () => {
  const vars = cssVars({ theme: 'light', overrides: { error: '#ff8080' } });
  assert.equal(vars['--red'], '#ff8080');
  assert.equal(readableOn('#ffffff', ['#000000', '#111111']), '#ffffff');
});

test('readableOn moves a colour toward white on dark surfaces and toward black on light ones, by the least step', () => {
  const dark = ['#0e1016', '#404452'];
  const lifted = readableOn('#ff0000', dark);
  assert.ok(contrastRatio(lifted, '#404452') >= AA_TEXT);
  assert.ok(lifted.startsWith('#ff'), lifted);
  const light = ['#ffffff', '#e6e9ef'];
  const darkened = readableOn('#fe640b', light);
  assert.ok(contrastRatio(darkened, '#e6e9ef') >= AA_TEXT);
  assert.ok(contrastRatio(darkened, '#e6e9ef') < AA_TEXT + 0.3, 'the smallest step that clears, not a jump to black');
  // Surfaces that need opposite directions cannot be satisfied: the colour stays.
  assert.equal(readableOn('#808080', ['#000000', '#ffffff']), '#808080');
});

// ESP Square (#301): every glyph, the cursor and the flash text read against the card in every theme.
import { GLYPH_MIN_CONTRAST, HIT_COLOR, MISS_COLOR, SQUARE_COLORS, flashTextOn, squareColorsOn } from '../dist/esp-model.js';

for (const theme of [null, ...THEMES.map((t) => t.key)]) {
  test(`ESP Square glyphs clear 4.5:1 on the card and keep their hue side (${theme ?? 'desktop default'})`, () => {
    const vars = varsOf(theme);
    const colors = squareColorsOn(vars['--mantle'], vars['--text']);
    assert.equal(colors.length, SQUARE_COLORS.length);
    colors.forEach((c, i) => {
      const ratio = contrastRatio(c, vars['--mantle']);
      assert.ok(ratio >= GLYPH_MIN_CONTRAST, `square ${i} ${SQUARE_COLORS[i]} -> ${c} on ${vars['--mantle']}: ${ratio.toFixed(2)}:1`);
    });
    assert.ok(contrastRatio(vars['--text'], vars['--mantle']) >= 3, 'the cursor (theme text) reads');
  });
}

test('ESP Square flash text clears 4.5:1 on the hit and miss fills', () => {
  assert.ok(contrastRatio(flashTextOn(true), HIT_COLOR) >= AA_TEXT);
  assert.ok(contrastRatio(flashTextOn(false), MISS_COLOR) >= AA_TEXT);
});

test('ESP Square on the Light canvas: yellow was 1.08:1, now reads and stays a warm hue', () => {
  const light = varsOf('light');
  const [, , yellow] = squareColorsOn(light['--mantle'], light['--text']);
  assert.ok(contrastRatio(SQUARE_COLORS[2], light['--mantle']) < 1.5);
  assert.ok(contrastRatio(yellow, light['--mantle']) >= GLYPH_MIN_CONTRAST);
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(yellow.slice(i, i + 2), 16));
  assert.ok(r >= b && g >= b, `yellow family kept: ${yellow}`);
});

test('ESP Square keeps a colour that already reads (cyan on a dark canvas is unchanged)', () => {
  const dark = varsOf(null);
  assert.equal(squareColorsOn(dark['--mantle'], dark['--text'])[6], SQUARE_COLORS[6]);
});
