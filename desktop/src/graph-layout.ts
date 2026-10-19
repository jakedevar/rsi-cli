// Deterministic layered layout for workflow graphs (hub #3013, slice 11): a
// pure TypeScript port of the TUI's crates/rsi/src/ui/overlay/graph_layout.rs.
// Positions are in the TUI's virtual cell units (one column, one row); the
// SVG renderer scales them, so the desktop draws the same shape the TUI does.
//
// The algorithm: weakly connected components (in node order) are stacked top
// to bottom; inside a component, Kahn layers left to right (a cycle is broken
// by taking the lowest remaining node and flagged), each layer's nodes are
// stacked and centred in the component's tallest layer; edges are routed as
// orthogonal polylines (forward, backward around the top, or vertical around
// the right). No DOM; tested in tests/graphs.test.mjs.

export const NODE_BOX_HEIGHT = 3;
export const NODE_LABEL_MIN_WIDTH = 14;
export const NODE_LABEL_MAX_WIDTH = 28;
export const HORIZONTAL_PAN_STEP = 12;
export const VERTICAL_PAN_STEP = 4;

const COLUMN_GAP = 12;
const ROW_GAP = 4;
const COMPONENT_GAP_Y = 6;
const LANE_STEP = 2;

/** The parts of a WorkflowDefinition the layout reads (rsi_graph::format). */
export interface LayoutPort { name: string }
export interface LayoutNode {
  id: string;
  name: string;
  inputs?: readonly LayoutPort[];
  outputs?: readonly LayoutPort[];
}
export interface LayoutEdge {
  source: string;
  target: string;
  source_port?: string | null;
  target_port?: string | null;
  label?: string | null;
}
export interface LayoutWorkflow {
  nodes: readonly LayoutNode[];
  edges: readonly LayoutEdge[];
  metadata?: Record<string, unknown>;
}

export interface Point { x: number; y: number }
export interface Rect { x: number; y: number; width: number; height: number }
export type Direction = 'left' | 'right' | 'up' | 'down';
export type EdgeKind = 'executable' | 'visual-loop';

export interface NodeLayout { nodeIndex: number; rect: Rect; innerWidth: number }
export interface Segment { start: Point; end: Point }
export interface EdgeRoute {
  kind: EdgeKind;
  label: string | null;
  sourceIndex: number;
  targetIndex: number;
  segments: Segment[];
  arrow: { position: Point; glyph: '>' | '<' | '^' | 'v' };
}
export interface GraphLayout {
  bounds: Rect;
  nodes: NodeLayout[];
  edges: EdgeRoute[];
  /** A component had an executable cycle the layering had to break. */
  hasCycleEdges: boolean;
}

export const right = (r: Rect): number => r.x + r.width - 1;
export const bottom = (r: Rect): number => r.y + r.height - 1;
export const centerX = (r: Rect): number => r.x + Math.trunc(r.width / 2);
export const centerY = (r: Rect): number => r.y + Math.trunc(r.height / 2);

function rectFromPoints(a: Point, b: Point): Rect {
  const x = Math.min(a.x, b.x);
  const y = Math.min(a.y, b.y);
  return { x, y, width: Math.max(a.x, b.x) - x + 1, height: Math.max(a.y, b.y) - y + 1 };
}

function expand(r: Rect, o: Rect): Rect {
  if (r.width <= 0 || r.height <= 0) return o;
  const minX = Math.min(r.x, o.x);
  const minY = Math.min(r.y, o.y);
  const maxX = Math.max(right(r), right(o));
  const maxY = Math.max(bottom(r), bottom(o));
  return { x: minX, y: minY, width: maxX - minX + 1, height: maxY - minY + 1 };
}

const includePoint = (r: Rect, p: Point): Rect => expand(r, { x: p.x, y: p.y, width: 1, height: 1 });

/** Box width for a title: the TUI clamps the label to 14..=28 columns plus the border. */
export function nodeOuterWidth(title: string): number {
  const len = Math.min(NODE_LABEL_MAX_WIDTH, Math.max(NODE_LABEL_MIN_WIDTH, [...title].length));
  return len + 2;
}

