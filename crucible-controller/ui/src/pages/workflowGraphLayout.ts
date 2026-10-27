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
  /// A runtime instance of a mapped task: drawn as a row under its deck, in the deck's layer.
  instance: { deck: string; index: number } | null;
}

export interface LaidOutEdge {
  from: string;
  to: string;
  /// Cubics through the gutter between each pair of layers the edge spans.
  path: string;
  /// How the consumer joins on the edge: `passed` or `settled`.
  label: string | null;
  /// The route answers the consumer runs on (`tier: high|low`). Data, so drawn in its own case.
  reads: string | null;
  /// What the edge carries: the fields the consumer narrows to each instance (`notes[item]`) and
  /// the files the producer stages into it (`ISSUES.md`, `TRIAGE.md +1`), dot-separated. Shown
  /// only while the edge is traced or hovered.
  plumbing: string | null;
  labelX: number;
  labelY: number;
  /// The consumer's flag: an edge into an advisory task is itself advisory.
  required: boolean;
  /// Both ends map over the same list, so each instance reads its counterpart, not the fold.
  aligned: boolean;
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

/// Every task is drawn in the same box: the card holds its name, what the task runs, what it maps
/// over and what it emits, and a uniform box is what keeps the layers readable. The foot of the
/// box is kept clear of text for what the task linked out to.
export const NODE_W = 156;
export const NODE_H = 78;
/// A plain command task in a run says only its name and how it ended, on one line, in a box as
/// wide as that line: the advance of a character of its name (12px mono) and of its trailing
/// note (10px mono), and what the dot and the padding take.
export const PILL_H = 26;
export const PILL_MIN_W = 72;
const PILL_CHAR = 7.3;
const PILL_NOTE_CHAR = 6.1;
const PILL_CHROME = 38;
export const LINK_STRIP = 14;
/// Gap between layers, along the direction the graph runs, and between the cards of one layer.
export const LAYER_GAP = 32;
/// Advance of one character of an edge label (10px mono), and the label's own padding.
const LABEL_CHAR = 6.1;
const LABEL_PAD = 12;
const ROW_GAP = 12;
const PAD = 10;
/// Offset of each card behind a mapped task, and how many are drawn.
export const STACK_STEP = 4;
const BEND_GUTTER = 14;
const BEND_GAP = 12;
export const STACK_CARDS = 2;
/// An instance row: its height, its width top down, how far it is indented under its deck left to
/// right, and the gap between rows.
export const INSTANCE_H = 24;
const INSTANCE_W = 136;
export const INSTANCE_INDENT = 12;
export const INSTANCE_GAP = 4;

/// The zoom a fitted graph is clamped to: below the floor its text is unreadable, so a long graph
/// is shown from its start and panned instead.
export const FIT_MIN_ZOOM = 0.7;
export const FIT_MAX_ZOOM = 1.35;
/// Room the fit leaves around the graph; the top clears the toolbar.
const FIT_PAD = { top: 40, right: 12, bottom: 12, left: 12 };
const FRAME_MIN_H = 200;
const FRAME_PANEL_H = 480;

export interface Viewport {
  x: number;
  y: number;
  zoom: number;
}

/// The viewport that fits a graph into a canvas: as large as fits, clamped to a legible zoom,
/// centred where it fits and anchored at its start where it does not.
export function fitViewport(
  graph: { width: number; height: number },
  canvas: { width: number; height: number }
): Viewport {
  const roomW = canvas.width - FIT_PAD.left - FIT_PAD.right;
  const roomH = canvas.height - FIT_PAD.top - FIT_PAD.bottom;
  const fits = Math.min(roomW / Math.max(1, graph.width), roomH / Math.max(1, graph.height));
  const zoom = Math.min(FIT_MAX_ZOOM, Math.max(FIT_MIN_ZOOM, fits));
  const offset = (start: number, room: number, size: number) =>
    size * zoom <= room ? start + (room - size * zoom) / 2 : start;
  return {
    x: offset(FIT_PAD.left, roomW, graph.width),
    y: offset(FIT_PAD.top, roomH, graph.height),
    zoom,
  };
}

/// The height of the in-page frame: the graph at the zoom the frame's width lets it fit at, so a
/// frame is neither taller than its graph needs nor too short to show it whole. An open task panel
/// shares the frame and needs room to read evidence in, however short the graph beside it.
export function frameHeight(
  graph: { width: number; height: number },
  width: number,
  panel: boolean
): number {
  const zoom = Math.min(
    FIT_MAX_ZOOM,
    Math.max(FIT_MIN_ZOOM, (width - FIT_PAD.left - FIT_PAD.right) / Math.max(1, graph.width))
  );
  const fitted = Math.ceil(graph.height * zoom + FIT_PAD.top + FIT_PAD.bottom);
  return Math.max(panel ? FRAME_PANEL_H : FRAME_MIN_H, fitted);
}

/// The mapped task an instance came from. The executor names an instance `node[item]` and a
/// declared name may not hold a bracket, so the prefix is the node it was mapped from.
export function mappedFrom(name: string): string | null {
  const cut = name.indexOf('[');
  if (cut <= 0 || !name.endsWith(']')) return null;
  return name.slice(0, cut);
}

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

/// The point on a cubic whose `along` is the one asked for: along each segment the layout draws,
/// `along` only grows, so halving the interval converges on it.
function atAlong(
  along: number,
  p0: [number, number],
  p1: [number, number],
  p2: [number, number],
  p3: [number, number]
): [number, number] {
  let [lo, hi] = [0, 1];
  for (let step = 0; step < 24; step += 1) {
    const mid = (lo + hi) / 2;
    if (onCubic(mid, p0, p1, p2, p3)[0] < along) lo = mid;
    else hi = mid;
  }
  return onCubic((lo + hi) / 2, p0, p1, p2, p3);
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

function median(xs: number[]): number | null {
  if (xs.length === 0) return null;
  const sorted = [...xs].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 1
    ? (sorted[mid] ?? 0)
    : ((sorted[mid - 1] ?? 0) + (sorted[mid] ?? 0)) / 2;
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

/// The route answers an edge's consumer runs on, as its label reads them.
export function edgeWhen(edge: WorkflowGraphEdge): string | null {
  const when = edge.when ?? null;
  return when === null ? null : `${when.question}: ${when.labels.join('|')}`;
}

/// Plain command tasks drawn as pills, and the characters each one trails its name with (its
/// figures, its link), by task name.
export interface PillOptions {
  trailing: ReadonlyMap<string, number>;
}

/// Whether a node is drawn as a one-line pill: a plain command task, once the graph is a run's.
export function isPill(node: WorkflowGraphNode, pills: PillOptions | null): boolean {
  return pills !== null && node.kind === 'command' && (node.fanout ?? null) === null;
}

/// How wide a pill is: its name and whatever trails it, within a floor and a card's width.
export function pillWidth(name: string, trailing: number): number {
  const text = name.length * PILL_CHAR + (trailing > 0 ? trailing * PILL_NOTE_CHAR + 6 : 0);
  return Math.min(NODE_W, Math.max(PILL_MIN_W, Math.ceil(text + PILL_CHROME)));
}

/// The gutter an edge's label needs. Left to right, a label sits across the gutter its last
/// segment is drawn in, so that gutter widens to hold its `when` label; top down it lies along it.
/// What the edge carries is shown only on demand and is drawn over whatever it lands on.
function labelGap(edge: WorkflowGraphEdge, direction: LayoutDirection): number {
  if (direction === 'TD') return LAYER_GAP;
  const reads = edgeWhen(edge);
  if (reads === null) return LAYER_GAP;
  const chars = reads.length + (edge.join === 'all' ? 0 : 8);
  // The label is centred past the middle of the gutter, so the gap is a little over its width.
  return Math.max(LAYER_GAP, Math.ceil(chars * LABEL_CHAR * 1.15) + LABEL_PAD);
}

/// Lay the compiled graph out in dependency layers, left to right or top down, in the flow's own
/// coordinates: every task gets the same box, and a layer is centred against the widest one. A
/// mapped task's runtime instances are rows under its card, in its layer, so a fan-out does not
/// cost the graph a layer; the edge from a deck to its own instance is the row's placement and is
/// not drawn. An edge that spans more than one layer is broken at a bend in each layer it crosses,
/// so it routes through the gutters instead of running under the tasks in between. Each gutter is
/// as wide as the labels drawn in it. The layout is worked out along and across the layers and
/// only turned into x and y at the end, so the two directions are the same picture transposed.
export function layoutWorkflow(
  graph: WorkflowGraphDoc,
  direction: LayoutDirection = 'LR',
  { pills = null }: { pills?: PillOptions | null } = {}
): WorkflowLayout {
  const known = new Set(graph.nodes.map((n) => n.name));
  const edges = graph.edges.filter((e) => known.has(e.from) && known.has(e.to));
  const deckOf = (name: string): string | null => {
    const deck = mappedFrom(name);
    return deck !== null && known.has(deck) ? deck : null;
  };
  const headOf = (name: string): string => deckOf(name) ?? name;

  const heads = graph.nodes.filter((n) => deckOf(n.name) === null);
  const members = new Map<string, WorkflowGraphNode[]>();
  for (const node of graph.nodes) {
    const deck = deckOf(node.name);
    if (deck !== null) members.set(deck, [...(members.get(deck) ?? []), node]);
  }
  const drawn = edges.filter((e) => headOf(e.from) !== headOf(e.to));

  const parents = new Map<string, string[]>();
  for (const edge of drawn) {
    const [from, to] = [headOf(edge.from), headOf(edge.to)];
    parents.set(to, [...(parents.get(to) ?? []), from]);
  }
  const layerIndex = layerOf(heads, parents);

  // Cells are decks and plain tasks plus the bends long edges are broken at; they are what the
  // layers hold and what the ordering sees.
  const prefix = bendPrefix(known);
  const cellLayer = new Map<string, number>(layerIndex);
  const chains: string[][] = [];
  const segments: WorkflowGraphEdge[] = [];
  drawn.forEach((edge, i) => {
    const [head, tail] = [headOf(edge.from), headOf(edge.to)];
    const from = layerIndex.get(head) ?? 0;
    const to = layerIndex.get(tail) ?? 0;
    const chain = [head];
    for (let layer = from + 1; layer < to; layer += 1) {
      const bend = `${prefix}${i}:${layer}`;
      cellLayer.set(bend, layer);
      chain.push(bend);
    }
    chain.push(tail);
    chains.push(chain);
    for (let step = 0; step + 1 < chain.length; step += 1) {
      const a = chain[step];
      const b = chain[step + 1];
      if (a === undefined || b === undefined) continue;
      segments.push({ ...edge, from: a, to: b });
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
    ...heads.map((n) => n.name),
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
  const instanceAlong = direction === 'LR' ? NODE_W - INSTANCE_INDENT : INSTANCE_H;
  const instanceAcross = direction === 'LR' ? INSTANCE_H : INSTANCE_W;
  // Left to right the rows hang under the card, indented; top down they sit beside it, centred
  // on it, so no edge into or out of a row runs through the card or another row.
  const instanceInset = direction === 'LR' ? INSTANCE_INDENT : (NODE_H - INSTANCE_H) / 2;
  const stack = STACK_CARDS * STACK_STEP;

  const byName = new Map(graph.nodes.map((n) => [n.name, n]));
  const sizeOf = (name: string): Along => {
    const node = byName.get(name);
    if (node === undefined || pills === null || !isPill(node, pills)) {
      return [alongSize, acrossSize];
    }
    const width = pillWidth(name, pills.trailing.get(name) ?? 0);
    return direction === 'LR' ? [width, PILL_H] : [PILL_H, width];
  };
  const stackOf = (name: string): number => ((byName.get(name)?.fanout ?? null) === null ? 0 : stack);
  const extentOf = (head: string): number =>
    sizeOf(head)[1] +
    stackOf(head) +
    (members.get(head)?.length ?? 0) * (instanceAcross + INSTANCE_GAP);
  const memberOffset = (deck: string, index: number): number =>
    sizeOf(deck)[1] + stackOf(deck) + INSTANCE_GAP + index * (instanceAcross + INSTANCE_GAP);
  const middleOffset = (name: string): number => {
    const deck = deckOf(name);
    if (deck === null) return sizeOf(name)[1] / 2;
    const index = (members.get(deck) ?? []).findIndex((m) => m.name === name);
    return memberOffset(deck, Math.max(0, index)) + instanceAcross / 2;
  };

  // Each card sits level with the median of what feeds it straight from the layer before; the
  // layer is packed in order, then shifted by its mean displacement so it stays balanced on its
  // parents. A deck lines up with the deck it reads per element, and row with row.
  const topOf = new Map<string, number>();
  let previous: { top: number; bottom: number } | null = null;
  layers.forEach((layer, layerNo) => {
    const cards = layer.filter((c) => byName.has(c));
    const wanted = cards.map((card) =>
      median(
        drawn
          .filter((e) => headOf(e.to) === card && layerIndex.get(headOf(e.from)) === layerNo - 1)
          .map((e) => (topOf.get(headOf(e.from)) ?? 0) + middleOffset(e.from) - middleOffset(e.to))
      )
    );
    let cursor: number | null = null;
    const tops = cards.map((card, i) => {
      const want = wanted[i] ?? null;
      const top: number = cursor === null ? (want ?? 0) : Math.max(want ?? cursor, cursor);
      cursor = top + extentOf(card) + ROW_GAP;
      return top;
    });
    const pulled = tops.flatMap((top, i) => {
      const want = wanted[i] ?? null;
      return want === null ? [] : [top - want];
    });
    const span = cards.reduce((total, c) => total + extentOf(c), 0) + ROW_GAP * (cards.length - 1);
    const shift =
      pulled.length > 0
        ? pulled.reduce((a, b) => a + b, 0) / pulled.length
        : previous === null
          ? 0
          : -((previous.top + previous.bottom - span) / 2);
    cards.forEach((card, i) => topOf.set(card, Math.round((tops[i] ?? 0) - shift)));
    const placedTops = cards.map((card) => topOf.get(card) ?? 0);
    previous = {
      top: Math.min(...placedTops),
      bottom: Math.max(...cards.map((card) => (topOf.get(card) ?? 0) + extentOf(card))),
    };
  });
  const lowest = Math.min(0, ...topOf.values());
  for (const [card, top] of topOf) topOf.set(card, top - lowest + PAD);
  const bottomOf = (layer: string[]): number =>
    Math.max(
      PAD,
      ...layer.filter((c) => byName.has(c)).map((c) => (topOf.get(c) ?? PAD) + extentOf(c))
    );

  // Bends live in a lane past the cards of their own layer, so a layer-skipping edge routes
  // around the occupied rows instead of threading between them and reading as a root-to-leaf
  // link, without detouring past cards in layers it never crosses.
  const laneAt = (layer: string[]): number => bottomOf(layer) + BEND_GUTTER;
  const acrossExtent =
    layers.reduce((most, layer) => {
      const bendCount = layer.filter((c) => !byName.has(c)).length;
      const reach = bendCount === 0 ? bottomOf(layer) : laneAt(layer) + (bendCount - 1) * BEND_GAP;
      return Math.max(most, reach);
    }, PAD + acrossSize + stack) + PAD;

  const gaps = layers.slice(1).map(() => LAYER_GAP);
  for (const edge of drawn) {
    const gap = (layerIndex.get(headOf(edge.to)) ?? 0) - 1;
    if (gap < 0 || gap >= gaps.length) continue;
    gaps[gap] = Math.max(gaps[gap] ?? LAYER_GAP, labelGap(edge, direction));
  }
  // A layer is as deep as its deepest card; a shallower one is centred in it.
  const depths = layers.map((layer) =>
    Math.max(PILL_H, ...layer.filter((c) => byName.has(c)).map((c) => sizeOf(c)[0]))
  );
  const alongAt: number[] = [];
  layers.forEach((_, layerNo) => {
    const before = alongAt[layerNo - 1];
    const depth = depths[layerNo - 1] ?? alongSize;
    alongAt.push(before === undefined ? PAD : before + depth + (gaps[layerNo - 1] ?? LAYER_GAP));
  });
  const alongExtent =
    (alongAt[alongAt.length - 1] ?? PAD) + (depths[depths.length - 1] ?? alongSize) + PAD;

  const place = (
    node: WorkflowGraphNode,
    layer: number,
    order: number,
    at: Along,
    size: Along,
    instance: LaidOutNode['instance']
  ): LaidOutNode => {
    const [x, y] = toXY(direction, at);
    const [width, height] = toXY(direction, size);
    return {
      node,
      layer,
      order,
      x,
      y,
      width,
      height,
      isResult: graph.result === node.name,
      instance,
    };
  };

  const placed = new Map<string, LaidOutNode>();
  const bends = new Map<string, Along>();
  const nodes: LaidOutNode[] = [];
  layers.forEach((layer, layerNo) => {
    const cards = layer.filter((c) => byName.has(c));
    const layerBends = layer.filter((c) => !byName.has(c));
    const start = alongAt[layerNo] ?? PAD;
    const depth = depths[layerNo] ?? alongSize;
    layerBends.forEach((name, k) => {
      bends.set(name, [start + depth / 2, laneAt(layer) + k * BEND_GAP]);
    });
    cards.forEach((name, order) => {
      const node = byName.get(name);
      if (node === undefined) return;
      const across = topOf.get(name) ?? PAD;
      const size = sizeOf(name);
      const along = start + (depth - size[0]) / 2;
      const card = place(node, layerNo, order, [along, across], size, null);
      placed.set(name, card);
      nodes.push(card);
      (members.get(name) ?? []).forEach((member, index) => {
        const row = place(
          member,
          layerNo,
          order,
          [along + instanceInset, across + memberOffset(name, index)],
          [instanceAlong, instanceAcross],
          { deck: name, index }
        );
        placed.set(member.name, row);
        nodes.push(row);
      });
    });
  });

  // Where an edge leaves a box and where it lands: the middle of the side facing the next layer.
  const port = (laid: LaidOutNode, leaving: boolean): Along => {
    const [along, across] = direction === 'LR' ? [laid.x, laid.y] : [laid.y, laid.x];
    const [length, breadth] =
      direction === 'LR' ? [laid.width, laid.height] : [laid.height, laid.width];
    return [leaving ? along + length : along, across + breadth / 2];
  };

  const laidEdges: LaidOutEdge[] = [];
  drawn.forEach((edge, i) => {
    const from = placed.get(edge.from);
    const to = placed.get(edge.to);
    if (from === undefined || to === undefined) return;
    const chain = chains[i] ?? [];
    const points: Along[] = [port(from, true)];
    for (const cell of chain.slice(1, -1)) {
      const bend = bends.get(cell);
      if (bend !== undefined) points.push(bend);
    }
    points.push(port(to, false));

    const start = toXY(direction, points[0] ?? [0, 0]);
    let path = `M ${start[0]} ${start[1]}`;
    let label: Along = [0, 0];
    const gapBefore = gaps[(layerIndex.get(headOf(edge.to)) ?? 1) - 1] ?? LAYER_GAP;
    for (let p = 0; p + 1 < points.length; p += 1) {
      const a = points[p];
      const b = points[p + 1];
      if (a === undefined || b === undefined) continue;
      const bend = Math.max(24, (b[0] - a[0]) / 2);
      const c1: Along = [a[0] + bend, a[1]];
      const c2: Along = [b[0] - bend, b[1]];
      const [c1x, c1y] = toXY(direction, c1);
      const [c2x, c2y] = toXY(direction, c2);
      const [bx, by] = toXY(direction, b);
      path += ` C ${c1x} ${c1y}, ${c2x} ${c2y}, ${bx} ${by}`;
      // The middle of the last gutter: a join label belongs next to the task that waits, not out
      // in the graph or over the layer a bend runs through.
      label = atAlong(b[0] - gapBefore / 2, a, c1, c2, b);
    }
    const files = byName.get(edge.from)?.emits_files ?? [];
    const fileLabel =
      files.length === 0
        ? null
        : files.length === 1
          ? files[0]
          : `${files[0]} +${files.length - 1}`;
    const plumbing = [
      ...edge.keyed.map((field) => `${field}[item]`),
      ...(fileLabel === undefined || fileLabel === null ? [] : [fileLabel]),
    ];
    const [labelX, labelY] = toXY(direction, [label[0], label[1] - (direction === 'LR' ? 7 : 0)]);
    laidEdges.push({
      from: edge.from,
      to: edge.to,
      path,
      label: edge.join === 'passed' || edge.join === 'settled' ? edge.join : null,
      reads: edgeWhen(edge),
      plumbing: plumbing.length === 0 ? null : plumbing.join(' · '),
      labelX,
      labelY,
      required: edge.required,
      aligned: edge.aligned,
    });
  });

  // Tracing an instance lights its deck: the row is the deck dealt out.
  const lineage = new Map<string, string[]>();
  for (const edge of edges) lineage.set(edge.to, [...(lineage.get(edge.to) ?? []), edge.from]);
  for (const [deck, rows] of members) {
    for (const row of rows) lineage.set(row.name, [...(lineage.get(row.name) ?? []), deck]);
  }

  const [width, height] = toXY(direction, [alongExtent, acrossExtent]);
  return {
    nodes,
    edges: laidEdges,
    width,
    height,
    ancestry: ancestryOf(graph.nodes, lineage),
    crossings,
  };
}
