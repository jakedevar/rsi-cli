// Operator configuration (hub #3039, Desktop 13B): the registry entries, the
// hooks block edits, the provider / bridge form rules, and the wiring of every
// Tauri command to the daemon's operator RPC the TUI uses.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, paletteEntries, unavailable } from '../dist/registry.js';
import {
  OPERATOR_CONFIG_ACTIONS, OPERATOR_CONFIG_RULES, isOperatorConfigAction,
} from '../dist/actions/operatorconfig-spec.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import {
  addHook, bridgeDetail, bridgePutArgs, deleteProviderMessage, hookLabel, hookRows, isHooksConflict, keyStateText,
  parseBridges, parseHooksView, parseProviders, parseRoutes, providerPutArgs, routeOf, routeText, removeHook, replaceHook, validateBridgeForm, validateHookDraft,
  validateProviderForm,
} from '../dist/operator-config-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const ID = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';

const snap = (over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, ...over,
});

test('the three dialogs are registered actions, always available, in the View menu', () => {
  for (const a of OPERATOR_CONFIG_ACTIONS) {
    assert.ok(ACTIONS.some((x) => x.id === a.id), a.id);
    assert.ok(isOperatorConfigAction(a.id));
    assert.equal(OPERATOR_CONFIG_RULES[a.id](), null);
    assert.equal(unavailable(a.id, snap()), null);
    assert.ok(paletteEntries(snap(), 'linux').some((e) => e.id === a.id), `${a.id} is in the palette`);
  }
  assert.ok(!isOperatorConfigAction('emergency-stop'));
});

test(':hooks stays the settings slice\'s command and the hooks list opens on top of it', () => {
  // desktopKeymap throws when a bound command is missing from docs/keybindings.md.
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('hooks').action, 'settings-hooks');
  const run = readFileSync(here('../src/actions/operatorconfig.ts'), 'utf8');
  assert.match(run, /afterAction: \(id\) => \{ if \(id === 'settings-hooks'\) void openHooks\(\); \}/);
  const groups = new Set(OPERATOR_CONFIG_ACTIONS.map((a) => a.group));
  assert.deepEqual([...groups], ['view'], 'Quit stays last in File');
});

const VIEW = {
  path: '/h/.claude/settings.json', exists: true, revision: 'r1',
  hooks: {
    PreToolUse: [{ matcher: 'Bash', hooks: [{ type: 'command', command: 'a.sh', timeout: 5 }, { type: 'command', command: 'b.sh' }] }],
    Stop: [{ hooks: [{ type: 'command', command: 'done.sh' }] }],
  },
};

test('the hooks view is read defensively and flattened in event order', () => {
  const view = parseHooksView(VIEW);
  assert.equal(view.revision, 'r1');
  assert.equal(parseHooksView({ revision: 1 }), null);
  assert.equal(parseHooksView(null), null);
  const rows = hookRows(view.hooks);
  assert.deepEqual(rows.map((r) => [r.event, r.matcher, r.command.command]), [
    ['PreToolUse', 'Bash', 'a.sh'], ['PreToolUse', 'Bash', 'b.sh'], ['Stop', '', 'done.sh'],
  ]);
  assert.equal(hookLabel(rows[0]), 'PreToolUse [Bash]  a.sh');
  assert.equal(hookLabel(rows[2]), 'Stop  done.sh');
});

test('adding a hook joins the entry with the same matcher and leaves the input alone', () => {
  const hooks = parseHooksView(VIEW).hooks;
  const before = JSON.stringify(hooks);
  const joined = addHook(hooks, { event: 'PreToolUse', matcher: 'Bash', command: 'c.sh', timeout: '' });
  assert.equal(joined.PreToolUse.length, 1);
  assert.deepEqual(joined.PreToolUse[0].hooks.map((h) => h.command), ['a.sh', 'b.sh', 'c.sh']);
  const other = addHook(hooks, { event: 'PreToolUse', matcher: 'Edit', command: 'd.sh', timeout: '9' });
  assert.equal(other.PreToolUse.length, 2);
  assert.deepEqual(other.PreToolUse[1], { matcher: 'Edit', hooks: [{ type: 'command', command: 'd.sh', timeout: 9 }] });
  // An event without matcher support drops the typed matcher; the default timeout is not written.
  const stop = addHook({}, { event: 'Stop', matcher: 'Bash', command: 'x', timeout: '60' });
  assert.deepEqual(stop, { Stop: [{ hooks: [{ type: 'command', command: 'x' }] }] });
  assert.equal(JSON.stringify(hooks), before);
});

