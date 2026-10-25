// Slice registration (hub #3000): every slice registers from its own files.
// The generated indexes are current, the registry has no clashing
// registration, and every slice's pure SLICE has exactly one DOM RUNTIME.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ACTIONS, sliceProblems } from '../dist/registry.js';
import { SLICES } from '../dist/actions/index.generated.js';
import { aliasOf, generateAll, handlerPaths, renderIndex, tauriCommands } from '../scripts/slices.mjs';
import { mergeSidecars } from '../scripts/parity-lib.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

test('the generated slice index, runtime index and Tauri handler list are current', () => {
  for (const o of generateAll(here('..'))) {
    assert.equal(readFileSync(o.path, 'utf8'), o.text, `${o.path} is stale: run node scripts/slices.mjs`);
  }
});

test('the registered slices clash nowhere', () => {
  assert.deepEqual(sliceProblems(), []);
  assert.deepEqual(SLICES.map((s) => s.order), [...SLICES.map((s) => s.order)].sort((a, b) => a - b), 'SLICES is in order');
  const ids = ACTIONS.map((a) => a.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const s of SLICES) for (const a of s.actions) assert.ok(ids.includes(a.id), `${s.name}: ${a.id}`);
});

test('sliceProblems names a clashing registration', () => {
  const spec = (name, order, actions, rules, keys, commands) => ({ name, order, actions, rules, keys, commands });
  const act = (id) => ({ id, title: id, group: 'go' });
  const ok = () => null;
  // Slice 6 rules the core answer-question action and gap sweep G1 focus-composer; the stand-in keeps the core complete.
  const base = spec('base', 0, [], { 'answer-question': ok, 'focus-composer': ok });
  const problems = (slices) => sliceProblems([base, ...slices]);
  const a = spec('a', 1, [act('x-one')], { 'x-one': ok }, { zq: { action: 'x-one' } });
  assert.deepEqual(problems([a]), []);
  const twice = spec('b', 2, [act('x-one')], {});
  assert.ok(problems([a, twice]).some((p) => /x-one is registered by both a and b/.test(p)));
  const noRule = spec('c', 3, [act('x-two')], {});
  assert.ok(problems([noRule]).some((p) => /x-two \(c\) has no rule/.test(p)));
  const twoRules = spec('d', 4, [], { 'x-one': ok });
  assert.ok(problems([a, twoRules]).some((p) => /x-one has a rule in both a and d/.test(p)));
  const stray = spec('e', 5, [], { 'x-ghost': ok });
  assert.ok(problems([stray]).some((p) => /e has a rule for x-ghost/.test(p)));
  const rebind = spec('f', 6, [act('x-three')], { 'x-three': ok }, { zq: { action: 'x-three' } });
  assert.ok(problems([a, rebind]).some((p) => /key zq is bound to x-one by a and to x-three by f/.test(p)));
  const core = spec('g', 7, [act('x-four')], { 'x-four': ok }, { j: { action: 'x-four' } }, { quit: { action: 'x-four' } });
  const found = problems([core]);
  assert.ok(found.some((p) => /key j of g overrides the core binding \(cursor-down\)/.test(p)), found.join('\n'));
  assert.ok(found.some((p) => /:command quit of g overrides the core binding \(quit\)/.test(p)), found.join('\n'));
  const sameKey = spec('h', 8, [], {}, { zq: { action: 'x-one' } });
  assert.deepEqual(problems([a, sameKey]), [], 'two slices may bind one key to the same action');
});

test('sliceProblems names a key two slices route in raw listeners', () => {
  const ok = () => null;
  const act = (id, extraKeys) => ({ id, title: id, group: 'go', ...(extraKeys ? { extraKeys } : {}) });
  const spec = (name, order, rawKeys, actions = [], rules = {}, keys) => ({ name, order, actions, rules, keys, rawKeys });
  const base = spec('base', 0, undefined, [], { 'answer-question': ok, 'focus-composer': ok });
  const problems = (slices) => sliceProblems([base, ...slices]);
  // The Issue's case: two sweeps both raw-route Ctrl-H.
  const g1 = spec('g1', 1, ['C-h', 'C-l']);
  const g2 = spec('g2', 2, ['C-h', 'C-d']);
  assert.deepEqual(problems([g1]), []);
  assert.ok(problems([g1, g2]).some((p) => /raw key C-h is routed by both g1 and g2/.test(p)), problems([g1, g2]).join('\n'));
  assert.equal(problems([g1, g2]).filter((p) => /raw key/.test(p)).length, 1, 'only the shared token clashes');
  // A raw token another slice binds in the registry (`keys`; extraKeys are display hints and may repeat across panes), whichever order they are listed in.
  const binder = spec('binder', 3, undefined, [act('x-one', ['C-h'])], { 'x-one': ok }, { 'C-l': { action: 'x-one' }, 'C-d': { action: 'x-one' } });
  for (const order of [[g1, binder], [binder, g1]]) {
    assert.ok(problems(order).some((p) => /raw key C-l of g1 is also bound in the registry by binder/.test(p)), problems(order).join('\n'));
  }
  assert.ok(problems([g2, binder]).some((p) => /raw key C-d of g2 is also bound in the registry by binder/.test(p)));
  const hintOnly = spec('hints', 5, undefined, [act('x-three', ['C-h'])], { 'x-three': ok });
  assert.deepEqual(problems([g1, hintOnly]).filter((p) => /raw key/.test(p)), [], 'a display hint is no binding');
  // A slice may bind in the registry what it routes itself, and repeating a token in one slice is no clash.
  const own = spec('own', 4, ['C-l', 'C-l'], [act('x-two', ['C-l'])], { 'x-two': ok }, { 'C-l': { action: 'x-two' } });
  assert.deepEqual(problems([own]), []);
});

