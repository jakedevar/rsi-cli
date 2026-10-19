// The DOM side of the assist overlays' slice (actions/assist.ts); the feature
// code is assist.ts. Collected by actions/runtime.generated.ts.
import { assistSnapshot, initAssist, runAssist } from '../assist.js';
import type { SliceRuntime } from '../commands.js';
import { type AssistActionId, SLICE } from './assist.js';

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runAssist(id as AssistActionId, inv),
  snapshot: () => ({ assist: assistSnapshot() }),
  init: initAssist,
  // Its capture keydown listener (the AI chords in a draft) registers after the files slice's (55).
  initOrder: 70,
};