test('removing a hook drops the entry and the event it empties', () => {
  const hooks = parseHooksView(VIEW).hooks;
  const rows = hookRows(hooks);
  const one = removeHook(hooks, rows[0]);
  assert.deepEqual(one.PreToolUse[0].hooks.map((h) => h.command), ['b.sh']);
  const stop = removeHook(hooks, rows[2]);
  assert.equal('Stop' in stop, false);
  assert.ok('PreToolUse' in stop);
  assert.equal(JSON.stringify(parseHooksView(VIEW).hooks), JSON.stringify(hooks), 'input unchanged');
});

test('editing a hook changes it in place, or moves it when the event or matcher changes', () => {
  const hooks = parseHooksView(VIEW).hooks;
  const before = JSON.stringify(hooks);
  const rows = hookRows(hooks);
  const same = replaceHook(hooks, rows[0], { event: 'PreToolUse', matcher: 'Bash', command: 'a2.sh', timeout: '' });
  assert.deepEqual(same.PreToolUse[0].hooks.map((h) => h.command), ['a2.sh', 'b.sh']);
  assert.equal('timeout' in same.PreToolUse[0].hooks[0], false, 'the default timeout is not written');
  const timed = replaceHook(hooks, rows[1], { event: 'PreToolUse', matcher: 'Bash', command: 'b.sh', timeout: '5' });
  assert.equal(timed.PreToolUse[0].hooks[1].timeout, 5);
  // A new matcher moves the command to its own entry; the old entry keeps the rest.
  const moved = replaceHook(hooks, rows[0], { event: 'PreToolUse', matcher: 'Edit', command: 'a.sh', timeout: '' });
  assert.deepEqual(moved.PreToolUse.map((e) => e.matcher), ['Bash', 'Edit']);
  assert.deepEqual(moved.PreToolUse[0].hooks.map((h) => h.command), ['b.sh']);
  // A new event removes the command from the old one (and the event it empties).
  const other = replaceHook(hooks, rows[2], { event: 'SessionStart', matcher: '', command: 'stop.sh', timeout: '' });
  assert.equal('Stop' in other, false);
  assert.equal(other.SessionStart[0].hooks[0].command, 'stop.sh');
  assert.equal(JSON.stringify(hooks), before, 'input unchanged');
});

test('a hook draft needs an event, a command and a whole-second timeout', () => {
  const ok = { event: 'Stop', matcher: '', command: 'x', timeout: '' };
  assert.equal(validateHookDraft(ok), null);
  assert.match(validateHookDraft({ ...ok, event: ' ' }), /event/i);
  assert.match(validateHookDraft({ ...ok, command: ' ' }), /command/i);
  assert.match(validateHookDraft({ ...ok, timeout: '1.5' }), /seconds/);
  assert.match(validateHookDraft({ ...ok, timeout: '0' }), /at least 1/);
});

test('a stale hooks save is recognized by the daemon\'s typed code', () => {
  assert.equal(isHooksConflict('daemon error -32602: claude_hooks_conflict: x changed [claude_hooks_conflict]'), true);
  assert.equal(isHooksConflict(new Error('daemon error: claude_hooks_conflict')), true);
  assert.equal(isHooksConflict('daemon error: boom'), false);
});

test('providers are read without any key, only its state and fingerprint', () => {
  const list = parseProviders({
    providers: [
      { id: ID.toUpperCase(), name: 'Local', base_url: 'http://localhost:8080/v1', default_model: 'm', api_key: { state: 'set', fingerprint: 'ab12', set_at: '2026-01-01T00:00:00Z' } },
      { id: 'x', name: 'Bare', base_url: 'https://b/v1', api_key: { state: 'absent' } },
      { nope: true },
    ],
  });
  assert.equal(list.length, 2);
  assert.equal(list[0].id, ID);
  assert.equal(keyStateText(list[0].api_key), 'API key set (ab12)');
  assert.equal(keyStateText(list[1].api_key), 'no API key');
  assert.equal(list[1].default_model, '');
  assert.equal(parseProviders({}), null);
  assert.ok(!JSON.stringify(list).includes('secret'));
});