interface Resolved {
  sourceIndex: number;
  sourcePort: string | null;
  targetIndex: number;
  targetPort: string | null;
  label: string | null;
  kind: EdgeKind;
  declarationIndex: number;
}

interface VisualEdge { source: string; target: string; source_port?: string | null; target_port?: string | null; label?: string | null; kind?: string }

/** `metadata.graph_view.visual_edges` (loop arrows); unreadable metadata draws none, as in the TUI. */
export function visualEdges(workflow: LayoutWorkflow): VisualEdge[] {
  const view = workflow.metadata?.graph_view;
  if (!view || typeof view !== 'object') return [];
  const edges = (view as { visual_edges?: unknown }).visual_edges;
  if (!Array.isArray(edges)) return [];
  return edges.filter((e): e is VisualEdge => !!e && typeof e === 'object'
    && typeof (e as VisualEdge).source === 'string' && typeof (e as VisualEdge).target === 'string'
    && ((e as VisualEdge).kind === undefined || (e as VisualEdge).kind === 'loop_arrow'));
}

function components(count: number, edges: readonly Resolved[]): number[][] {
  const adj: number[][] = Array.from({ length: count }, () => []);
  for (const e of edges) { adj[e.sourceIndex]!.push(e.targetIndex); adj[e.targetIndex]!.push(e.sourceIndex); }
  for (const a of adj) { a.sort((x, y) => x - y); }
  const seen = new Array<boolean>(count).fill(false);
  const out: number[][] = [];
  for (let root = 0; root < count; root++) {
    if (seen[root]) continue;
    const queue = [root];
    const comp: number[] = [];
    seen[root] = true;
    while (queue.length > 0) {
      const n = queue.shift()!;
      comp.push(n);
      for (const m of adj[n]!) {
        if (seen[m]) continue;
        seen[m] = true;
        queue.push(m);
      }
    }
    comp.sort((a, b) => a - b);
    out.push(comp);
  }
  return out;
}

/** Kahn layering; when no node is free the lowest remaining one is taken (cycle broken). */
export function stableLayers(nodes: readonly number[], edges: ReadonlyArray<{ sourceIndex: number; targetIndex: number }>): { layers: number[][]; cycleBroken: boolean } {
  const incoming = new Map<number, number>(nodes.map((n) => [n, 0]));
  const outgoing = new Map<number, number[]>(nodes.map((n) => [n, []]));
  for (const e of edges) {
    incoming.set(e.targetIndex, (incoming.get(e.targetIndex) ?? 0) + 1);
    if (!outgoing.has(e.sourceIndex)) outgoing.set(e.sourceIndex, []);
    outgoing.get(e.sourceIndex)!.push(e.targetIndex);
  }
  for (const t of outgoing.values()) t.sort((a, b) => a - b);
  let remaining = [...nodes];
  const live = new Set(remaining);
  const layers: number[][] = [];
  let cycleBroken = false;
  while (remaining.length > 0) {
    let next = remaining.filter((n) => (incoming.get(n) ?? 0) === 0);
    if (next.length === 0) { next = [remaining[0]!]; cycleBroken = true; }
    next.sort((a, b) => a - b);
    for (const n of next) live.delete(n);
    for (const n of next) {
      for (const t of outgoing.get(n) ?? []) {
        if (live.has(t)) incoming.set(t, Math.max(0, (incoming.get(t) ?? 0) - 1));
      }
    }
    const taken = new Set(next);
    remaining = remaining.filter((n) => !taken.has(n));
    layers.push(next);
  }
  return { layers, cycleBroken };
}

