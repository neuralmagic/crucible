// Layered layout for a compiled plan's graph (`WorkflowGraphDto`). Pure functions so the DAG maths
// is testable without an SVG — same split as taskGraph.ts.

import type { components } from '../api/schema';

export type WorkflowGraphDoc = components['schemas']['WorkflowGraphDto'];
export type WorkflowGraphNode = components['schemas']['GraphNodeDto'];
export type WorkflowGraphEdge = components['schemas']['GraphEdgeDto'];

/// Which way the layers run: left to right, or top down.
export type LayoutDirection = 'LR' | 'TD';

export interface LaidOutNode {
  node: WorkflowGraphNode;
  /// Longest path from a root: the column left to right, or the row top down.
  layer: number;
  /// Position within the layer.
  order: number;
  /// Top-left corner, in the flow's own coordinates.
  x: number;
  y: number;
  width: number;
  height: number;
  isResult: boolean;
}

export interface LaidOutEdge {
  from: string;
  to: string;
  /// Cubics through the gutter between each pair of layers the edge spans.
  path: string;
  /// What rides the edge: `passed` when the consumer joins on it, and the files the
  /// producer stages into the consumer (`ISSUES.md`, `TRIAGE.md +1`), dot-separated.
  label: string | null;
  labelX: number;
  labelY: number;
  /// The consumer's flag: an edge into an advisory task is itself advisory.
  required: boolean;
}

export interface WorkflowLayout {
  nodes: LaidOutNode[];
  edges: LaidOutEdge[];
  width: number;
  height: number;
  /// Every task each task transitively depends on, including itself.
  ancestry: ReadonlyMap<string, ReadonlySet<string>>;
  /// Crossings left after ordering; drives nothing visual, kept because the ordering picks the
  /// arrangement that minimizes it.
  crossings: number;
}

/// Every task is drawn in the same box: the card holds a badge, the name, what the task runs, what
/// it maps over and what it emits, and a uniform box is what keeps the layers readable.
export const NODE_W = 176;
export const NODE_H = 72;
/// Gap between layers, along the direction the graph runs, and between the cards of one layer.
const LAYER_GAP = 44;
const ROW_GAP = 12;
const PAD = 10;
/// Offset of each card behind a mapped task, and how many are drawn.
export const STACK_STEP = 4;
const BEND_GUTTER = 14;
const BEND_GAP = 12;
export const STACK_CARDS = 2;

/// A point in layout space: how far along the layers, and how far across them.
type Along = [along: number, across: number];

function toXY(direction: LayoutDirection, [along, across]: Along): [number, number] {
  return direction === 'LR' ? [along, across] : [across, along];
}

/// A point on a cubic, used to sit an edge label on the curve rather than on the straight line
/// between its ends.
function onCubic(
  t: number,
  p0: [number, number],
  p1: [number, number],
  p2: [number, number],
  p3: [number, number]
): [number, number] {
  const u = 1 - t;
  const w = [u * u * u, 3 * u * u * t, 3 * u * t * t, t * t * t];
  const at = (i: 0 | 1): number =>
    (w[0] ?? 0) * p0[i] + (w[1] ?? 0) * p1[i] + (w[2] ?? 0) * p2[i] + (w[3] ?? 0) * p3[i];
  return [at(0), at(1)];
}

/// Longest path from a root. A dependency on a name the graph does not carry is ignored, and a
/// cycle (which the engine rejects, but the wire can still carry) stops at the repeated node
/// rather than recursing forever.
function layerOf(
  nodes: WorkflowGraphNode[],
  parents: ReadonlyMap<string, string[]>
): Map<string, number> {
  const done = new Map<string, number>();
  const walk = (name: string, seen: Set<string>): number => {
    const cached = done.get(name);
    if (cached !== undefined) return cached;
    if (seen.has(name)) return 0;
    seen.add(name);
    let layer = 0;
    for (const parent of parents.get(name) ?? []) layer = Math.max(layer, walk(parent, seen) + 1);
    seen.delete(name);
    done.set(name, layer);
    return layer;
  };
  for (const node of nodes) walk(node.name, new Set());
  return done;
}

