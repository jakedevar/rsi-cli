// Local #291: the operator to-do panel is a column on a wide window and a drawer over the session pane below 1200 px, so the
// conversation keeps a readable width. Asserted on the stylesheet text, the pure rules and the DOM wiring in todo.ts (the headless
// smoke step "todo-drawer" measures the rendered result at 760x480, 1024x640 and 1280x800).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { TODO_DRAWER_MAX_WIDTH, TODO_DRAWER_QUERY, drawerEscape, drawerReturnTarget, initialPanelOpen } from '../dist/todo-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const layout = readFileSync(here('../static/layout.css'), 'utf8');
const todoSrc = readFileSync(here('../src/todo.ts'), 'utf8');
const stripped = layout.replace(/\/\*[\s\S]*?\*\//g, '');
const block = (media) => {
  const i = stripped.indexOf(`@media (${media})`);
  assert.ok(i >= 0, `layout.css has a @media (${media}) block`);
  let depth = 0, j = stripped.indexOf('{', i);
  const start = j;
  for (; j < stripped.length; j++) { if (stripped[j] === '{') depth++; else if (stripped[j] === '}' && --depth === 0) break; }
  return stripped.slice(start + 1, j);
};

test('the drawer breakpoint is the same number in the stylesheet and the code', () => {
  assert.equal(TODO_DRAWER_QUERY, `(max-width: ${TODO_DRAWER_MAX_WIDTH}px)`);
  assert.ok(layout.includes(`@media (max-width: ${TODO_DRAWER_MAX_WIDTH}px)`));
});

test('below the breakpoint the panel overlays the pane instead of taking a column', () => {
  const css = block(`max-width: ${TODO_DRAWER_MAX_WIDTH}px`);
  assert.match(css, /\.layout\s*\{\s*position:\s*relative/, 'the layout is the drawer\'s containing block');
  const rule = css.match(/\.layout > \.todo-panel\s*\{([^}]*)\}/)?.[1] ?? '';
  assert.match(rule, /position:\s*absolute/);
  assert.match(rule, /top:\s*0/); assert.match(rule, /bottom:\s*0/); assert.match(rule, /right:\s*0/);
  assert.match(rule, /width:\s*min\(320px,\s*100%\)/, 'at most the panel\'s own 320 px, never wider than the window');
  assert.match(rule, /z-index:\s*\d+/);
  // The panel's own rules are injected after layout.css, so the override needs the higher specificity of `.layout >`.
  assert.match(todoSrc, /\.todo-panel \{ width: 320px;/);
});

test('the column fits wherever it is kept: sidebar, panel and a 480 px conversation need no more than the breakpoint', () => {
  const sidebarMax = 340, panel = 320, conversation = 480;
  assert.ok(sidebarMax + panel + conversation <= TODO_DRAWER_MAX_WIDTH + 1, `a column from ${TODO_DRAWER_MAX_WIDTH + 1} px leaves the conversation ${TODO_DRAWER_MAX_WIDTH + 1 - sidebarMax - panel} px`);
  // At the narrowest drawer window (760 px) the conversation has the whole pane beside the 260 px sidebar: 500 px, over 45ch (about 400 px).
  assert.ok(760 - 260 >= 45 * 8.8);
});

test('Esc closes an open drawer unless a dialog is above it; a column and a closed panel ignore it', () => {
  const st = { drawer: true, open: true, dialogOpen: false, key: 'Escape' };
  assert.equal(drawerEscape(st), 'close');
  assert.equal(drawerEscape({ ...st, dialogOpen: true }), 'none');
  assert.equal(drawerEscape({ ...st, drawer: false }), 'none');
  assert.equal(drawerEscape({ ...st, open: false }), 'none');
  assert.equal(drawerEscape({ ...st, key: 'q' }), 'none');
});

test('focus returns to where it was, when that is still on the page and outside the drawer', () => {
  const alive = () => true, inside = (e) => e === 'in';
  assert.equal(drawerReturnTarget('message', alive, inside), 'message');
  assert.equal(drawerReturnTarget(null, alive, inside), null);
  assert.equal(drawerReturnTarget('message', () => false, inside), null);
  assert.equal(drawerReturnTarget('in', alive, inside), null);
});

test('todo.ts wires the drawer: focus moves in on open, back on close, Esc closes, the aside can take focus', () => {
  assert.match(todoSrc, /aside\.tabIndex = -1/);
  assert.match(todoSrc, /document\.addEventListener\('keydown', onDrawerEscape\)/);
  assert.match(todoSrc, /ui\.aside\.focus\(/);
  assert.match(todoSrc, /drawerReturnTarget<Element>\(drawerFrom/);
  assert.match(todoSrc, /drawerEscape\(\{ drawer: isDrawer\(\)/);
  // Hide and <Space>dt both go through toggle(), so they restore focus the same way.
  assert.match(todoSrc, /hide\.addEventListener\('click', \(\) => toggle\(\)\)/);
  assert.match(todoSrc, /case 'todo-toggle': toggle\(\)/);
});

test('a drawer window starts with the panel closed even if it was open; a wide window keeps the remembered choice', () => {
  assert.equal(initialPanelOpen(true, true), false);
  assert.equal(initialPanelOpen(true, false), true);
  assert.equal(initialPanelOpen(false, false), false);
  assert.equal(initialPanelOpen(false, true), false);
  assert.equal(initialPanelOpen(undefined, false), false);
  assert.equal(initialPanelOpen('true', false), false);
});

test('todo.ts starts the panel through initialPanelOpen and keeps the remembered choice when a narrow launch closes it', () => {
  assert.match(todoSrc, /new TodoPanel\(initialPanelOpen\(o\.open, isDrawer\(\)\)/);
  assert.match(todoSrc, /rememberedOpen = o\.open === true/);
  // Only a toggle changes what is stored, so a narrow launch (drawer closed) does not erase "open" for the next wide launch.
  assert.match(todoSrc, /JSON\.stringify\(\{ open: rememberedOpen,/);
  assert.match(todoSrc, /panel\.open = !panel\.open;\n\s+rememberedOpen = panel\.open;/);
});
