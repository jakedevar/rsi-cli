// Global Dashboard (hub #3366, slice D of #3314): the desktop `gd` / `:dashboard`
// view's rules, from a fixture GetGlobalDashboard response
// (tests/fixtures/global-dashboard.json), mirroring the TUI's
// crates/rsi/src/overlay/global_dashboard_tests.rs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  DASHBOARD_HINTS, DASHBOARD_KEYS, NOT_A_PROJECT, SORTS, START_HINT, WINDOWS, alreadyLiveNotice, columns, conversationNotice, cycle,
  dashboardHints, dashboardRows, detailLines, fmtAge, fmtCost, fmtCount, incompleteText, isIncomplete, liveSeat, noManagerNotice,
  notListedText, providersText, refreshFailedText, reselect, retryHint, seatAction, seatFromGrant, sortedProjects, startHint,
  startHost, unreadableText,
} from '../dist/dashboard-model.js';
import { offText, stripKeyLabel } from '../dist/key-hints.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = () => JSON.parse(readFileSync(join(here, 'fixtures/global-dashboard.json'), 'utf8'));
const HOME = 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb';
const ALPHA = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';

test('project rows put the home project first, then by live agents, then name, with the All projects row pinned last', () => {
  const rows = dashboardRows(fixture(), 'agents');
  assert.deepEqual(rows.map((r) => r.cells.project.text), ['rsi (home)', 'alpha', 'Zeta', 'All projects']);
  assert.equal(rows[0].isHome, true);
  assert.equal(rows.at(-1).isTotal, true);
  assert.equal(rows.at(-1).id, null);
  // The home project leads every sort.
  for (const sort of SORTS) assert.equal(sortedProjects(fixture(), sort)[0].id, HOME, sort);
});

test('a project row shows live agents by role and provider, tokens, cost and Issue flow as the daemon sent them', () => {
  const alpha = dashboardRows(fixture(), 'agents')[1].cells;
  assert.equal(alpha.agents.text, '5');
  assert.deepEqual([alpha.managers.text, alpha.leads.text, alpha.workers.text, alpha.reviewers.text], ['1', '1', '2', '1']);
  assert.equal(alpha.providers.text, 'Claude 3 · Codex 2');
  assert.equal(alpha.tokensIn.text, '1.2M');
  assert.equal(alpha.tokensIn.title, '1234567');
  assert.equal(alpha.tokensOut.text, '46k');
  assert.equal(alpha.cost.text, '$13');
  assert.deepEqual([alpha.open.text, alpha.inProgress.text, alpha.created.text, alpha.closed.text, alpha.requests.text], ['7', '2', '4', '3', '1']);
  assert.equal(alpha.landings.text, '2');
  assert.equal(alpha.qa.text, '2h');
  assert.equal(alpha.pm.text, '●');
  assert.equal(alpha.pm.title, 'Project manager Running');
  const home = dashboardRows(fixture(), 'agents')[0].cells;
  assert.equal(home.qa.text, '2 red');
  assert.equal(home.pm.title, 'Project manager waiting for you');
  // Zero counts are dim; a project with no QA history reads `-`.
  const zeta = dashboardRows(fixture(), 'agents')[2].cells;
  assert.equal(zeta.agents.dim, true);
  assert.equal(zeta.cost.text, '$0');
  assert.equal(zeta.providers.text, '-');
  assert.equal(zeta.qa.text, '-');
  assert.equal(zeta.pm.title, 'no project manager');
});

test('the TOTAL row reads the daemon totals, not a sum of the listed rows', () => {
  const total = dashboardRows(fixture(), 'agents').at(-1).cells;
  assert.equal(total.agents.text, '7');
  assert.equal(total.providers.text, 'Claude 4 · Codex 2 +1');
  assert.equal(total.cost.text, '$16');
  assert.deepEqual([total.open.text, total.created.text, total.closed.text], ['10', '6', '8']);
  assert.equal(total.qa.text, '2 red');
  assert.equal(total.pm.text, '');
});