const form = (over = {}) => ({ name: 'Local', base_url: 'http://localhost:8080/v1', default_model: '', api_key: '', clear_key: 'false', ...over });

test('a provider form validates and maps its key field to keep, set or clear', () => {
  assert.equal(validateProviderForm(form()), null);
  assert.match(validateProviderForm(form({ name: ' ' })), /Name/);
  assert.match(validateProviderForm(form({ base_url: 'localhost' })), /http/);
  assert.match(validateProviderForm(form({ api_key: 'sk-1', clear_key: 'true' })), /not both/);
  assert.deepEqual(providerPutArgs(null, form()), {
    id: null, name: 'Local', baseUrl: 'http://localhost:8080/v1', defaultModel: '', keyAction: 'keep', secret: null,
  });
  const set = providerPutArgs(ID, form({ api_key: ' sk-1 ' }));
  assert.equal(set.keyAction, 'set');
  assert.equal(set.secret, 'sk-1');
  assert.equal(set.id, ID);
  const clear = providerPutArgs(ID, form({ clear_key: 'true' }));
  assert.equal(clear.keyAction, 'clear');
  assert.equal(clear.secret, null);
  assert.ok(!JSON.stringify(providerPutArgs(ID, form())).includes('sk-'));
});

test('deleting a provider reports the roles it was detached from', () => {
  assert.equal(deleteProviderMessage('L', []), 'Deleted L');
  assert.equal(deleteProviderMessage('L', ['dream_model', 'odd_role']), 'Deleted L; detached: Dream model, odd_role');
  assert.equal(deleteProviderMessage('L', undefined), 'Deleted L');
});

test('bridges list both kinds, configured or not, and the form follows the daemon\'s rules', () => {
  const list = parseBridges({
    bridges: [
      { kind: 'signal', path: '/d/signal.toml', config: { enabled: true, account: '+1', allow_from: 'a', working_dir: '/w' } },
      { kind: 'imessage', path: '/d/imessage.toml', config: null },
      { kind: 'matrix', path: 'x', config: null },
    ],
  });
  assert.deepEqual(list.map((b) => b.kind), ['signal', 'imessage']);
  assert.equal(bridgeDetail(list[0]), 'enabled · /w');
  assert.equal(bridgeDetail(list[1]), 'not configured');
  const v = { enabled: 'true', account: ' +1 ', allow_from: ' a, b ', working_dir: ' /w ' };
  assert.equal(validateBridgeForm('signal', v), null);
  assert.match(validateBridgeForm('signal', { ...v, account: '' }), /account/);
  assert.equal(validateBridgeForm('imessage', { ...v, account: '' }), null);
  assert.match(validateBridgeForm('imessage', { ...v, working_dir: ' ' }), /Working directory/);
  assert.deepEqual(bridgePutArgs('signal', v), { kind: 'signal', enabled: true, account: '+1', allowFrom: 'a, b', workingDir: '/w' });
  assert.equal(bridgePutArgs('imessage', v).account, '', 'iMessage has no account');
  assert.equal(bridgePutArgs('signal', { ...v, enabled: 'false' }).enabled, false);
});

