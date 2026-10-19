// Tool projection (gap sweep G4, hub #3000): exact pairing by tool_use_id,
// parallel batches, pending and orphan rows, transparent System rows and
// unpaired pre-migration rows (crates/rsi/src/ui/tool_projection.rs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { callNote, isTransparent, project, resultNote } from '../dist/tool-projection.js';

let seq = 0;
const ev = (event_type, over = {}) => ({ id: 0, session_id: 's', sequence: ++seq, event_type, created_at: '', content: 'x', ...over });

test('a result pairs with the call carrying its id, however far apart', () => {
  seq = 0;
  const events = [
    ev('ToolUse', { tool_name: 'Read', tool_use_id: 'a' }),   // 1
    ev('ToolUse', { tool_name: 'Grep', tool_use_id: 'b' }),   // 2
    ev('System', { content: '' }),                            // 3 transparent
    ev('ToolUse', { tool_name: 'Bash', tool_use_id: 'c' }),   // 4
    ev('ToolResult', { tool_use_id: 'b' }),                   // 5
    ev('ToolResult', { tool_use_id: 'a' }),                   // 6
    ev('Message', { role: 'Assistant' }),                     // 7
    ev('ToolResult', { tool_use_id: 'zz' }),                  // 8 orphan
    ev('ToolUse', { tool_name: 'Old' }),                      // 9 legacy
    ev('ToolResult', {}),                                     // 10 legacy
  ];
  const p = project(events);
  assert.equal(p.calls.get(1).result, 6);
  assert.equal(p.calls.get(2).result, 5);
  assert.equal(p.calls.get(4).result, null, 'still running');
  assert.deepEqual(p.batches.get(1), { calls: 3, pending: 1 }, 'the transparent row neither joins nor ends the batch');
  assert.equal(callNote(p, 1), '✓ answered · 3 parallel calls, 1 running');
  assert.equal(callNote(p, 4), '⧗ running');
  assert.equal(resultNote(p, 5), '↳ Grep #2');
  assert.equal(resultNote(p, 8), 'no matching call');
  assert.equal(callNote(p, 9), '', 'pre-migration rows are never paired');
  assert.equal(resultNote(p, 10), '');
  assert.equal(isTransparent(ev('System', { content: '  ' })), true);
  assert.equal(isTransparent(ev('Message', { content: '' })), false);
});