function positions(layers: string[][]): Map<string, number> {
  const at = new Map<string, number>();
  layers.forEach((layer) => layer.forEach((name, i) => at.set(name, i)));
  return at;
}

/// Edges that cross, counted pairwise per gutter. Two edges between the same pair of layers cross
/// when their endpoints are ordered oppositely.
export function countCrossings(layers: string[][], edges: readonly WorkflowGraphEdge[]): number {
  const at = positions(layers);
  const layerOfName = new Map<string, number>();
  layers.forEach((layer, i) => layer.forEach((name) => layerOfName.set(name, i)));

  const spans = edges
    .map((e) => ({
      gutter: layerOfName.get(e.from) ?? -1,
      from: at.get(e.from) ?? 0,
      to: at.get(e.to) ?? 0,
    }))
    .filter((s) => s.gutter >= 0);

  let crossings = 0;
  for (let i = 0; i < spans.length; i += 1) {
    for (let j = i + 1; j < spans.length; j += 1) {
      const a = spans[i];
      const b = spans[j];
      if (a === undefined || b === undefined || a.gutter !== b.gutter) continue;
      if ((a.from - b.from) * (a.to - b.to) < 0) crossings += 1;
    }
  }
  return crossings;
}

/// Order each layer by the mean position of its neighbours in the previous one, ties broken by the
/// order the layer already had. Sweeping down then up and keeping the best arrangement is the
/// standard barycentre heuristic; keeping the *first* best makes the result a function of the
/// input alone.
function orderLayers(
  layers: string[][],
  parents: ReadonlyMap<string, string[]>,
  children: ReadonlyMap<string, string[]>,
  edges: readonly WorkflowGraphEdge[]
): { layers: string[][]; crossings: number } {
  let current = layers.map((layer) => [...layer]);
  let best = current.map((layer) => [...layer]);
  let bestCrossings = countCrossings(best, edges);

  const sweep = (down: boolean) => {
    const at = positions(current);
    const indices = down ? current.map((_, i) => i) : current.map((_, i) => current.length - 1 - i);
    for (const i of indices) {
      const neighbours = down ? parents : children;
      const layer = current[i];
      if (layer === undefined) continue;
      const ranked = layer.map((name, index) => {
        const near = (neighbours.get(name) ?? [])
          .map((other) => at.get(other))
          .filter((p): p is number => p !== undefined);
        const want = near.length === 0 ? index : near.reduce((s, p) => s + p, 0) / near.length;
        return { name, index, want };
      });
      ranked.sort((a, b) => a.want - b.want || a.index - b.index);
      const ordered = ranked.map((r) => r.name);
      current[i] = ordered;
      ordered.forEach((name, index) => at.set(name, index));
    }
  };

  for (let pass = 0; pass < 4; pass += 1) {
    sweep(pass % 2 === 0);
    const crossings = countCrossings(current, edges);
    if (crossings < bestCrossings) {
      bestCrossings = crossings;
      best = current.map((layer) => [...layer]);
    }
    current = current.map((layer) => [...layer]);
  }
  return { layers: best, crossings: bestCrossings };
}

function ancestryOf(
  nodes: WorkflowGraphNode[],
  parents: ReadonlyMap<string, string[]>
): Map<string, Set<string>> {
  const done = new Map<string, Set<string>>();
  const walk = (name: string, seen: Set<string>): Set<string> => {
    const cached = done.get(name);
    if (cached !== undefined) return cached;
    if (seen.has(name)) return new Set([name]);
    seen.add(name);
    const all = new Set<string>([name]);
    for (const parent of parents.get(name) ?? []) {
      for (const up of walk(parent, seen)) all.add(up);
    }
    seen.delete(name);
    done.set(name, all);
    return all;
  };
  for (const node of nodes) walk(node.name, new Set());
  return done;
}

/// A key for a bend, distinct from every task name in the graph: a pack names its own tasks, so
/// the prefix is grown until nothing can collide with it.
function bendPrefix(names: Iterable<string>): string {
  let prefix = '~bend~';
  const taken = [...names];
  while (taken.some((name) => name.startsWith(prefix))) prefix = `~${prefix}`;
  return prefix;
}

