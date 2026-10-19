// The DOM side of the shared fold keys (actions/folds.ts): zo zc za zM zR run
// slice 5's transcript fold when the transcript has the keys, else slice 4's
// list fold. Collected by actions/runtime.generated.ts.
import { type SliceRuntime, snapshot } from '../commands.js';
import { runDetail } from './detail.js';
import { SLICE, type FoldActionId, foldPane } from './folds.js';
import { runSessionList } from './sessionlist-run.js';

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id, inv) => {
    if (foldPane(snapshot()) === 'transcript') runDetail(id as FoldActionId, inv);
    else runSessionList(id, inv);
  },
};
