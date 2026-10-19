// Styles of the gap sweep G2 surfaces (hub #3000): the session-row markers,
// the create-entity form and the model dropdown. Injected once by the slices
// that use them, so the sweep adds no static file. Colours are the desktop
// CSS variables the theme sets.
export const SWEEP_CSS = `
.session-item .row-mark { font-size: 11px; margin-right: 4px; font-weight: 600; }
.row-mark.mark-pin { color: var(--mauve); }
.row-mark.mark-testing { color: var(--yellow); }
.row-mark.mark-rotation { color: var(--peach); }
.row-mark.mark-retry, .row-mark.mark-depth { color: var(--subtext); font-weight: 400; }
dialog.entity-dialog { width: min(620px, 94vw); }
.entity-kinds { display: flex; flex-wrap: wrap; gap: 4px; }
.entity-kinds .seg[aria-pressed="true"] { background: var(--surface1); color: var(--text); border-color: var(--mauve); }
.entity-kinds .seg:disabled { opacity: 0.4; cursor: not-allowed; }
.entity-chips { display: flex; flex-wrap: wrap; gap: 4px; margin-top: 4px; }
.entity-chip { font-size: 11px; border-radius: 8px; padding: 1px 8px; background: var(--surface0); border: 1px solid var(--surface1); }
.entity-chip.bad { border-color: var(--red); color: var(--red); }
.entity-row { display: flex; gap: 8px; flex-wrap: wrap; }
.entity-row > label { flex: 1 1 140px; }
.entity-check { display: flex; align-items: center; gap: 6px; }
.issues-tabs { display: inline-flex; gap: 0; margin: 0 8px; }
.iv-assoc { margin: 8px 0; }
.iv-assoc .assoc { cursor: pointer; padding: 2px 6px; border-radius: 4px; }
.iv-assoc .assoc.selected { background: var(--surface0); box-shadow: inset 2px 0 var(--blue); }
`;

const injected = new Set<string>();

/** Add one stylesheet to the document, once. */
export function injectCss(id: string, css: string): void {
  if (injected.has(id) || document.getElementById(id)) { injected.add(id); return; }
  const style = document.createElement('style');
  style.id = id;
  style.textContent = css;
  document.head.append(style);
  injected.add(id);
}