// Every raw global keydown/keyup listener must be accounted for (parallel sweeps once routed the same keys in two
// listeners and no check saw it). A listener in src/actions belongs to its slice, which must declare rawKeys; one
// elsewhere needs an entry here with the reason it cannot collide. Listeners on a dialog, form or field
// (dlg.onkeydown, el.addEventListener) only see keys while that element has focus, so they are not global.
const GLOBAL_KEY_LISTENER = /\b(?:window|document|win)\s*\.\s*(?:addEventListener\(\s*['"]key(?:down|up|press)['"]|onkey(?:down|up|press)\s*=)/;
const RAW_LISTENER_ALLOWLIST = {
  'assist.ts': 'Ctrl/Cmd-Alt-A and Ctrl/Cmd-Shift-A inside a draft only; the assist chords bind no registry key',
  'dag.ts': 'pane keys, only while the DAG pane is open (paneKeyAllowed, no dialog, no typing target)',
  'dashboard.ts': 'pane keys, only while the Global Dashboard (gd) is open',
  'esp.ts': 'pane keys, only while the ESP overlay is open',
  'files.ts': 'pane keys, only while the explorer or viewer has focus',
  'graphs.ts': 'pane keys, only while the graph pane is open',
  'issues.ts': 'pane keys, only while the Issues workspace is active',
  'managers.ts': 'pane keys, only while a manager view is open',
  'menus.ts': 'Cmd-N / Ctrl-N only (isNewSessionChord), the File menu New session chord with Vim keys off (#305)',
  'modal-gate.ts': 'the activation guard and dialog key gate: they stop keys, they route none',
  'palette.ts': 'Ctrl-K / Cmd-K only (isPaletteChord), the core palette chord',
  'prompt-creator.ts': 'keys inside the prompt-creator dialog only (window capture so it beats the vim-field handler)',
  'prompts.ts': 'keys inside the launch and prompt dialogs only (window capture so it beats the vim-field handler)',
  'settings.ts': 'pane keys, only while the settings pane is open',
  'todo.ts': 'Esc only, and only while the to-do panel is an open drawer (a narrow window); it routes no other key (local #291)',
  'vimkeys.ts': 'the vim key engine: it routes by the registry and the core keymap, not by raw tokens',
};

test('every raw global key listener is declared by its slice or allowlisted with a reason', () => {
  const src = here('../src');
  const walk = (d) => readdirSync(d, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(join(d, e.name)) : [join(d, e.name)]));
  const files = walk(src).filter((f) => f.endsWith('.ts') && !f.endsWith('.generated.ts'));
  const listeners = files.filter((f) => GLOBAL_KEY_LISTENER.test(readFileSync(f, 'utf8')));
  assert.ok(listeners.length >= 10, 'the scan finds the known listeners');
  const rel = (f) => f.slice(src.length + 1);
  // The slice file a src/actions module belongs to: itself when it has the SLICE, else the one its RUNTIME imports.
  const specOf = (f) => {
    const text = readFileSync(f, 'utf8');
    if (/^export const SLICE\b/m.test(text)) return f;
    const m = /import \{[^}]*\bSLICE\b[^}]*\} from '\.\/([\w-]+)\.js'/.exec(text);
    return m ? join(f, '..', `${m[1]}.ts`) : null;
  };
  const byName = new Map(SLICES.map((s) => [s.name, s]));
  const nameOf = (specFile) => /^  name: '([^']+)'/m.exec(readFileSync(specFile, 'utf8').slice(readFileSync(specFile, 'utf8').indexOf('export const SLICE')))?.[1];
  const used = new Set();
  const failures = [];
  for (const f of listeners) {
    const r = rel(f);
    if (r.startsWith('actions/')) {
      const spec = specOf(f);
      const slice = spec && byName.get(nameOf(spec));
      if (!slice) failures.push(`${r}: has a raw key listener but is not part of a slice; declare it in a slice or add it to RAW_LISTENER_ALLOWLIST with a reason`);
      else if (!(slice.rawKeys?.length > 0)) failures.push(`${r}: slice "${slice.name}" has a raw key listener but declares no rawKeys`);
      continue;
    }
    if (RAW_LISTENER_ALLOWLIST[r]) used.add(r);
    else failures.push(`${r}: has a raw key listener; declare rawKeys in its slice (src/actions) or add it to RAW_LISTENER_ALLOWLIST with a reason`);
  }
  assert.deepEqual(failures, []);
  assert.deepEqual(Object.keys(RAW_LISTENER_ALLOWLIST).filter((k) => !used.has(k)), [], 'allowlist entries that no longer have a listener');
  for (const [k, why] of Object.entries(RAW_LISTENER_ALLOWLIST)) assert.ok(why.length > 10, `${k} needs a reason`);
});

