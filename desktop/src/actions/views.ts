// Gap sweep G4 (hub #3000): the overlay rows' registry entries, pure (no DOM).
// The widgets themselves are shared modules: the model dropdown
// (model-dropdown*.ts), the prompts' chords and settings side (prompt*.ts),
// the text-area background (textbg.ts) and the pickers' Space leader
// (overlay-keys.ts). RUNTIME is actions/views-run.ts.
import type { ActionSpec, SliceSpec } from '../registry.js';

export type ViewsActionId = 'text-area-background';

export const VIEWS_ACTIONS: readonly ActionSpec[] = [
  {
    id: 'text-area-background', title: 'Text area background colour…', group: 'view', noMenu: true,
    detail: 'The TUI’s hex editor: #RRGGBB paints behind every text field (Settings › Appearance › Screen switches it on)',
  },
];

export const VIEWS_RULES: Readonly<Record<ViewsActionId, () => string | null>> = {
  'text-area-background': () => null,
};

export const SLICE: SliceSpec<ViewsActionId> = {
  name: 'overlays (gap sweep G4, hub #3000)',
  order: 96,
  actions: VIEWS_ACTIONS,
  rules: VIEWS_RULES,
};
