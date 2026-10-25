// Local #273: the layout rules for the 760x480 minimum window and a 2560 px display, asserted on the stylesheet text and the
// shipped files (the headless smoke step "window-sizes" measures the rendered result).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const read = (p) => readFileSync(join(root, p), 'utf8');
const css = read('static/layout.css').replace(/\/\*[\s\S]*?\*\//g, '');

/** The top-level blocks and @media blocks of the file as { media, selector, body }. */
function rules(text) {
  const out = [];
  const walk = (src, media) => {
    let i = 0;
    while (i < src.length) {
      const open = src.indexOf('{', i);
      if (open < 0) break;
      const head = src.slice(i, open).trim();
      let depth = 1, j = open + 1;
      while (j < src.length && depth) { if (src[j] === '{') depth++; else if (src[j] === '}') depth--; j++; }
      const body = src.slice(open + 1, j - 1);
      if (head.startsWith('@media')) walk(body, head.replace('@media', '').trim());
      else out.push({ media, selector: head.replace(/\s+/g, ' '), body: body.trim() });
      i = j;
    }
  };
  walk(text, null);
  return out;
}
const R = rules(css);
const find = (selector, media = null) => R.find((r) => r.selector === selector && r.media === media);
const decl = (r, prop) => (r?.body.match(new RegExp(`(?:^|;|\\s)${prop}\\s*:\\s*([^;]+)`)) ?? [])[1]?.trim();

test('layout.css is linked after styles.css and copied into dist', () => {
  const html = read('static/index.html');
  assert.ok(html.indexOf('href="layout.css"') > html.indexOf('href="styles.css"'), 'layout.css loads after styles.css so it overrides');
  assert.match(read('scripts/copy-static.mjs'), /'layout\.css'/);
});

test('the conversation, question, outcome card and message box share one readable measure and stay centred', () => {
  const measure = decl(find(':root'), '--measure');
  const ch = Number(measure?.match(/^(\d+)ch$/)?.[1]);
  assert.ok(ch >= 80 && ch <= 100, `the measure is ${measure}, about 80-100ch`);
  const r = R.find((x) => x.media === null && ['.events', '.approval-banner', '.outcome-card', '.composer'].every((s) => x.selector.split(', ').includes(s)));
  assert.ok(r, 'one rule covers .events, .approval-banner, .outcome-card and .composer');
  // Equal padding of max(16px, half of what the measure leaves) on both sides: capped and centred; never less than 16 px.
  assert.equal(decl(r, 'padding-inline'), 'max(16px, calc((100% - var(--measure)) / 2))');
});

test('the question banner keeps its height: it never shrinks to a scroll box', () => {
  const r = find('.approval-banner');
  assert.equal(decl(r, 'flex'), 'none');
  assert.equal(decl(r, 'max-height'), 'none');
  assert.equal(decl(r, 'overflow'), 'visible');
  // The transcript keeps a floor so it never collapses to nothing.
  assert.match(decl(find('.events'), 'min-height') ?? '', /^\d+px$/);
});

test('the sidebar shrinks with the window: 340 px wide, 260 px at the minimum', () => {
  assert.equal(decl(find('.sidebar'), 'width'), 'clamp(260px, 30vw, 340px)');
});

test('a narrow window: the header actions take their own row and meta items shorten instead of breaking by word', () => {
  const m = '(max-width: 1100px)';
  assert.equal(decl(find('.session-head', m), 'flex-wrap'), 'wrap');
  assert.equal(decl(find('.head-main', m), 'flex'), '1 1 100%', 'the title column takes the full row');
  assert.equal(decl(find('.head-actions', m), 'flex'), '1 1 100%', 'the actions are a row of their own');
  assert.equal(decl(find('.head-actions', m), 'flex-wrap'), 'wrap');
  const item = find('.session-head .meta > *', m);
  assert.equal(decl(item, 'white-space'), 'nowrap');
  assert.equal(decl(item, 'text-overflow'), 'ellipsis');
  assert.equal(decl(item, 'min-width'), '0');
  assert.equal(decl(find('.session-head .meta .head-details', m), 'white-space'), 'normal', 'the open Details rows still wrap');
  assert.equal(decl(find('.head-details:not([open])', m), 'flex-basis'), 'auto', 'closed Details sits beside the ID');
});

test('a short window (480 px) tightens the question, the outcome card and the message box', () => {
  const m = '(max-height: 600px)';
  const px = (sel, prop) => parseFloat(decl(find(sel, m), prop));
  assert.ok(px('.approval textarea', 'height') <= 36);
  assert.ok(px('.composer textarea', 'height') <= 44);
  assert.ok(px('.composer textarea', 'min-height') <= 44);
  // The budget at 480 px: topbar 49 + head 106 + banner ~225 + composer 53 leaves the transcript a visible strip.
  const pane = 480 - 49;
  assert.ok(106 / pane <= 0.25, 'the measured header is about a quarter of the pane');
});

test('lists and tables keep the full width: only the conversation, banner, next-waiting bar, outcome card and composer are capped', () => {
  const capped = R.filter((r) => decl(r, 'padding-inline')?.includes('--measure')).flatMap((r) => r.selector.split(', ')).sort();
  assert.deepEqual(capped, ['.approval-banner', '.composer', '.events', '.next-waiting', '.outcome-card']);
  for (const r of R) assert.doesNotMatch(r.body, /max-width:\s*\d+ch/, `${r.selector} caps no list or table by ch`);
});

// Local #288: Settings at the minimum window. The settings slice injects its own rules after layout.css, so each rule here is
// prefixed with .settings-pane to win on specificity.
test('a narrow window gives the Settings rows room: slimmer rail and info columns, the value under its label', () => {
  const m = '(max-width: 900px)';
  const cols = decl(find('.settings-pane .settings-body', m), 'grid-template-columns') ?? '';
  const mins = [...cols.matchAll(/minmax\((\d+)px/g)].map((x) => Number(x[1]));
  assert.equal(mins.length, 2, 'a rail and an info column with a minimum, the rows take the rest');
  assert.ok(cols.includes('minmax(0, 1fr)'));
  assert.ok(760 - 190 - 220 >= 340 && mins[0] <= 190, 'the rail is at most 190 px');
  assert.ok(Number(cols.match(/minmax\(\d+px, (\d+)px\) minmax\(0, 1fr\) minmax\(\d+px, (\d+)px\)/)?.[1]) + Number(cols.match(/minmax\(0, 1fr\) minmax\(\d+px, (\d+)px\)/)?.[1]) <= 410, 'rail and info together leave the rows 350 px of 760');
  const row = find('.settings-pane .settings-row', m);
  assert.match(decl(row, 'grid-template-columns') ?? '', /^14px minmax\(0, 1fr\) 16px$/, 'dirty mark, label, apply mark; the value goes below');
  // The value takes a second line under the label: rows 1 and 2.
  assert.match(decl(find('.settings-pane .settings-row .sv-label', m), 'grid-area') ?? '', /^1 \/ 2$/);
  assert.match(decl(find('.settings-pane .settings-row .sv-value', m), 'grid-area') ?? '', /^2 \/ 2/);
  assert.equal(decl(find('.settings-pane .settings-row .sv-value', m), 'max-width'), '100%');
});

test('a short window fits every Settings group in the rail without scrolling', () => {
  const m = '(max-height: 600px)';
  const px = (sel, prop) => parseFloat((decl(find(sel, m), 'padding') ?? decl(find(sel, m), prop) ?? '').split(' ')[0]);
  // Eight groups of label (16) + summary (14) + padding: they must fit the 480 px window under two 49 px bars (382 px).
  assert.ok(px('.settings-pane .settings-group', 'padding') <= 4, 'tight group padding');
  const summary = find('.settings-pane .settings-group .sg-summary', m);
  assert.equal(decl(summary, 'white-space'), 'nowrap', 'a summary is one line');
  assert.equal(decl(summary, 'text-overflow'), 'ellipsis');
  const groups = 8, per = 16 + 14 + 6, rail = 8 + groups * per;
  assert.ok(rail <= 480 - 98, `eight groups need about ${rail} px of the 382 px rail`);
});

test('the Settings rules win over the slice-injected ones: every selector carries .settings-pane', () => {
  const settings = R.filter((r) => /settings-/.test(r.selector));
  assert.ok(settings.length >= 8);
  for (const r of settings) assert.ok(r.selector.split(', ').every((s) => s.startsWith('.settings-pane ')), `${r.selector} starts with .settings-pane`);
});

// Local #302: the Issues workspace and its form dialogs in the 480 px window.
test('a short window keeps the Issues inspector tab row on screen: it sticks to the bottom edge of the scrolling inspector', () => {
  const m = '(max-height: 600px)';
  const tabs = find('.issues-inspector .iv-tabs', m);
  assert.ok(tabs, 'a short-window rule for the inspector tab row');
  assert.equal(decl(tabs, 'position'), 'sticky');
  assert.match(decl(tabs, 'bottom') ?? '', /^-?\d+px$/, 'it sticks to a bottom edge');
  assert.match(decl(tabs, 'background') ?? '', /^var\(--/, 'opaque, so the section text does not show through');
  // A tall window is unchanged: no sticky rule outside the media query.
  assert.equal(R.some((r) => r.media === null && /issues-inspector .iv-tabs/.test(r.selector)), false);
  // The inspector it sticks in is a scroll region (styles.css).
  assert.match(read('static/styles.css'), /\.issues-inspector \{[^}]*overflow-y: auto/);
});

test('a form dialog is never taller than the window: the title and the buttons stay, the fields scroll', () => {
  const dlg = find('dialog.issue-dialog[open]');
  assert.equal(decl(dlg, 'display'), 'flex');
  assert.equal(decl(dlg, 'flex-direction'), 'column');
  assert.equal(decl(dlg, 'overflow'), 'hidden', 'the dialog itself does not scroll; its fields do');
  assert.match(decl(dlg, 'max-height') ?? '', /100vh/, 'bounded by the window height');
  const form = find('dialog.issue-dialog #issue-form');
  assert.equal(decl(form, 'min-height'), '0');
  const fixed = R.find((r) => r.media === null && r.selector.includes('#issue-form > .actions'));
  assert.ok(fixed, 'the action row is a fixed part of the column');
  for (const part of ['h2', '.form-intro', '.form-error', '.stale-bar', '.actions']) assert.ok(fixed.selector.includes(`#issue-form > ${part}`), `${part} does not shrink`);
  assert.equal(decl(fixed, 'flex'), 'none');
  const fields = find('dialog.issue-dialog #issue-form > .form-fields');
  assert.equal(decl(fields, 'overflow-y'), 'auto');
  assert.equal(decl(fields, 'min-height'), '0');
});

test('in a short window the short form fields share a row and the description is a few lines, so New Issue fits 480 px', () => {
  const m = '(max-height: 600px)';
  const grid = find('dialog.issue-dialog .form-fields', m);
  assert.equal(decl(grid, 'display'), 'grid');
  assert.match(decl(grid, 'grid-template-columns') ?? '', /repeat\(2, minmax\(0, 1fr\)\)/);
  const wide = R.find((r) => r.media === m && r.selector.startsWith('dialog.issue-dialog .form-fields >'));
  assert.equal(decl(wide, 'grid-column'), '1 / -1', 'a textarea, a checkbox group and the first field take the full width');
  const ta = find('dialog.issue-dialog .form-fields textarea', m);
  const height = parseFloat(decl(ta, 'height') ?? '');
  assert.ok(height > 0 && height <= 5, `the description is ${height}em tall, about three lines`);
  // The dialog is the whole window minus 24 px at most: 456 px. Title, description, two rows of two fields and the buttons fit.
  const used = 24 + 54 + (height * 14 + 28) + 2 * 66 + 52 + 20;
  assert.ok(used <= 480 - 24, `the form needs about ${Math.round(used)} px of 456`);
});

test('the Issues smoke step covers 760x480 and 1024x640 and the shipped stylesheet is the one measured', () => {
  const step = read('scripts/smoke-steps/issues-small-window.mjs');
  assert.match(step, /\[760, 480\], \[1024, 640\]/);
  for (const sel of ['.iv-tabs', 'issues-new', '.actions']) assert.ok(step.includes(sel), `the step looks at ${sel}`);
});
