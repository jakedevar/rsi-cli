// Styles of the settings pane, theme dialogs and help overlay (slice 7). The
// slice injects them at startup, so it adds no static file and no copy step.
// Colours are the desktop CSS variables, which the theme sets (theme-model.ts).
export const SETTINGS_CSS = `
.settings-pane { flex: 1; display: flex; flex-direction: column; min-height: 0; background: var(--base); outline: none; }
.settings-pane[hidden] { display: none; }
.settings-head { display: flex; align-items: center; gap: 8px; padding: 8px 14px; border-bottom: 1px solid var(--surface0); background: var(--mantle); }
.settings-head h1 { font-size: 15px; margin: 0 8px 0 0; color: var(--mauve); }
.settings-head .spacer { flex: 1; }
.settings-search { width: 260px; background: var(--base); color: var(--text); border: 1px solid var(--surface1); border-radius: 6px; padding: 4px 8px; }
.settings-search[hidden] { display: none; }
.settings-body { flex: 1; display: grid; grid-template-columns: minmax(180px, 230px) minmax(0, 1fr) minmax(220px, 300px); min-height: 0; }
.settings-rail { display: flex; flex-direction: column; gap: 2px; padding: 8px; overflow: auto; border-right: 1px solid var(--surface0); background: var(--mantle); }
.settings-group { display: flex; flex-direction: column; align-items: flex-start; text-align: left; gap: 2px; padding: 6px 8px; border: 0; border-radius: 6px; background: transparent; color: var(--subtext); cursor: pointer; font: inherit; }
.settings-group .sg-label { font-size: 12px; font-weight: 600; letter-spacing: .03em; color: var(--text); }
.settings-group .sg-summary { font-size: 11px; color: var(--overlay0); }
.settings-group.sel { background: var(--surface0); box-shadow: inset 3px 0 var(--blue); }
.settings-rail.focused .settings-group.sel { background: var(--surface1); }
.settings-main { display: flex; flex-direction: column; min-width: 0; min-height: 0; padding: 8px 12px; }
.settings-tabs { display: flex; flex-wrap: wrap; gap: 0; margin-bottom: 6px; }
.settings-tabs .seg { background: var(--base); color: var(--subtext); border: 1px solid var(--surface1); padding: 4px 10px; cursor: pointer; font: inherit; font-size: 12px; }
.settings-tabs .seg.active { background: var(--surface0); color: var(--text); box-shadow: inset 0 -2px var(--blue); }
.settings-section-summary { font-size: 12px; margin: 2px 0 8px; }
.settings-rows { list-style: none; margin: 0; padding: 0; overflow: auto; flex: 1; }
.settings-row { display: grid; grid-template-columns: 14px minmax(0, 1fr) auto 16px; align-items: center; gap: 8px; padding: 5px 8px; border-radius: 6px; cursor: default; }
.settings-row.sel { background: var(--surface0); }
.settings-rows.focused .settings-row.sel { box-shadow: inset 3px 0 var(--blue); }
.settings-row.partial .sv-label { color: var(--overlay0); }
.settings-row.hit .sv-label { text-decoration: underline var(--yellow); }
.sv-dirty { color: var(--yellow); font-size: 11px; }
.sv-value { font-family: var(--mono); font-size: 12px; color: var(--subtext); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 360px; cursor: pointer; }
.sv-switch.on { color: var(--green); }
.sv-choice, .sv-local { color: var(--blue); }
.sv-partial { color: var(--overlay0); font-family: inherit; font-style: italic; cursor: help; }
.settings-row.tui-only .sv-label { color: var(--overlay0); }
.sv-tui-only { color: var(--overlay0); font-family: inherit; font-style: italic; cursor: help; }
.usage-dialog { min-width: 520px; max-width: 760px; }
.usage-dialog h3 { margin: 10px 0 4px; font-size: 11px; letter-spacing: .06em; text-transform: uppercase; color: var(--overlay0); }
.usage-label { font-weight: 600; }
.usage-value { color: var(--subtext); }
.sv-apply { color: var(--peach); font-size: 12px; }
.settings-info { padding: 12px; border-left: 1px solid var(--surface0); background: var(--mantle); overflow: auto; font-size: 12px; }
.settings-info h3 { margin: 0 0 6px; font-size: 13px; color: var(--text); }
.settings-info p { margin: 0 0 8px; color: var(--subtext); }
.settings-info dl { display: grid; grid-template-columns: auto 1fr; gap: 4px 8px; margin: 0; }
.settings-info dt { color: var(--overlay0); }
.settings-info dd { margin: 0; color: var(--text); overflow-wrap: anywhere; }
.theme-swatch { display: inline-flex; gap: 2px; margin-left: auto; }
.chip { display: inline-block; width: 12px; height: 12px; border-radius: 3px; border: 1px solid var(--surface1); }
dialog.theme-dialog { min-width: 460px; }
dialog.keys-help { width: min(960px, 94vw); height: min(80vh, 760px); background: var(--mantle); color: var(--text); padding: 0; display: none; }
dialog.keys-help[open] { display: flex; flex-direction: column; }
.keys-help-tabs { display: flex; gap: 0; padding: 10px 12px 6px; }
.keys-help-tabs .seg { background: var(--base); color: var(--subtext); border: 1px solid var(--surface1); padding: 4px 12px; cursor: pointer; font: inherit; }
.keys-help-tabs .seg.active { background: var(--surface0); color: var(--text); box-shadow: inset 0 -2px var(--blue); }
.keys-help-search { padding: 0 12px 6px; font-family: var(--mono); color: var(--yellow); }
.keys-help-search[hidden] { display: none; }
.keys-help-body { flex: 1; overflow: auto; padding: 0 12px; outline: none; }
.keys-help-section { margin: 10px 0 4px; font-size: 11px; text-transform: uppercase; letter-spacing: .04em; color: var(--mauve); }
.keys-help-row { display: grid; grid-template-columns: minmax(120px, 200px) minmax(140px, 240px) 1fr; gap: 10px; padding: 3px 0; font-size: 12px; border-bottom: 1px solid var(--surface0); }
.keys-help-row .k { display: flex; flex-wrap: wrap; gap: 4px; }
.keys-help-row kbd { font-family: var(--mono); font-size: 11px; padding: 0 5px; border: 1px solid var(--surface1); border-radius: 4px; background: var(--base); color: var(--text); }
.keys-help-row .d { color: var(--subtext); }
.keys-help-row.unbound .t, .keys-help-row.unbound kbd { color: var(--overlay0); }
.keys-help-foot { padding: 6px 12px; font-size: 11px; color: var(--overlay0); border-top: 1px solid var(--surface0); }
`;