test('column headers follow the window and each carries plain words', () => {
  const cols = columns('7d');
  assert.equal(cols.length, 18);
  assert.equal(cols[0].header, 'Project');
  assert.equal(cols.find((c) => c.key === 'created').header, 'New 7d');
  assert.equal(cols.find((c) => c.key === 'closed').title, 'Issues closed in the last 7d');
  for (const c of cols) assert.ok(c.title.length > c.header.length - 1, c.key);
});

test('the incomplete flag shows whenever the daemon capped a read, with its reasons', () => {
  const complete = fixture();
  assert.equal(isIncomplete(complete), false);
  assert.equal(incompleteText(complete), null);
  const capped = { ...fixture(), incomplete_reasons: ['sessions', 'issues:abc'] };
  assert.equal(isIncomplete(capped), true);
  assert.match(incompleteText(capped), /^Incomplete: .*lower bound: sessions, issues:abc\.$/);
  const totals = fixture();
  totals.totals.incomplete = true;
  assert.equal(isIncomplete(totals), true);
  const projects = { ...fixture(), projects_incomplete: true, incomplete_reasons: ['projects'] };
  assert.match(incompleteText(projects), /Not every project is listed\./);
  // The TOTAL detail band names the capped reads too.
  assert.equal(detailLines(capped, 'agents', 3).at(-1), 'Incomplete (lower bound): sessions, issues:abc');
});

test('an empty dashboard still has its TOTAL row (the view adds the empty-state words)', () => {
  const empty = { ...fixture(), projects: [] };
  const rows = dashboardRows(empty, 'agents');
  assert.deepEqual(rows.map((r) => r.name), ['All projects']);
  assert.deepEqual(dashboardRows(null, 'agents'), []);
});

test('the detail band names the selected project, agents by model and effort, usage and Issue flow', () => {
  const lines = detailLines(fixture(), 'agents', 1);
  assert.equal(lines[0], 'alpha · project manager Running');
  assert.equal(lines[1], 'Agents 5 live · 1 manager · 1 lead · 2 workers · 1 reviewer');
  assert.equal(lines[2], 'By model: Claude opus high ×3 · Codex gpt ×2');
  assert.equal(lines[3], 'Usage 24h: Claude opus 1.2M in 46k out $13');
  assert.equal(lines[4], 'Issues: 7 open · 2 in progress · 4 created · 3 closed in 24h · 1 operator request · landings 2 published, 1 refused');
  assert.equal(lines[5], 'QA green 2h ago');
  assert.equal(detailLines(fixture(), 'agents', 0)[0], 'rsi · home project · project manager WaitingApproval');
  assert.equal(detailLines(fixture(), 'agents', 3)[0], 'All projects · every project');
  assert.match(detailLines(fixture(), 'agents', 3)[1], /· 1 other$/);
});

test('a new snapshot or sort keeps the selection on the same project, or on TOTAL', () => {
  const s = fixture();
  const at = dashboardRows(s, 'agents').findIndex((r) => r.id === ALPHA);
  const bySort = reselect({ snapshot: s, sort: 'agents', selected: at }, { snapshot: s, sort: 'issues' });
  assert.equal(dashboardRows(s, 'issues')[bySort].id, ALPHA);
  const fewer = { ...s, projects: s.projects.filter((p) => p.id !== HOME) };
  assert.equal(dashboardRows(fewer, 'agents')[reselect({ snapshot: s, sort: 'agents', selected: at }, { snapshot: fewer, sort: 'agents' })].id, ALPHA);
  assert.equal(reselect({ snapshot: s, sort: 'agents', selected: 3 }, { snapshot: fewer, sort: 'agents' }), 2);
  assert.equal(reselect({ snapshot: null, sort: 'agents', selected: 0 }, { snapshot: s, sort: 'agents' }), 0);
});

