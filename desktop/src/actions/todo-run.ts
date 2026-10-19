// The DOM side of the operator to-do slice (actions/todo.ts); the feature code
// is todo.ts. Collected by actions/runtime.generated.ts.
import type { SliceRuntime } from '../commands.js';
import { initTodo, runTodo, todoSnapshot } from '../todo.js';
import { SLICE, type TodoActionId } from './todo.js';

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runTodo(id as TodoActionId, inv),
  snapshot: () => ({ todo: todoSnapshot() }),
  init: initTodo,
  initOrder: 72,
};