function layoutNodes(workflow: LayoutWorkflow, layers: number[][], yOrigin: number): NodeLayout[] {
  const rowStep = NODE_BOX_HEIGHT + ROW_GAP;
  const rows = Math.max(1, ...layers.map((l) => l.length));
  const widths = layers.map((l) => Math.max(...l.map((i) => nodeOuterWidth(workflow.nodes[i]!.name)), nodeOuterWidth('')));
  const out: NodeLayout[] = [];
  let x = 0;
  layers.forEach((layer, li) => {
    const lw = widths[li]!;
    const slot = Math.trunc((rows - layer.length) / 2);
    layer.forEach((idx, row) => {
      const w = nodeOuterWidth(workflow.nodes[idx]!.name);
      out.push({
        nodeIndex: idx,
        rect: { x: x + Math.trunc((lw - w) / 2), y: yOrigin + (slot + row) * rowStep, width: w, height: NODE_BOX_HEIGHT },
        innerWidth: Math.max(1, w - 2),
      });
    });
    x += lw + COLUMN_GAP;
  });
  out.sort((a, b) => a.nodeIndex - b.nodeIndex);
  return out;
}

function sideSlot(span: number, portCount: number, portIndex: number | null): number {
  if (span <= 1 || portCount <= 1 || portIndex === null) return Math.trunc(Math.max(0, span - 1) / 2);
  const slots = Math.max(1, portCount - 1);
  return Math.trunc((portIndex * Math.max(0, span - 1) + Math.trunc(slots / 2)) / slots);
}

type Side = 'left' | 'right' | 'top' | 'bottom';

function anchor(workflow: LayoutWorkflow, n: NodeLayout, isSource: boolean, port: string | null, side: Side): Point {
  const node = workflow.nodes[n.nodeIndex]!;
  const ports = (isSource ? node.outputs : node.inputs) ?? [];
  const at = port === null ? -1 : ports.findIndex((p) => p.name === port);
  const idx = at < 0 ? null : at;
  const r = n.rect;
  switch (side) {
    case 'left': return { x: r.x, y: r.y + sideSlot(r.height, ports.length, idx) };
    case 'right': return { x: right(r), y: r.y + sideSlot(r.height, ports.length, idx) };
    case 'top': return { x: r.x + sideSlot(r.width, ports.length, idx), y: r.y };
    case 'bottom': return { x: r.x + sideSlot(r.width, ports.length, idx), y: bottom(r) };
  }
}

function segmentsOf(points: readonly Point[]): Segment[] {
  const out: Segment[] = [];
  for (let i = 0; i + 1 < points.length; i++) {
    const a = points[i]!;
    const b = points[i + 1]!;
    if (a.x !== b.x || a.y !== b.y) out.push({ start: a, end: b });
  }
  return out;
}

function arrowGlyph(s: Segment): '>' | '<' | '^' | 'v' {
  if (s.start.x === s.end.x) return s.end.y > s.start.y ? 'v' : '^';
  return s.end.x > s.start.x ? '>' : '<';
}

function route(workflow: LayoutWorkflow, e: Resolved, src: NodeLayout, dst: NodeLayout, lane: number): EdgeRoute {
  const scx = centerX(src.rect);
  const dcx = centerX(dst.rect);
  const orientation = dcx > scx ? 'forward' : dcx < scx ? 'backward' : 'vertical';
  const down = centerY(dst.rect) > centerY(src.rect);
  const srcSide: Side = orientation === 'vertical' ? (down ? 'bottom' : 'top') : 'right';
  const dstSide: Side = orientation === 'vertical' ? (down ? 'top' : 'bottom') : 'left';
  const a = anchor(workflow, src, true, e.sourcePort, srcSide);
  const b = anchor(workflow, dst, false, e.targetPort, dstSide);
  let points: Point[];
  if (orientation === 'forward') {
    const laneX = Math.min(a.x + 2 + lane * LANE_STEP, b.x - 1);
    points = [a, { x: laneX, y: a.y }, { x: laneX, y: b.y }, b];
  } else if (orientation === 'backward') {
    const outX = right(src.rect) + 2 + lane * LANE_STEP;
    const inX = dst.rect.x - 2 - lane * LANE_STEP;
    const laneY = Math.min(src.rect.y, dst.rect.y) - 2 - lane * LANE_STEP;
    points = [a, { x: outX, y: a.y }, { x: outX, y: laneY }, { x: inX, y: laneY }, { x: inX, y: b.y }, b];
  } else {
    const laneX = Math.max(right(src.rect), right(dst.rect)) + 2 + lane * LANE_STEP;
    const exitY = down ? a.y + 1 : a.y - 1;
    const entryY = down ? b.y - 1 : b.y + 1;
    points = [a, { x: a.x, y: exitY }, { x: laneX, y: exitY }, { x: laneX, y: entryY }, { x: b.x, y: entryY }, b];
  }
  const segments = segmentsOf(points);
  const last = segments[segments.length - 1] ?? { start: b, end: b };
  return {
    kind: e.kind,
    label: e.label,
    sourceIndex: e.sourceIndex,
    targetIndex: e.targetIndex,
    segments,
    arrow: { position: b, glyph: arrowGlyph(last) },
  };
}