test('with no live global seat the pane offers the one-key start, hosted in the home project', () => {
  assert.deepEqual(seatFromGrant(null), { kind: 'absent' });
  assert.deepEqual(seatFromGrant({ state: 'revoked', seat_session_id: 'x' }), { kind: 'absent' });
  const absent = seatAction({ kind: 'absent' }, null);
  assert.equal(absent.kind, 'start');
  assert.equal(absent.text, `No top-level manager is live; ${START_HINT} (it launches and appoints one)`);
  assert.equal(START_HINT, 'press n to start one');
  assert.equal(DASHBOARD_KEYS.n, 'start');
  assert.equal(startHost(fixture()), HOME);
  assert.equal(startHost({ ...fixture(), projects: [] }), null);
  // An archived seat is not live: the start is offered again.
  const seat = seatFromGrant({ state: 'active', seat_session_id: '33333333-3333-4333-8333-333333333333' });
  assert.equal(seatAction(seat, 'Archived').kind, 'start');
  assert.equal(liveSeat(seat, 'Deleted'), null);
  assert.equal(seatAction({ kind: 'unknown' }, null).kind, 'loading');
});

test('a live global seat offers typing to it instead of a start', () => {
  const seat = seatFromGrant({ state: 'active', seat_session_id: '33333333-3333-4333-8333-333333333333' });
  assert.equal(liveSeat(seat, 'Running'), '33333333-3333-4333-8333-333333333333');
  const a = seatAction(seat, 'Running');
  assert.equal(a.kind, 'type');
  assert.match(a.text, /^Top-level manager 33333333 · Running · press i to type to it$/);
});

test('keys: j/k move, Enter/l open the project, Tab focuses the conversation, Esc closes, as in the TUI view', () => {
  assert.equal(DASHBOARD_KEYS.j, 'down');
  assert.equal(DASHBOARD_KEYS.ArrowDown, 'down');
  assert.equal(DASHBOARD_KEYS.k, 'up');
  assert.equal(DASHBOARD_KEYS.g, 'first');
  assert.equal(DASHBOARD_KEYS.G, 'last');
  assert.equal(DASHBOARD_KEYS.Enter, 'open');
  assert.equal(DASHBOARD_KEYS.l, 'open');
  assert.equal(DASHBOARD_KEYS.Tab, 'conversation');
  assert.equal(DASHBOARD_KEYS.Escape, 'close');
  assert.equal(DASHBOARD_KEYS.q, 'close');
  assert.equal(DASHBOARD_KEYS.w, 'window');
  assert.equal(DASHBOARD_KEYS.s, 'sort');
  assert.equal(DASHBOARD_KEYS.t, 'pane');
  assert.equal(DASHBOARD_KEYS.i, 'type');
  assert.equal(DASHBOARD_KEYS.r, 'refresh');
  assert.deepEqual([cycle(WINDOWS, '1h'), cycle(WINDOWS, '24h'), cycle(WINDOWS, '7d')], ['24h', '7d', '1h']);
  assert.deepEqual(SORTS.map((s) => cycle(SORTS, s)), ['tokens', 'cost', 'issues', 'agents']);
});

test('gd and :dashboard (and :gd) open the dashboard', () => {
  const e = new KeyEngine(desktopKeymap());
  e.feed('g');
  assert.equal(e.feed('d').action, 'mgr-dashboard');
  for (const line of ['dashboard', 'gd']) assert.equal(new KeyEngine(desktopKeymap()).runCommand(line).action, 'mgr-dashboard', line);
  const src = readFileSync(join(here, '../src/managers.ts'), 'utf8');
  assert.match(src, /case 'mgr-dashboard':[\s\S]{0,200}openDashboard\(\)/);
});