// The Tauri side: each command is one of the daemon's operator methods, and the
// webview adapter calls exactly the commands the Rust module defines.
test('the stored route per role is read without any key and marks the current provider', () => {
  const routes = parseRoutes({ routes: [
    { role: 'title_model', id: null, name: null, api_key: null },
    { role: 'dream_model', id: ID.toUpperCase(), name: 'Local', api_key: { state: 'set', fingerprint: 'ab12', secret: 'sk-LEAK' }, api_key_value: 'sk-LEAK' },
    { role: 'coding_model', id: ID, name: 'x', api_key: null },
    { role: 'prompt_compile_model', id: null, name: 'stale', api_key: { state: 'set' } },
  ] });
  assert.deepEqual(routes.map((r) => r.role), ['title_model', 'dream_model', 'prompt_compile_model']);
  assert.equal(routeOf(routes, 'title_model'), null);
  assert.equal(routeOf(routes, 'coding_model'), null);
  const dream = routeOf(routes, 'dream_model');
  assert.equal(dream.id, ID);
  assert.deepEqual(dream.api_key, { state: 'set', fingerprint: 'ab12' });
  assert.ok(!JSON.stringify(routes).includes('LEAK'));
  assert.equal(routeText(dream), 'Local · API key set (ab12)');
  assert.equal(routeText(null), 'default (not routed)');
  // A detached role never shows a stale name or key.
  assert.equal(routes[2].name, null);
  assert.equal(routes[2].api_key, null);
  assert.equal(parseRoutes({}), null);
  assert.equal(parseRoutes({ providers: [] }), null);
});

test('every Tauri command forwards one operator RPC and is wired into the app', () => {
  const rs = readFileSync(here('../src-tauri/src/operator_config.rs'), 'utf8');
  const wired = [
    ['list_claude_hooks', 'LIST_HOOKS', 'ListClaudeHooks'],
    ['put_claude_hooks', 'PUT_HOOKS', 'PutClaudeHooks'],
    ['delete_claude_hooks', 'DELETE_HOOKS', 'DeleteClaudeHooks'],
    ['list_custom_providers', 'LIST_PROVIDERS', 'ListCustomProviders'],
    ['put_custom_provider', 'PUT_PROVIDER', 'PutCustomProvider'],
    ['delete_custom_provider', 'DELETE_PROVIDER', 'DeleteCustomProvider'],
    ['set_custom_provider_route', 'SET_ROUTE', 'SetCustomProviderRoute'],
    ['list_message_bridges', 'LIST_BRIDGES', 'ListMessageBridges'],
    ['put_message_bridge', 'PUT_BRIDGE', 'PutMessageBridge'],
    ['delete_message_bridge', 'DELETE_BRIDGE', 'DeleteMessageBridge'],
  ];
  const api = readFileSync(here('../src/operator-config-api.ts'), 'utf8');
  const handlers = readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8');
  for (const [cmd, konst, method] of wired) {
    assert.match(rs, new RegExp(`pub async fn ${cmd}\\(`), cmd);
    assert.match(rs, new RegExp(`pub const ${konst}: &str = "${method}";`), konst);
    assert.match(rs, new RegExp(`forward\\(&state, ${konst},`), `${cmd} forwards ${konst}`);
    assert.ok(api.includes(`'${cmd}'`), `${cmd} is called by the adapter`);
    assert.ok(handlers.includes(`operator_config::${cmd},`), `${cmd} is in the generated handler list`);
  }
  assert.match(readFileSync(here('../src-tauri/src/modules.generated.rs'), 'utf8'), /^pub mod operator_config;$/m);
  // The route read is sanitized by an allowlist on the Rust side before the webview sees it.
  assert.match(rs, /pub async fn list_custom_provider_routes\(/);
  assert.match(rs, /pub const LIST_ROUTES: &str = "ListCustomProviderRoutes";/);
  assert.match(rs, /\.call\(LIST_ROUTES, json!\(\{\}\)\)/);
  assert.match(rs, /read_routes\(&state\.client\)/);
  assert.ok(api.includes("'list_custom_provider_routes'"));
  assert.ok(handlers.includes('operator_config::list_custom_provider_routes,'));
});

test('the Settings rows open these dialogs, and the provider key is write-only in the DOM code', () => {
  const src = readFileSync(here('../src/actions/operatorconfig.ts'), 'utf8');
  for (const row of ['claude_hooks', 'api_providers', 'title_model', 'prompt_compiler_model', 'memory_model', 'dream_model']) {
    assert.ok(src.includes(`'${row}'`), row);
  }
  assert.match(src, /registerRowOverlay\(`\$\{kind\}_bridge`/);
  assert.ok(!/console\./.test(src), 'no console logging');
  assert.ok(!/localStorage|sessionStorage|clipboard/.test(src), 'no storage or clipboard');
  assert.ok(!/innerHTML/.test(src), 'no HTML');
  assert.match(src, /type: 'password'/);
});