function shift(nodes: NodeLayout[], edges: EdgeRoute[], dy: number): void {
  if (dy === 0) return;
  for (const n of nodes) n.rect = { ...n.rect, y: n.rect.y + dy };
  for (const e of edges) {
    e.segments = e.segments.map((s) => ({ start: { x: s.start.x, y: s.start.y + dy }, end: { x: s.end.x, y: s.end.y + dy } }));
    e.arrow = { ...e.arrow, position: { x: e.arrow.position.x, y: e.arrow.position.y + dy } };
  }
}

function boundsOf(nodes: readonly NodeLayout[], edges: readonly EdgeRoute[]): Rect {
  let b: Rect = nodes[0]?.rect ?? { x: 0, y: 0, width: 1, height: 1 };
  for (const n of nodes.slice(1)) b = expand(b, n.rect);
  for (const e of edges) {
    for (const s of e.segments) b = expand(b, rectFromPoints(s.start, s.end));
    b = includePoint(b, e.arrow.position);
  }
  return b;
}

/** The TUI's `layout_workflow_graph`: same inputs, same positions. */
export function layoutWorkflowGraph(workflow: LayoutWorkflow): GraphLayout {
  const count = workflow.nodes.length;
  if (count === 0) return { bounds: { x: 0, y: 0, width: 0, height: 0 }, nodes: [], edges: [], hasCycleEdges: false };
  const indexById = new Map<string, number>();
  workflow.nodes.forEach((n, i) => { if (!indexById.has(n.id)) indexById.set(n.id, i); });
  const exec: Resolved[] = [];
  workflow.edges.forEach((e, i) => {
    const s = indexById.get(e.source);
    const t = indexById.get(e.target);
    if (s === undefined || t === undefined) return;
    exec.push({ sourceIndex: s, sourcePort: e.source_port ?? null, targetIndex: t, targetPort: e.target_port ?? null, label: e.label ?? null, kind: 'executable', declarationIndex: i });
  });
  const visual: Resolved[] = [];
  visualEdges(workflow).forEach((e, i) => {
    const s = indexById.get(e.source);
    const t = indexById.get(e.target);
    if (s === undefined || t === undefined) return;
    visual.push({ sourceIndex: s, sourcePort: e.source_port ?? null, targetIndex: t, targetPort: e.target_port ?? null, label: e.label ?? null, kind: 'visual-loop', declarationIndex: workflow.edges.length + i });
  });

  const finalNodes: Array<NodeLayout | undefined> = new Array(count);
  const finalEdges: EdgeRoute[] = [];
  let overall: Rect | null = null;
  let minY = 0;
  let hasCycleEdges = false;
  for (const comp of components(count, [...exec, ...visual])) {
    const inComp = new Set(comp);
    const cExec = exec.filter((e) => inComp.has(e.sourceIndex) && inComp.has(e.targetIndex));
    const cVisual = visual.filter((e) => inComp.has(e.sourceIndex) && inComp.has(e.targetIndex));
    const layering = stableLayers(comp, cExec.length > 0 ? cExec : cVisual);
    if (layering.cycleBroken && cExec.length > 0) hasCycleEdges = true;
    const nodes = layoutNodes(workflow, layering.layers, minY);
    const byIndex = new Map(nodes.map((n) => [n.nodeIndex, n]));
    const lanes = new Map<string, number>();
    const edges = [...cExec, ...cVisual].sort((a, b) => a.declarationIndex - b.declarationIndex).map((e) => {
      const key = JSON.stringify([e.sourceIndex, e.sourcePort, e.targetIndex, e.targetPort]);
      const lane = lanes.get(key) ?? 0;
      lanes.set(key, lane + 1);
      return route(workflow, e, byIndex.get(e.sourceIndex)!, byIndex.get(e.targetIndex)!, lane);
    });
    let b = boundsOf(nodes, edges);
    if (b.y < minY) {
      const delta = minY - b.y;
      shift(nodes, edges, delta);
      b = { ...b, y: b.y + delta, height: Math.max(1, b.height) };
    }
    minY = bottom(b) + 1 + COMPONENT_GAP_Y;
    overall = overall ? expand(overall, b) : b;
    for (const n of nodes) finalNodes[n.nodeIndex] = n;
    finalEdges.push(...edges);
  }
  return {
    bounds: overall ?? { x: 0, y: 0, width: 0, height: 0 },
    nodes: Array.from({ length: count }, (_, i) => finalNodes[i] ?? { nodeIndex: i, rect: { x: 0, y: 0, width: 0, height: 0 }, innerWidth: NODE_LABEL_MIN_WIDTH }),
    edges: finalEdges,
    hasCycleEdges,
  };
}

