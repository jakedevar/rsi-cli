// The DOM side of the Issues workspace slice (actions/issues.ts); the feature
// code is issues.ts. Collected by actions/runtime.generated.ts.
import type { SliceRuntime } from '../commands.js';
import { initIssues, issuesSnapshot, runIssues } from '../issues.js';
import { type IssuesActionId, SLICE } from './issues.js';

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => runIssues(id as IssuesActionId, inv),
  snapshot: () => ({ issues: issuesSnapshot() }),
  init: initIssues,
  initOrder: 20,
};
