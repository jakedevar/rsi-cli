// The DOM side of the project-files slice (actions/files.ts); the feature code
// is files.ts. Collected by actions/runtime.generated.ts.
import type { SliceRuntime } from '../commands.js';
import { filesSnapshot, initFiles, runFiles } from '../files.js';
import { type FilesActionId, SLICE } from './files.js';

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runFiles(id as FilesActionId, inv),
  snapshot: () => ({ files: filesSnapshot() }),
  init: initFiles,
  // Its capture keydown listener registers after the Issues workspace's (20) and the terminal's (50).
  initOrder: 55,
};