/** Nearest node in a direction: least cross-axis offset, then least distance, then index (the TUI's h/j/k/l). */
export function directionalNeighbor(layout: GraphLayout, current: number, dir: Direction): number | null {
  const cur = layout.nodes[current];
  if (!cur) return null;
  const cx = centerX(cur.rect);
  const cy = centerY(cur.rect);
  let best: [number, number, number] | null = null;
  for (const c of layout.nodes) {
    if (c.nodeIndex === current) continue;
    const dx = centerX(c.rect) - cx;
    const dy = centerY(c.rect) - cy;
    let key: [number, number, number] | null = null;
    if (dir === 'left' && dx < 0) key = [Math.abs(dy), Math.abs(dx), c.nodeIndex];
    else if (dir === 'right' && dx > 0) key = [Math.abs(dy), Math.abs(dx), c.nodeIndex];
    else if (dir === 'up' && dy < 0) key = [Math.abs(dx), Math.abs(dy), c.nodeIndex];
    else if (dir === 'down' && dy > 0) key = [Math.abs(dx), Math.abs(dy), c.nodeIndex];
    if (key && (!best || key[0] < best[0] || (key[0] === best[0] && (key[1] < best[1] || (key[1] === best[1] && key[2] < best[2]))))) best = key;
  }
  return best ? best[2] : null;
}

function axisOrigin(min: number, span: number, view: number, desired: number): number {
  if (span <= view) return min - Math.trunc(Math.max(0, view - span) / 2);
  return Math.min(Math.max(desired, min), min + span - view);
}

/**
 * Top-left of the viewport (in layout units): centred on the selection when
 * following it, offset by the pan when manual, clamped to the graph bounds
 * (a graph smaller than the viewport is centred).
 */
export function viewportOrigin(layout: GraphLayout, width: number, height: number, selected: number, offset: Point | null): Point {
  if (layout.nodes.length === 0 || width <= 0 || height <= 0) return { x: 0, y: 0 };
  const sel = layout.nodes[selected] ?? layout.nodes[0]!;
  const b = layout.bounds;
  const follow = {
    x: axisOrigin(b.x, b.width, width, centerX(sel.rect) - Math.trunc(width / 2)),
    y: axisOrigin(b.y, b.height, height, centerY(sel.rect) - Math.trunc(height / 2)),
  };
  const desired = offset ? { x: follow.x + offset.x, y: follow.y + offset.y } : follow;
  return { x: axisOrigin(b.x, b.width, width, desired.x), y: axisOrigin(b.y, b.height, height, desired.y) };
}

/** True when two node boxes overlap (used by the tests on fixtures). */
export function rectsOverlap(a: Rect, b: Rect): boolean {
  return a.x <= right(b) && b.x <= right(a) && a.y <= bottom(b) && b.y <= bottom(a);
}