test('formatting matches the TUI helpers', () => {
  assert.deepEqual([0, 812, 1000, 1234, 340_000, 999_499, 999_500, 12_000_000, 1_234_567_890].map(fmtCount),
    ['0', '812', '1.0k', '1.2k', '340k', '999k', '1.0M', '12M', '1.2B']);
  assert.deepEqual([0, -1, 3.2, 123.4, 1234].map(fmtCost), ['$0', '$0', '$3.20', '$123', '$1.2k']);
  assert.deepEqual([45, 720, 10_800, 172_800, -5].map(fmtAge), ['45s', '12m', '3h', '2d', '0s']);
  assert.equal(providersText({ by_provider: [{ provider: 'B', count: 1 }, { provider: 'A', count: 1 }, { provider: 'C', count: 0 }] }, 2), 'A 1 · B 1');
});

// ---------- Vim keys off (local #230) ----------

const SEAT = seatFromGrant({ state: 'active', seat_session_id: '33333333-3333-4333-8333-333333333333' });
/** A letter key named as something to press, or a key list ("j/k", "g/G"), or the gm chord. */
const NAMES_A_VIM_KEY = /\b[Pp]ress [A-Za-z]\b|\(or press|\b[A-Za-z]\/[A-Za-z]\b|\bgm\b|\(n\)|\(i\)|\(r\)/;

test('with Vim keys on the dashboard keeps today\'s wording', () => {
  assert.equal(dashboardHints(true), DASHBOARD_HINTS);
  assert.equal(DASHBOARD_HINTS, 'j/k move · Enter/l open project · w window · s sort · Tab conversation · i type · n start manager · t pane · r refresh · gm workspace · Esc close');
  assert.equal(startHint(true), START_HINT);
  assert.equal(retryHint(true), 'press r to retry');
  assert.equal(refreshFailedText('boom', true, true), 'Refresh failed: boom (showing the last snapshot); press r to retry');
  assert.equal(refreshFailedText('boom', false, true), 'Refresh failed: boom; press r to retry');
  assert.equal(unreadableText(true), 'The dashboard could not be read yet. Press r to retry.');
  assert.equal(notListedText('alpha', true), 'alpha is not in the project list yet; press r');
  assert.equal(conversationNotice(true), 'Conversation: j/k, Ctrl-D/U scroll, g/G ends; Tab or Esc returns to the projects');
  assert.equal(noManagerNotice({ kind: 'absent' }, true), 'No top-level manager is live; press n to start one');
  assert.equal(alreadyLiveNotice(true), 'A top-level manager is already live; press i to type to it, or gm to replace it');
  assert.equal(seatAction({ kind: 'absent' }, null, true).text, `No top-level manager is live; ${START_HINT} (it launches and appoints one)`);
  assert.equal(seatAction(SEAT, 'Running', true).text, 'Top-level manager 33333333 · Running · press i to type to it');
});

test('with Vim keys off the footer, retry, error, empty and start lines name a button, not a letter key', () => {
  const lines = {
    footer: dashboardHints(false),
    retry: retryHint(false),
    refreshFailed: refreshFailedText('boom', true, false),
    unreadable: unreadableText(false),
    notListed: notListedText('alpha', false),
    conversation: conversationNotice(false),
    start: noManagerNotice({ kind: 'absent' }, false),
    already: alreadyLiveNotice(false),
    pane: seatAction({ kind: 'absent' }, null, false).text,
    type: seatAction(SEAT, 'Running', false).text,
  };
  for (const [name, text] of Object.entries(lines)) assert.doesNotMatch(text, NAMES_A_VIM_KEY, name);
  // The footer keeps only keys that still work with the switch off.
  assert.equal(lines.footer, 'Up/Down move · Enter open project · F6 conversation · Esc close');
  assert.equal(lines.retry, 'click Refresh to retry');
  assert.equal(lines.refreshFailed, 'Refresh failed: boom (showing the last snapshot); click Refresh to retry');
  assert.equal(lines.unreadable, 'The dashboard could not be read yet. Click Refresh to retry.');
  assert.equal(lines.start, 'No top-level manager is live; click Start a top-level manager…');
  assert.equal(lines.pane, 'No top-level manager is live; click Start a top-level manager… (it launches and appoints one)');
  assert.equal(lines.type, 'Top-level manager 33333333 · Running · click Type to it…');
  assert.match(lines.notListed, /click Refresh/);
  assert.match(lines.already, /click Type to it… to message it, or Manager workspace to replace it/);
  // The seat is a manager here, not a "seat".
  assert.equal(noManagerNotice({ kind: 'unknown' }, false), 'Still reading the top-level manager');
  assert.equal(seatAction({ kind: 'unknown' }, null).text, 'Reading the top-level manager…');
  for (const t of [...Object.values(lines), NOT_A_PROJECT]) assert.doesNotMatch(t, /\bseat\b|TOTAL/, t);
});

test('the button labels carry their key as a hint the key-hints pass drops with the switch off', () => {
  for (const [label, off] of [
    ['Open project (Enter)', 'Open project (Enter)'], ['Manager workspace (gm)', 'Manager workspace'], ['Sort (s)', 'Sort'],
    ['Show/hide manager pane (t)', 'Show/hide manager pane'], ['Refresh (r)', 'Refresh'], ['Start a top-level manager… (n)', 'Start a top-level manager…'],
    ['Type to it… (i)', 'Type to it…'],
  ]) assert.equal(stripKeyLabel(label), off, label);
  assert.equal(offText('tip', 'Open the manager workspace (gm)'), 'Open the manager workspace');
});

test('column headers spell the roles out, and use the short forms only in a narrow table', () => {
  const wide = Object.fromEntries(columns('24h').map((c) => [c.key, c.header]));
  assert.deepEqual([wide.pm, wide.managers, wide.leads, wide.workers, wide.reviewers, wide.inProgress, wide.requests],
    ['Project manager', 'Managers', 'Epic leads', 'Workers', 'Reviewers', 'In progress', 'Requests']);
  const narrow = Object.fromEntries(columns('24h', true).map((c) => [c.key, c.header]));
  assert.deepEqual([narrow.pm, narrow.managers, narrow.leads, narrow.workers, narrow.reviewers, narrow.inProgress, narrow.requests],
    ['PM', 'Mgr', 'Lead', 'Wrk', 'Rev', 'WIP', 'Req']);
  assert.deepEqual(columns('7d').map((c) => c.key), columns('7d', true).map((c) => c.key));
});

test('the Open project and Manager workspace buttons run the same actions as Enter and gm', () => {
  const src = readFileSync(join(here, '../src/dashboard.ts'), 'utf8');
  // Buttons: data-dash names the action the key resolves to.
  assert.match(src, /btn\('Open project \(Enter\)', 'open'\)/);
  assert.match(src, /btn\('Manager workspace \(gm\)', 'workspace'/);
  assert.equal(DASHBOARD_KEYS.Enter, 'open');
  // Enter and the button both end in dashboardKey('open') -> openProject; gm and the button both in dashboardKey('workspace').
  assert.match(src, /case 'open': openProject\(\); return;/);
  assert.match(src, /case 'workspace': close\(\); runManagers\('mgr-global'/);
  assert.match(src, /if \(ev\.key === 'd'\) \{ void refresh\(\); return; \}\s+dashboardKey\('workspace'\);/);
  // The button is enabled only for a project row.
  assert.match(src, /open\.disabled = !ok;/);
  // Pane button reads in words; the title is not the bare RSI / Dashboard.
  assert.match(src, /Show\/hide manager pane \(t\)/);
  assert.doesNotMatch(src, /'RSI \/ Dashboard'|btn\('Pane'/);
});

test('Tab: the dashboard has no Tab special case; the shared rule leaves Tab to the browser with Vim keys off (#243)', () => {
  const src = readFileSync(join(here, '../src/dashboard.ts'), 'utf8');
  assert.doesNotMatch(src, /dashboardClaimsTab/);
  assert.equal(DASHBOARD_KEYS.Tab, 'conversation');
  assert.match(src, /DASHBOARD_KEYS\[paneKeyName\(ev\)\]/);
});