/// Lay the compiled graph out in dependency layers, left to right or top down, in the flow's own
/// coordinates: every task gets the same box, and a layer is centred against the widest one. An
/// edge that spans more than one layer is broken at a bend in each layer it crosses, so it routes
/// through the gutters instead of running under the tasks in between. The layout is worked out
/// along and across the layers and only turned into x and y at the end, so the two directions are
/// the same picture transposed.
export function layoutWorkflow(
  graph: WorkflowGraphDoc,
  direction: LayoutDirection = 'LR'
): WorkflowLayout {
  const known = new Set(graph.nodes.map((n) => n.name));
  const edges = graph.edges.filter((e) => known.has(e.from) && known.has(e.to));

  const parents = new Map<string, string[]>();
  for (const edge of edges) {
    parents.set(edge.to, [...(parents.get(edge.to) ?? []), edge.from]);
  }
  const layerIndex = layerOf(graph.nodes, parents);

  // Cells are tasks plus the bends long edges are broken at; they are what the layers hold and
  // what the ordering sees.
  const prefix = bendPrefix(known);
  const cellLayer = new Map<string, number>(layerIndex);
  const chains = new Map<string, string[]>();
  const segments: WorkflowGraphEdge[] = [];
  edges.forEach((edge, i) => {
    const from = layerIndex.get(edge.from) ?? 0;
    const to = layerIndex.get(edge.to) ?? 0;
    const chain = [edge.from];
    for (let layer = from + 1; layer < to; layer += 1) {
      const bend = `${prefix}${i}:${layer}`;
      cellLayer.set(bend, layer);
      chain.push(bend);
    }
    chain.push(edge.to);
    chains.set(`${edge.from}\u0000${edge.to}`, chain);
    for (let step = 0; step + 1 < chain.length; step += 1) {
      const a = chain[step];
      const b = chain[step + 1];
      if (a === undefined || b === undefined) continue;
      segments.push({ from: a, to: b, join: edge.join, required: edge.required });
    }
  });

  const cellParents = new Map<string, string[]>();
  const cellChildren = new Map<string, string[]>();
  for (const segment of segments) {
    cellParents.set(segment.to, [...(cellParents.get(segment.to) ?? []), segment.from]);
    cellChildren.set(segment.from, [...(cellChildren.get(segment.from) ?? []), segment.to]);
  }

  const grouped: string[][] = [];
  const cells = [
    ...graph.nodes.map((n) => n.name),
    ...[...cellLayer.keys()].filter((k) => k.startsWith(prefix)),
  ];
  for (const cell of cells) {
    const layer = cellLayer.get(cell) ?? 0;
    while (grouped.length <= layer) grouped.push([]);
    grouped[layer]?.push(cell);
  }
  const { layers, crossings } = orderLayers(grouped, cellParents, cellChildren, segments);

  const alongSize = direction === 'LR' ? NODE_W : NODE_H;
  const acrossSize = direction === 'LR' ? NODE_H : NODE_W;
  const stack = STACK_CARDS * STACK_STEP;
  const layerPitch = alongSize + LAYER_GAP;
  const rowPitch = acrossSize + stack + ROW_GAP;
  const byName = new Map(graph.nodes.map((n) => [n.name, n]));
  const widest = layers.reduce(
    (most, layer) => Math.max(most, layer.filter((c) => byName.has(c)).length),
    0
  );
  // Bends live in a gutter lane past every card, so a layer-skipping edge routes around the
  // occupied rows instead of threading between them and reading as a root-to-leaf link.
  const mostBends = layers.reduce(
    (most, layer) => Math.max(most, layer.filter((c) => !byName.has(c)).length),
    0
  );
  const cardRegion = Math.max(rowPitch, widest * rowPitch - ROW_GAP);
  const gutter = mostBends > 0 ? BEND_GUTTER + mostBends * BEND_GAP : 0;
  const acrossExtent = cardRegion + gutter + PAD * 2;
  const alongExtent = Math.max(1, layers.length) * layerPitch - LAYER_GAP + PAD * 2;

  const placed = new Map<string, LaidOutNode>();
  const bends = new Map<string, Along>();
  const nodes: LaidOutNode[] = [];
  layers.forEach((layer, layerNo) => {
    const cards = layer.filter((c) => byName.has(c));
    const layerBends = layer.filter((c) => !byName.has(c));
    const span = cards.length * rowPitch - ROW_GAP;
    const first = PAD + (cardRegion - span) / 2;
    const along = PAD + layerNo * layerPitch;
    layerBends.forEach((name, k) => {
      bends.set(name, [along + alongSize / 2, PAD + cardRegion + BEND_GUTTER + k * BEND_GAP]);
    });
    cards.forEach((name, order) => {
      const node = byName.get(name);
      if (node === undefined) {
        return;
      }
      const [x, y] = toXY(direction, [along, first + order * rowPitch]);
      const laid: LaidOutNode = {
        node,
        layer: layerNo,
        order,
        x,
        y,
        width: NODE_W,
        height: NODE_H,
        isResult: graph.result === node.name,
      };
      placed.set(name, laid);
      nodes.push(laid);
    });
  });

  // Where an edge leaves a card and where it lands: the middle of the side facing the next layer.
  const port = (laid: LaidOutNode, leaving: boolean): Along => {
    const along = direction === 'LR' ? laid.x : laid.y;
    const across = direction === 'LR' ? laid.y : laid.x;
    return [leaving ? along + alongSize : along, across + acrossSize / 2];
  };

  const laidEdges: LaidOutEdge[] = [];
  for (const edge of edges) {
    const from = placed.get(edge.from);
    const to = placed.get(edge.to);
    if (from === undefined || to === undefined) continue;
    const chain = chains.get(`${edge.from}\u0000${edge.to}`) ?? [edge.from, edge.to];
    const points: Along[] = [port(from, true)];
    for (const cell of chain.slice(1, -1)) {
      const bend = bends.get(cell);
      if (bend !== undefined) points.push(bend);
    }
    points.push(port(to, false));

    const start = toXY(direction, points[0] ?? [0, 0]);
    let path = `M ${start[0]} ${start[1]}`;
    let label: Along = [0, 0];
    for (let i = 0; i + 1 < points.length; i += 1) {
      const a = points[i];
      const b = points[i + 1];
      if (a === undefined || b === undefined) continue;
      const bend = Math.max(24, (b[0] - a[0]) / 2);
      const c1: Along = [a[0] + bend, a[1]];
      const c2: Along = [b[0] - bend, b[1]];
      const [c1x, c1y] = toXY(direction, c1);
      const [c2x, c2y] = toXY(direction, c2);
      const [bx, by] = toXY(direction, b);
      path += ` C ${c1x} ${c1y}, ${c2x} ${c2y}, ${bx} ${by}`;
      // The last gutter: a join label belongs next to the task that waits, not out in the graph.
      label = onCubic(0.55, a, c1, c2, b);
    }
    const files = byName.get(edge.from)?.emits_files ?? [];
    const fileLabel =
      files.length === 0
        ? null
        : files.length === 1
          ? files[0]
          : `${files[0]} +${files.length - 1}`;
    const joinLabel = edge.join === 'passed' || edge.join === 'settled' ? edge.join : null;
    const parts = [joinLabel, fileLabel].filter(
      (part): part is string => part !== null
    );
    const [labelX, labelY] = toXY(direction, [label[0], label[1] - (direction === 'LR' ? 7 : 0)]);
    laidEdges.push({
      from: edge.from,
      to: edge.to,
      path,
      label: parts.length === 0 ? null : parts.join(' · '),
      labelX,
      labelY,
      required: edge.required,
    });
  }

  const [width, height] = toXY(direction, [alongExtent, acrossExtent]);
  return {
    nodes,
    edges: laidEdges,
    width,
    height,
    ancestry: ancestryOf(graph.nodes, parents),
    crossings,
  };
}
