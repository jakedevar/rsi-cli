// Styles of the graph review, the recursive DAG browser, the ESP Square and
// the topology preview (slice 11, hub #3013). Injected at startup by
// actions/graphs-run.ts, so the slice adds no static file. Colours are the
// desktop CSS variables the theme sets; the ESP squares keep the TUI's fixed
// feedback colours (esp-model.ts).
export const GRAPHS_CSS = `
.graph-pane, .dag-pane { flex: 1; display: flex; flex-direction: column; min-height: 0; background: var(--base); outline: none; }
.graph-pane[hidden], .dag-pane[hidden], .esp-overlay[hidden] { display: none; }
.graph-head, .dag-head { display: flex; align-items: center; gap: 8px; padding: 8px 14px; border-bottom: 1px solid var(--surface0); background: var(--mantle); }
.graph-head h1, .dag-head h1 { font-size: 15px; margin: 0 8px 0 0; color: var(--mauve); }
.graph-head .spacer, .dag-head .spacer { flex: 1; }
.graph-head-title { display: flex; align-items: center; gap: 6px; min-width: 0; }
.gv-name { font-weight: 600; color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 34ch; }
.graph-head .badge { font-size: 11px; padding: 1px 6px; border-radius: 8px; border: 1px solid var(--surface1); color: var(--subtext); }
.gv-badge-ro { color: var(--peach) !important; border-color: var(--peach) !important; }
.gv-badge-dirty, .gv-badge-save-failed, .gv-badge-bad, .gv-badge-cycle { color: var(--yellow) !important; border-color: var(--yellow) !important; }
.gv-badge-save-failed, .gv-badge-bad { color: var(--red) !important; border-color: var(--red) !important; }
.gv-badge-run.running, .gv-badge-run.accepted, .gv-badge-run.succeeded { color: var(--green) !important; border-color: var(--green) !important; }
.gv-badge-run.failed, .gv-badge-run.blocked { color: var(--red) !important; border-color: var(--red) !important; }
.graph-body { flex: 1; display: grid; grid-template-columns: minmax(0, 1fr) minmax(260px, 360px); min-height: 0; }
.graph-canvas { position: relative; overflow: hidden; min-height: 0; background: var(--crust); }
.graph-svg { width: 100%; height: 100%; display: block; font-family: var(--mono); font-size: 12px; }
.gv-edge { fill: none; stroke: var(--overlay0); stroke-width: 1.5; }
.gv-edge.near { stroke: var(--subtext); stroke-width: 2; }
.gv-edge.loop { stroke: var(--yellow); stroke-dasharray: 5 3; }
.gv-edge.bad { stroke: var(--red); }
.gv-edge-label { fill: var(--subtext); font-size: 10px; }
.gv-node rect { fill: var(--surface0); stroke-width: 1.5; cursor: pointer; }
.gv-node.sel rect { fill: var(--surface1); stroke: var(--yellow) !important; stroke-width: 2.5; }
.gv-node-title { fill: var(--text); pointer-events: none; }
.gv-node.sel .gv-node-title { font-weight: 700; }
.gv-node-badge { pointer-events: none; font-weight: 700; }
.graph-empty { position: absolute; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; color: var(--subtext); font-family: var(--mono); }
.graph-empty h3 { color: var(--mauve); }
.graph-empty p { margin: 2px 0; }
.graph-side { overflow: auto; padding: 10px 12px; border-left: 1px solid var(--surface0); background: var(--mantle); }
.graph-side h3 { margin: 0 0 6px; font-size: 14px; color: var(--blue); }
.graph-side h4 { margin: 10px 0 4px; font-size: 12px; color: var(--mauve); text-transform: uppercase; letter-spacing: .04em; }
.gv-facts { display: grid; grid-template-columns: max-content 1fr; gap: 2px 10px; margin: 0; font-size: 12px; }
.gv-facts dt { color: var(--overlay0); }
.gv-facts dd { margin: 0; color: var(--text); word-break: break-word; }
.gv-instructions, .topo-preview, .dag-pre { white-space: pre-wrap; font-family: var(--mono); font-size: 12px; background: var(--base); border: 1px solid var(--surface0); border-radius: 6px; padding: 6px 8px; margin: 4px 0; max-height: 260px; overflow: auto; }
.gv-edges, .gv-picker, .gv-diags { list-style: none; margin: 0; padding: 0; font-family: var(--mono); font-size: 12px; }
.gv-edges li, .gv-diags li { padding: 2px 4px; border-radius: 4px; }
.gv-edges li.sel, .gv-picker li.sel { background: var(--surface1); box-shadow: inset 3px 0 var(--yellow); }
.gv-diags li.bad { color: var(--red); }
.gv-diags li.warn { color: var(--yellow); }
.gv-picker li { display: flex; flex-direction: column; padding: 4px 6px; border-radius: 4px; cursor: pointer; }
.gv-pick-label { color: var(--text); font-weight: 600; }
.gv-pick-detail { color: var(--overlay0); font-size: 11px; }
.gv-mini { width: 100%; max-height: 140px; margin-top: 6px; background: var(--crust); border-radius: 6px; }
.gv-mini-node { fill: var(--surface0); stroke-width: 1; }
.gv-edit-input { width: 100%; box-sizing: border-box; background: var(--base); color: var(--text); border: 1px solid var(--blue); border-radius: 6px; padding: 4px 6px; font-family: var(--mono); }
textarea.gv-edit-input { min-height: 140px; resize: vertical; }
.gv-run { margin-top: 10px; padding-top: 8px; border-top: 1px solid var(--surface0); font-family: var(--mono); font-size: 12px; white-space: pre-wrap; }
.gv-run-title { color: var(--mauve); font-weight: 700; }
.gv-run-current { color: var(--yellow); font-weight: 700; }
.gv-run-warn { color: var(--yellow); }
.gv-run-hint { color: var(--overlay0); }
.graph-status, .dag-footer { padding: 4px 14px; border-top: 1px solid var(--surface0); background: var(--mantle); font-size: 11px; }
.dag-caps { padding: 2px 14px; font-family: var(--mono); font-size: 11px; }
.dag-tabs { display: flex; flex-wrap: wrap; padding: 4px 14px; }
.dag-control-slot:empty, .dag-gates[hidden] { display: none; }
.dag-control, .dag-gates { margin: 4px 14px; padding: 6px 10px; border: 1px solid var(--yellow); border-radius: 6px; display: flex; flex-wrap: wrap; align-items: center; gap: 8px; font-size: 12px; }
.dag-gates { flex-direction: column; align-items: flex-start; border-color: var(--surface1); font-family: var(--mono); }
.dag-control input { background: var(--base); color: var(--text); border: 1px solid var(--surface1); border-radius: 6px; padding: 3px 6px; font-family: var(--mono); }
.dag-warn { color: var(--yellow); }
.dag-body { flex: 1; overflow: auto; padding: 4px 14px 14px; font-size: 12px; }
.dag-message { color: var(--yellow); }
.dag-section { margin: 10px 0; }
.dag-section h3 { margin: 0 0 4px; font-size: 12px; letter-spacing: .04em; color: var(--overlay0); cursor: pointer; }
.dag-section.active h3 { color: var(--yellow); }
.dag-kv { margin: 1px 0; font-family: var(--mono); }
.dag-k { color: var(--overlay0); margin-right: 8px; }
.dag-table { border-collapse: collapse; font-family: var(--mono); font-size: 12px; width: 100%; }
.dag-table th { text-align: left; color: var(--overlay0); font-weight: 400; padding: 1px 6px; }
.dag-table td { padding: 1px 6px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 40ch; }
.dag-table tr { cursor: pointer; }
.dag-table.active tr.sel { background: var(--surface1); }
.dag-mark { color: var(--yellow); width: 1ch; }
.dag-inspector { margin-top: 6px; padding: 6px 8px; border: 1px solid var(--surface1); border-radius: 6px; }
.esp-overlay { position: fixed; inset: 0; z-index: 40; display: flex; align-items: center; justify-content: center; background: rgba(0, 0, 0, .45); outline: none; }
.esp-card { background: var(--mantle); border: 1px solid var(--surface1); border-radius: 12px; padding: 14px 18px; min-width: 300px; font-family: var(--mono); }
.esp-card h2 { margin: 0 0 6px; font-size: 14px; color: var(--mauve); }
.esp-head { display: flex; gap: 14px; color: var(--text); margin-bottom: 10px; }
.esp-grid { display: grid; grid-template-columns: repeat(3, 72px); grid-template-rows: repeat(3, 56px); gap: 8px; justify-content: center; }
.esp-sq { background: transparent; border: 2px solid; border-radius: 10px; font: 700 16px var(--mono); cursor: pointer; opacity: .9; }
.esp-sq.cursor { border-width: 3px; }
.esp-foot { margin-top: 10px; }
.esp-message { font-weight: 700; color: var(--text); margin-bottom: 4px; }
.topo-confirm h2 { margin-top: 0; }
`;
