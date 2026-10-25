// The one keyboard-focus cue (local #320): the ring colour (--focus-ring, the theme's --blue) clears 3:1 (WCAG 1.4.11 non-text
// contrast) against every surface it is drawn on, in the desktop default and every built-in theme, and the rules that draw it
// are the shared ones: the Issues list, Issues inspector and to-do panel no longer switch their outline off.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { DEFAULT_CHOICE, DESKTOP_DEFAULT_VARS, contrastRatio, cssVars } from '../dist/theme-model.js';
import { THEMES } from '../dist/settings.generated.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const css = readFileSync(here('../static/styles.css'), 'utf8');
const html = readFileSync(here('../static/index.html'), 'utf8');
const todo = readFileSync(here('../src/todo.ts'), 'utf8');
const files = readFileSync(here('../src/files.ts'), 'utf8');
const AA_UI = 3;
/** The surfaces a focus ring touches: the page, a side panel (the to-do panel) and a selected row. */
const SURFACES = ['--base', '--mantle', '--surface0'];

const varsOf = (theme) => (theme === null ? { ...DESKTOP_DEFAULT_VARS, ...cssVars(DEFAULT_CHOICE) } : cssVars({ theme, overrides: {} }));

test('--focus-ring is the theme --blue, so a theme override of --blue moves the ring with it', () => {
  assert.match(css, /--focus-ring: var\(--blue\)/);
});

for (const theme of [null, ...THEMES.map((t) => t.key)]) {
  test(`the focus ring clears 3:1 on base, mantle and the selected row (${theme ?? 'desktop default'})`, () => {
    const vars = varsOf(theme);
    for (const surface of SURFACES) {
      const ratio = contrastRatio(vars['--blue'], vars[surface]);
      assert.ok(ratio >= AA_UI, `--blue on ${surface}: ${ratio.toFixed(2)}:1`);
    }
  });
}

test('the shared rules draw a 2px ring on a focused pane and on the current row of a focused list', () => {
  assert.match(css, /\.focus-pane:focus, \.focus-pane:focus-within \{ outline: 2px solid var\(--focus-ring\)/);
  assert.match(css, /\.focus-list:focus > \[aria-selected="true"\] \{ outline: 2px solid var\(--focus-ring\)/);
  assert.match(css, /\.focus-list:focus:not\(:has\(> \[aria-selected="true"\]\)\) \{ outline: 2px solid var\(--focus-ring\)/);
});

test('the Issues list, the Issues inspector and the to-do panel use them and switch no outline off', () => {
  assert.match(html, /id="issues-list" class="issues-list focus-list"/);
  assert.match(html, /id="issues-inspector" class="issues-inspector focus-pane"/);
  assert.match(todo, /el\('aside', 'todo-panel focus-pane'\)/);
  assert.match(files, /el\('ul', 'files-tree focus-list'\)/, 'the file tree draws its ring the same way');
  assert.doesNotMatch(css, /\.issues-list:focus \{ outline: none/);
  assert.doesNotMatch(css, /\.issues-inspector:focus \{ outline: none/);
  assert.doesNotMatch(todo, /\.todo-panel:focus \{ outline: none/);
});
