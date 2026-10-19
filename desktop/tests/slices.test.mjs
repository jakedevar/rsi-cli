// Slice registration (hub #3000): every slice registers from its own files.
// The generated indexes are current, the registry has no clashing
// registration, and every slice's pure SLICE has exactly one DOM RUNTIME.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
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
