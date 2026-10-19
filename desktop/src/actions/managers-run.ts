// The DOM side of the manager surfaces slice (actions/managers.ts); the
// feature code is managers.ts. Collected by actions/runtime.generated.ts.
import type { SliceRuntime } from '../commands.js';
import { initManagers, managersSnapshot, runManagers } from '../managers.js';
import { type ManagersActionId, SLICE } from './managers.js';

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runManagers(id as ManagersActionId, inv),
  snapshot: () => ({ managers: managersSnapshot() }),
  init: initManagers,
  initOrder: 55,
};
