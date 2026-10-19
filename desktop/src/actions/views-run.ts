// The DOM side of the G4 overlay slice (actions/views.ts): startup wiring for
// the shared widgets. Collected by actions/runtime.generated.ts.
import type { SliceRuntime } from '../commands.js';
import { initModelDropdown } from '../model-dropdown.js';
import { initPrompts } from '../prompts.js';
import { BUDGETS_CSS } from '../budgets.js';
import { TEXT_BG_CSS, applyTextBg, openTextBgEditor } from '../textbg.js';
import { SLICE, type ViewsActionId } from './views.js';

function run(id: ViewsActionId): void {
  switch (id) {
    case 'text-area-background': openTextBgEditor(() => undefined); return;
  }
}

function init(): void {
  initModelDropdown();
  const style = document.createElement('style');
  style.id = 'g4-textbg-style';
  style.textContent = TEXT_BG_CSS + BUDGETS_CSS;
  document.head.append(style);
  applyTextBg();
  initPrompts();
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id) => run(id as ViewsActionId),
  init,
  initOrder: 96,
};
