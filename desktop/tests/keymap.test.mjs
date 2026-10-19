import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { keymapIsFresh, parseCommands, parseNormal, renderKeymap } from '../scripts/keymap.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = readFileSync(join(here, 'fixtures', 'keymap-excerpt.md'), 'utf8');

test('src/keymap.generated.ts matches docs/keybindings.md (run `node scripts/keymap.mjs` if not)', () => {
  assert.equal(keymapIsFresh(), true);
});

test('parses the generated normal-mode rows and reserved sequences', () => {
  const { rows, reserved } = parseNormal(fixture);
  assert.deepEqual(rows, [
    { keys: 'j', title: 'Move selection or scroll down', description: 'Moves the selection down one row.' },
    { keys: '<Space>gp', title: 'Edit manager policy', description: 'Opens the policy editor.' },
    { keys: 'Ctrl-Alt-G', title: 'Contextual help', description: 'Opens help.' },
  ]);
  assert.deepEqual(reserved, [
    { keys: '<Space>', kind: 'prefix', why: 'leader key' },
    { keys: 'gt', kind: 'no-op', why: 'retired TaskRabbit zone chord; use gs / gj / ga' },
  ]);
});

test('parses command spellings, aliases, argument arity and escaped pipes', () => {
  assert.deepEqual(parseCommands(fixture), [
    { name: 'quit', aliases: ['q', 'q!'], arg: 'none', title: 'Quit', description: 'Quits rsi.' },
    { name: 'manual', aliases: ['man'], arg: 'optional', title: 'Open manual', description: 'Opens the manual.' },
    { name: 'project-delete', aliases: [], arg: 'required', title: 'Delete project', description: 'Deletes the named project.' },
    { name: 'topology-resolve', aliases: [], arg: 'optional', title: 'Resolve', description: '`inspect|accept` work.' },
  ]);
});

test('a missing generated block is a loud error', () => {
  assert.throws(() => renderKeymap('# nothing here'), /no generated block "normal"/);
});

test('the real keybindings.md yields the keys and commands the engine binds', () => {
  const md = readFileSync(join(here, '..', '..', 'docs', 'keybindings.md'), 'utf8');
  const keys = parseNormal(md).rows.map((r) => r.keys);
  for (const k of ['j', 'k', 'gg', 'G', 'Enter', '<Space>a', '<Space>gq', 'Ctrl-N', 'X', ']a']) {
    assert.ok(keys.includes(k), k);
  }
  const cmds = parseCommands(md).map((c) => c.name);
  for (const c of ['archive', 'continue', 'pause clear', 'manager global appoint']) assert.ok(cmds.includes(c), c);
});