test('the raw key scan sees window, document and win listeners but not element-local ones', () => {
  assert.ok(GLOBAL_KEY_LISTENER.test("window.addEventListener('keydown', (ev) => {"));
  assert.ok(GLOBAL_KEY_LISTENER.test('document.addEventListener("keyup", f)'));
  assert.ok(GLOBAL_KEY_LISTENER.test("win.addEventListener('keydown', f, true)"));
  assert.ok(GLOBAL_KEY_LISTENER.test('document.onkeydown = f'));
  assert.ok(!GLOBAL_KEY_LISTENER.test("dlg.addEventListener('keydown', f)"));
  assert.ok(!GLOBAL_KEY_LISTENER.test('dlg.onkeydown = f'));
  assert.ok(!GLOBAL_KEY_LISTENER.test("window.addEventListener('resize', f)"));
});

test('every slice with actions has exactly one RUNTIME that runs it', () => {
  const dir = here('../src/actions');
  const files = readdirSync(dir).filter((f) => f.endsWith('.ts') && !f.endsWith('.generated.ts'));
  const specs = files.filter((f) => /^export const SLICE\b/m.test(readFileSync(`${dir}/${f}`, 'utf8')));
  const runtimeOf = new Map();
  for (const f of files) {
    const text = readFileSync(`${dir}/${f}`, 'utf8');
    if (!/^export const RUNTIME\b/m.test(text)) continue;
    const m = /import \{[^}]*\bSLICE\b[^}]*\} from '\.\/([\w-]+)\.js'/.exec(text);
    assert.ok(m, `${f}: RUNTIME imports the SLICE it runs`);
    const spec = `${m[1]}.ts`;
    assert.ok(!runtimeOf.has(spec), `${spec} is run by both ${runtimeOf.get(spec)} and ${f}`);
    runtimeOf.set(spec, f);
  }
  for (const f of specs) assert.ok(runtimeOf.has(f), `${f} has a RUNTIME`);
});

test('the generator names slices by file and orders the union', () => {
  assert.equal(aliasOf('detail-spec.ts'), 'detail_spec');
  const text = renderIndex(['a-spec.ts', 'b.ts']);
  assert.match(text, /import \{ SLICE as a_spec \} from '\.\/a-spec\.js';/);
  assert.match(text, /\| IdsOf<typeof b>;/);
  assert.match(renderIndex([]), /export type SliceActionId = never;/);
});

test('Tauri commands are found past doc comments and attributes, and a name defined twice fails', () => {
  const rs = [
    '/// Doc.', '#[tauri::command]', '/// More doc.', '#[allow(clippy::too_many_arguments)]', 'pub async fn one(x: u8) {}',
    '#[tauri::command]', 'fn two() {}', 'fn not_a_command() {}',
  ].join('\n');
  assert.deepEqual(tauriCommands(rs), ['one', 'two']);
  assert.deepEqual(handlerPaths([{ file: 'm.rs', text: rs }, { file: 'lib.rs', text: '#[tauri::command]\npub fn root() {}' }]), ['root', 'm::one', 'm::two']);
  assert.throws(() => handlerPaths([{ file: 'a.rs', text: rs }, { file: 'b.rs', text: '#[tauri::command]\npub fn one() {}' }]), /one is defined in both a\.rs and b\.rs/);
  const handlers = readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8');
  assert.ok(handlers.includes('    lifecycle::unarchive_session,'), 'the one unarchive command is registered');
  assert.match(readFileSync(here('../src-tauri/src/lib.rs'), 'utf8'), /\.invoke_handler\(include!\("handlers\.generated\.rs"\)\)/);
});

test('parity status files merge, and a row in two files fails', () => {
  const merged = mergeSidecars([
    { name: 'a.json', data: { version: 1, rows: { 'cmd:help': { status: 'done' } } } },
    { name: 'b.json', data: { version: 1, rows: { 'cmd:quit': { status: 'partial' } } } },
  ]);
  assert.deepEqual(Object.keys(merged.rows), ['cmd:help', 'cmd:quit']);
  assert.equal(merged.sources['cmd:quit'], 'b.json');
  assert.throws(() => mergeSidecars([
    { name: 'a.json', data: { version: 1, rows: { 'cmd:help': { status: 'done' } } } },
    { name: 'b.json', data: { version: 1, rows: { 'cmd:help': { status: 'partial' } } } },
  ]), /cmd:help is in both parity-status\.d\/a\.json and parity-status\.d\/b\.json/);
  assert.throws(() => mergeSidecars([{ name: 'c.json', data: { rows: [] } }]), /parity-status\.d\/c\.json: expected/);
});
