import { describe, expect, it } from 'vitest';
import {
  countCrossings,
  edgeWhen,
  FIT_MAX_ZOOM,
  FIT_MIN_ZOOM,
  fitViewport,
  frameHeights,
  INSTANCE_H,
  INSTANCE_INDENT,
  isPill,
  LAYER_GAP,
  layoutWorkflow,
  mappedFrom,
  NODE_H,
  NODE_W,
  PILL_H,
  PILL_MIN_W,
  pillWidth,
  type WorkflowGraphDoc,
  type WorkflowGraphNode,
} from './workflowGraphLayout';

function task(name: string, overrides: Partial<WorkflowGraphNode> = {}): WorkflowGraphNode {
  return {
    name,
    kind: 'command',
    required: true,
    needs: 'any',
    join: 'all',
    isolation: null,
    emits: [],
    emits_files: [],
    fanout: null,
    session: null,
    harness: null,
    model: null,
    effort: null,
    prompt: null,
    command: null,
    keyed: [],
    when: null,
    questions: [],
    ...overrides,
  };
}

function edge(
  from: string,
  to: string,
  overrides: Partial<WorkflowGraphDoc['edges'][number]> = {}
): WorkflowGraphDoc['edges'][number] {
  return { from, to, join: 'all', required: true, when: null, aligned: false, keyed: [], ...overrides };
}

/// seed → fan → work → check → final, with work mapped over fan.idea and check advisory.
const PACK: WorkflowGraphDoc = {
  workflow_type: 'playbook',
  result: 'final',
  nodes: [
    task('seed'),
    task('fan', { kind: 'agent', emits: ['idea'], isolation: 'worktree' }),
    task('work', { fanout: { over_task: 'fan', over_field: 'idea', max_fanout: 4 } }),
    task('check', { required: false, join: 'passed', emits_files: ['out.json'] }),
    task('final', { join: 'passed' }),
  ],
  edges: [
    edge('seed', 'fan'),
    edge('fan', 'work'),
    edge('work', 'check', { join: 'passed', required: false }),
    edge('check', 'final', { join: 'passed' }),
  ],
};

function layerOf(layout: ReturnType<typeof layoutWorkflow>, name: string): number {
  const found = layout.nodes.find((n) => n.node.name === name);
  if (found === undefined) throw new Error(`${name} is not in the layout`);
  return found.layer;
}

describe('layer assignment', () => {
  it('puts a task one past its deepest dependency', () => {
    const layout = layoutWorkflow(PACK);
    expect([
      layerOf(layout, 'seed'),
      layerOf(layout, 'fan'),
      layerOf(layout, 'work'),
      layerOf(layout, 'check'),
      layerOf(layout, 'final'),
    ]).toEqual([0, 1, 2, 3, 4]);
  });

  /// Longest path, not shortest: a shortcut edge must not pull a task left of the dependency it
  /// would then be drawn before.
  it('takes the longest path when a task has both a long and a short route to a root', () => {
    const layout = layoutWorkflow({
      workflow_type: 'playbook',
      result: null,
      nodes: [task('a'), task('b'), task('c')],
      edges: [edge('a', 'b'), edge('b', 'c'), edge('a', 'c')],
    });
    expect(layerOf(layout, 'c')).toBe(2);
  });

  it('ignores an edge to a name the graph does not carry, and survives a cycle on the wire', () => {
    const layout = layoutWorkflow({
      workflow_type: 'playbook',
      result: null,
      nodes: [task('a'), task('b')],
      edges: [edge('ghost', 'a'), edge('a', 'b'), edge('b', 'a')],
    });
    expect(layout.nodes).toHaveLength(2);
    expect(layout.edges.map((e) => `${e.from}->${e.to}`)).toEqual(['a->b', 'b->a']);
  });

  it('lays out a single task and an empty graph without collapsing', () => {
    const one = layoutWorkflow({
      workflow_type: 'playbook',
      result: 'a',
      nodes: [task('a')],
      edges: [],
    });
    expect(one.nodes).toHaveLength(1);
    expect(one.width).toBeGreaterThan(0);
    expect(one.height).toBeGreaterThan(0);

    const none = layoutWorkflow({
      workflow_type: 'playbook',
      result: null,
      nodes: [],
      edges: [],
    });
    expect(none.nodes).toEqual([]);
    expect(none.height).toBeGreaterThan(0);
  });
});

describe('ordering within a layer', () => {
  /// Two parallel branches declared in the order that crosses. Ordering by dependency position
  /// untangles them, and the count it reports is the one the arrangement actually has.
  it('reduces crossings and reports what is left', () => {
    const graph: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: null,
      nodes: [task('a1'), task('a2'), task('b1'), task('b2')],
      edges: [edge('a1', 'b2'), edge('a2', 'b1')],
    };
    const tangled = countCrossings(
      [
        ['a1', 'a2'],
        ['b1', 'b2'],
      ],
      graph.edges
    );
    expect(tangled).toBe(1);

    const layout = layoutWorkflow(graph);
    expect(layout.crossings).toBe(0);
    const second = layout.nodes.filter((n) => n.layer === 1).map((n) => n.node.name);
    expect(second).toEqual(['b2', 'b1']);
  });

  it('is a function of the input alone', () => {
    const graph: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: null,
      nodes: [task('r1'), task('r2'), task('r3'), task('m1'), task('m2'), task('m3'), task('sink')],
      edges: [
        edge('r1', 'm3'),
        edge('r2', 'm1'),
        edge('r3', 'm2'),
        edge('m1', 'sink'),
        edge('m2', 'sink'),
        edge('m3', 'sink'),
      ],
    };
    const once = layoutWorkflow(graph).nodes.map((n) => `${n.node.name}@${n.layer}.${n.order}`);
    const twice = layoutWorkflow(graph).nodes.map((n) => `${n.node.name}@${n.layer}.${n.order}`);
    expect(twice).toEqual(once);
    expect(layoutWorkflow(graph).crossings).toBeLessThanOrEqual(
      countCrossings([['r1', 'r2', 'r3'], ['m1', 'm2', 'm3'], ['sink']], graph.edges)
    );
  });

  it('gives every node in a layer its own row and centres the layer', () => {
    const layout = layoutWorkflow({
      workflow_type: 'playbook',
      result: null,
      nodes: [task('root'), task('a'), task('b'), task('c')],
      edges: [edge('root', 'a'), edge('root', 'b'), edge('root', 'c')],
    });
    const fanned = layout.nodes.filter((n) => n.layer === 1).sort((l, r) => l.y - r.y);
    expect(fanned.map((n) => n.order)).toEqual([0, 1, 2]);
    expect(new Set(fanned.map((n) => n.y)).size).toBe(3);

    const root = layout.nodes.find((n) => n.layer === 0);
    const middle = fanned[1];
    expect(root?.y).toBe(middle?.y);
  });
});

describe('what the marks say', () => {
  it('labels an edge with its join, and keeps the files the producer stages as plumbing', () => {
    const layout = layoutWorkflow(PACK);
    const joining = layout.edges.find((e) => e.to === 'final');
    expect(joining?.label).toBe('passed');
    expect(joining?.plumbing).toBe('out.json');
    expect(layout.edges.find((e) => e.to === 'fan')?.label).toBeNull();
    expect(layout.edges.find((e) => e.to === 'fan')?.plumbing).toBeNull();
    expect(layout.edges.find((e) => e.to === 'check')?.required).toBe(false);
  });

  it('folds several staged files into one label with a count', () => {
    const doc: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: 'sink',
      nodes: [task('src', { emits_files: ['A.md', 'B.md', 'C.md'] }), task('sink')],
      edges: [edge('src', 'sink')],
    };
    expect(layoutWorkflow(doc).edges[0]?.plumbing).toBe('A.md +2');
  });

  it('marks the result task', () => {
    const layout = layoutWorkflow(PACK);
    expect(layout.nodes.filter((n) => n.isResult).map((n) => n.node.name)).toEqual(['final']);
  });

  it('draws every edge as a cubic from the producer to the consumer', () => {
    const layout = layoutWorkflow(PACK);
    for (const laid of layout.edges) {
      expect(laid.path).toMatch(/^M [\d.-]+ [\d.-]+ C /);
    }
  });
});

describe('edge routing', () => {
  it('routes a layer-skipping edge through the gutter below the cards', () => {
    const doc: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: 'sink',
      nodes: [
        task('root'),
        task('mid-a'),
        task('mid-b'),
        task('mid-c'),
        task('sink'),
      ],
      edges: [
        edge('root', 'mid-a'),
        edge('root', 'mid-b'),
        edge('root', 'mid-c'),
        edge('mid-a', 'sink'),
        edge('mid-b', 'sink'),
        edge('mid-c', 'sink'),
        edge('root', 'sink'),
      ],
    };
    const layout = layoutWorkflow(doc);
    const skip = layout.edges.find((e) => e.from === 'root' && e.to === 'sink');
    if (skip === undefined) throw new Error('skip edge missing');
    const cardBottom = Math.max(...layout.nodes.map((n) => n.y + n.height));
    // The path's bend waypoint (the C segment's landing between the endpoints) dips below
    // every card, so the chord reads as a detour, not a link into the middle rows.
    const ys = [...skip.path.matchAll(/C [\d.-]+ [\d.-]+, [\d.-]+ [\d.-]+, [\d.-]+ ([\d.-]+)/g)].map(
      (m) => Number(m[1])
    );
    expect(Math.max(...ys)).toBeGreaterThan(cardBottom);
  });
});

describe('the boxes the flow renders into', () => {
  /// Every card is the same size whatever it holds: the renderer truncates, so a pathological
  /// task name cannot stretch one column past the rest.
  it('gives every task the same box, whatever its name', () => {
    const layout = layoutWorkflow({
      workflow_type: 'playbook',
      result: null,
      nodes: [task('go'), task('a-task-name-that-runs-on-and-on-and-on')],
      edges: [],
    });
    expect(layout.nodes.map((n) => [n.width, n.height])).toEqual([
      [NODE_W, NODE_H],
      [NODE_W, NODE_H],
    ]);
  });

  it('separates the layers by more than a node is wide', () => {
    const layout = layoutWorkflow({
      workflow_type: 'playbook',
      result: null,
      nodes: [task('a'), task('b')],
      edges: [edge('a', 'b')],
    });
    const [first, second] = layout.nodes.sort((l, r) => l.layer - r.layer);
    expect((second?.x ?? 0) - (first?.x ?? 0)).toBeGreaterThan(NODE_W);
  });
});

describe('direction', () => {
  /// Top down is left to right transposed: the same layers, the same order within them, with
  /// the layer axis now running down the page.
  it('turns the layers to run down the page', () => {
    const across = layoutWorkflow(PACK, 'LR');
    const down = layoutWorkflow(PACK, 'TD');
    const byName = (layout: ReturnType<typeof layoutWorkflow>) =>
      new Map(layout.nodes.map((n) => [n.node.name, n]));
    const lr = byName(across);
    const td = byName(down);
    for (const name of lr.keys()) {
      expect(td.get(name)?.layer).toBe(lr.get(name)?.layer);
      expect(td.get(name)?.order).toBe(lr.get(name)?.order);
      expect([td.get(name)?.width, td.get(name)?.height]).toEqual([NODE_W, NODE_H]);
    }
    const seed = td.get('seed');
    const fan = td.get('fan');
    expect(seed?.x).toBe(fan?.x);
    expect((fan?.y ?? 0) - (seed?.y ?? 0)).toBeGreaterThan(NODE_H);
    expect(down.height).toBeGreaterThan(down.width);
    expect(across.width).toBeGreaterThan(across.height);
  });

  it('leaves a card from its bottom edge and enters the next at its top', () => {
    const layout = layoutWorkflow(PACK, 'TD');
    const seed = layout.nodes.find((n) => n.node.name === 'seed');
    const fan = layout.nodes.find((n) => n.node.name === 'fan');
    const edge = layout.edges.find((e) => e.from === 'seed' && e.to === 'fan');
    if (seed === undefined || fan === undefined || edge === undefined) throw new Error('missing');
    const m = /^M ([\d.-]+) ([\d.-]+) C .*, ([\d.-]+) ([\d.-]+)$/.exec(edge.path);
    if (m === null) throw new Error(`unexpected path ${edge.path}`);
    expect([Number(m[1]), Number(m[2])]).toEqual([seed.x + NODE_W / 2, seed.y + NODE_H]);
    expect([Number(m[3]), Number(m[4])]).toEqual([fan.x + NODE_W / 2, fan.y]);
  });

  it('routes a layer-skipping edge through the gutter beside the cards', () => {
    const doc: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: 'sink',
      nodes: [task('root'), task('mid-a'), task('mid-b'), task('sink')],
      edges: [
        edge('root', 'mid-a'),
        edge('root', 'mid-b'),
        edge('mid-a', 'sink'),
        edge('mid-b', 'sink'),
        edge('root', 'sink'),
      ],
    };
    const layout = layoutWorkflow(doc, 'TD');
    const skip = layout.edges.find((e) => e.from === 'root' && e.to === 'sink');
    if (skip === undefined) throw new Error('skip edge missing');
    const cardRight = Math.max(...layout.nodes.map((n) => n.x + n.width));
    const xs = [...skip.path.matchAll(/C [\d.-]+ [\d.-]+, [\d.-]+ [\d.-]+, ([\d.-]+) [\d.-]+/g)].map(
      (m) => Number(m[1])
    );
    expect(Math.max(...xs)).toBeGreaterThan(cardRight);
    expect(layout.width).toBeGreaterThan(cardRight);
  });

  it('puts a top-down join label on the edge rather than beside it', () => {
    const across = layoutWorkflow(PACK, 'LR').edges.find((e) => e.to === 'final');
    const down = layoutWorkflow(PACK, 'TD').edges.find((e) => e.to === 'final');
    const check = layoutWorkflow(PACK, 'TD').nodes.find((n) => n.node.name === 'check');
    if (across === undefined || down === undefined || check === undefined) throw new Error('missing');
    expect(down.label).toBe(across.label);
    expect(down.labelX).toBe(check.x + NODE_W / 2);
  });
});

describe('ancestry', () => {
  it('is every task a task transitively depends on, itself included', () => {
    const layout = layoutWorkflow(PACK);
    expect([...(layout.ancestry.get('final') ?? [])].sort()).toEqual([
      'check',
      'fan',
      'final',
      'seed',
      'work',
    ]);
    expect([...(layout.ancestry.get('seed') ?? [])]).toEqual(['seed']);
  });

  it('terminates on a cycle the wire carried', () => {
    const layout = layoutWorkflow({
      workflow_type: 'playbook',
      result: null,
      nodes: [task('a'), task('b')],
      edges: [edge('a', 'b'), edge('b', 'a')],
    });
    expect([...(layout.ancestry.get('a') ?? [])].sort()).toEqual(['a', 'b']);
  });
});

describe('what an edge reads', () => {
  const over = { over_task: 'scan', over_field: 'items', max_fanout: 4 };
  const routed: WorkflowGraphDoc = {
    workflow_type: 'playbook',
    result: null,
    nodes: [
      task('scan'),
      task('triage', { kind: 'route', fanout: over, questions: ['tier'] }),
      task('fix', { fanout: over, keyed: ['scan.notes'] }),
    ],
    edges: [
      edge('scan', 'triage'),
      edge('triage', 'fix', {
        aligned: true,
        when: { route: 'triage', question: 'tier', labels: ['high', 'low'] },
      }),
      edge('scan', 'fix', { keyed: ['notes'] }),
    ],
  };
  const laid = (from: string, to: string) => {
    const found = layoutWorkflow(routed).edges.find((e) => e.from === from && e.to === to);
    if (found === undefined) throw new Error(`${from} -> ${to} is not laid out`);
    return found;
  };

  it('labels a when edge with its question and answers', () => {
    expect(laid('triage', 'fix').reads).toBe('tier: high|low');
    expect(laid('triage', 'fix').label).toBeNull();
  });

  it('labels a narrowed edge with the field each instance reads its entry of', () => {
    expect(laid('scan', 'fix').reads).toBeNull();
    expect(laid('scan', 'fix').plumbing).toBe('notes[item]');
    expect(edgeWhen(edge('a', 'b', { keyed: ['x', 'y'] }))).toBeNull();
  });

  /// Left to right the label lies across the gutter, so the gutter holds it rather than letting
  /// it run under the cards either side.
  it('widens the gutters left to right to hold the widest label an edge reads', () => {
    const plain = layoutWorkflow({ ...routed, edges: routed.edges.map((e) => edge(e.from, e.to)) });
    const wide = layoutWorkflow(routed);
    const x = (layout: ReturnType<typeof layoutWorkflow>, name: string) =>
      layout.nodes.find((n) => n.node.name === name)?.x ?? 0;
    const gap = x(wide, 'fix') - x(wide, 'triage') - NODE_W;
    expect(x(plain, 'fix') - x(plain, 'triage') - NODE_W).toBe(LAYER_GAP);
    expect(gap).toBeGreaterThan('tier: high|low'.length * 5.6);
    const down = layoutWorkflow(routed, 'TD');
    const y = (name: string) => down.nodes.find((n) => n.node.name === name)?.y ?? 0;
    expect(y('fix') - y('triage') - NODE_H).toBe(LAYER_GAP);
  });

  it('carries which edges are read per element', () => {
    expect(laid('triage', 'fix').aligned).toBe(true);
    expect(laid('scan', 'triage').aligned).toBe(false);
    expect(laid('scan', 'triage').reads).toBeNull();
  });
});

/// seed → triage ⇒ fix → report as a run draws it: triage dealt out to two instances, fix to the
/// one triage let through, each fix instance fed by its own triage instance.
describe('runtime instances', () => {
  const over = { over_task: 'seed', over_field: 'items', max_fanout: null };
  const when = { route: 'triage', question: 'tier', labels: ['high', 'low'] };
  const RUN: WorkflowGraphDoc = {
    workflow_type: 'run',
    result: null,
    nodes: [
      task('seed'),
      task('triage', { kind: 'route', fanout: over, required: false }),
      task('fix', { kind: 'agent', fanout: over, required: false }),
      task('report'),
      task('triage[a]', { kind: 'route', required: false }),
      task('triage[b]', { kind: 'route', required: false }),
      task('fix[a]', { kind: 'agent', required: false }),
    ],
    edges: [
      edge('seed', 'triage', { required: false }),
      edge('triage', 'fix', { aligned: true, when, required: false }),
      edge('triage[a]', 'fix[a]', {
        aligned: true,
        when: { route: 'triage[a]', question: 'tier', labels: ['high'] },
        required: false,
      }),
      edge('triage', 'triage[a]', { required: false }),
      edge('triage', 'triage[b]', { required: false }),
      edge('fix[a]', 'report'),
    ],
  };
  const at = (layout: ReturnType<typeof layoutWorkflow>, name: string) => {
    const found = layout.nodes.find((n) => n.node.name === name);
    if (found === undefined) throw new Error(`${name} is not in the layout`);
    return found;
  };
  const ends = (path: string): number[][] => {
    const start = /^M ([\d.-]+) ([\d.-]+)/.exec(path);
    const lands = [...path.matchAll(/C [\d.-]+ [\d.-]+, [\d.-]+ [\d.-]+, ([\d.-]+) ([\d.-]+)/g)];
    return [
      [Number(start?.[1]), Number(start?.[2])],
      ...lands.map((m) => [Number(m[1]), Number(m[2])]),
    ];
  };

  it('names the deck an instance was dealt from', () => {
    expect(mappedFrom('fix[a]')).toBe('fix');
    expect(mappedFrom('fix[a[b]]')).toBe('fix');
    expect(mappedFrom('fix')).toBeNull();
    expect(mappedFrom('[a]')).toBeNull();
    expect(mappedFrom('fix[a')).toBeNull();
  });

  it("draws an instance as a row under its deck, in the deck's layer", () => {
    const layout = layoutWorkflow(RUN);
    const deck = at(layout, 'triage');
    const first = at(layout, 'triage[a]');
    const second = at(layout, 'triage[b]');
    expect(deck.instance).toBeNull();
    expect(first.instance).toEqual({ deck: 'triage', index: 0 });
    expect(second.instance).toEqual({ deck: 'triage', index: 1 });
    expect([first.layer, second.layer]).toEqual([deck.layer, deck.layer]);
    expect(first.x).toBe(deck.x + INSTANCE_INDENT);
    expect(first.x + first.width).toBe(deck.x + NODE_W);
    expect(first.height).toBe(INSTANCE_H);
    expect(first.y).toBeGreaterThan(deck.y + NODE_H);
    expect(second.y).toBeGreaterThan(first.y + INSTANCE_H);
  });

  it('spends no layer on a fan-out', () => {
    const layout = layoutWorkflow(RUN);
    expect(['seed', 'triage', 'fix', 'report'].map((name) => at(layout, name).layer)).toEqual([
      0, 1, 2, 3,
    ]);
    expect(at(layout, 'fix[a]').layer).toBe(2);
  });

  /// The row's place under its deck already says it is the deck's; an edge to it would run
  /// backwards into the card it hangs from.
  it('does not draw the edge from a deck to its own instance, and traces it all the same', () => {
    const layout = layoutWorkflow(RUN);
    expect(layout.edges.map((e) => `${e.from}->${e.to}`).sort()).toEqual([
      'fix[a]->report',
      'seed->triage',
      'triage->fix',
      'triage[a]->fix[a]',
    ]);
    expect([...(layout.ancestry.get('fix[a]') ?? [])].sort()).toEqual([
      'fix',
      'fix[a]',
      'seed',
      'triage',
      'triage[a]',
    ]);
  });

  it('lines a deck up with the deck it reads per element, and row with row', () => {
    const layout = layoutWorkflow(RUN);
    expect(at(layout, 'fix').y).toBe(at(layout, 'triage').y);
    expect(at(layout, 'fix[a]').y).toBe(at(layout, 'triage[a]').y);
    const pair = layout.edges.find((e) => e.from === 'triage[a]');
    if (pair === undefined) throw new Error('per-element edge missing');
    const [start, end] = ends(pair.path);
    const row = at(layout, 'triage[a]');
    expect(start).toEqual([row.x + row.width, row.y + INSTANCE_H / 2]);
    expect(end?.[1]).toBe(start?.[1]);
  });

  /// Top down a row under the card would put the card in the way of every edge into the row.
  it('sits instance rows beside the card top down, level with it', () => {
    const layout = layoutWorkflow(RUN, 'TD');
    const deck = at(layout, 'triage');
    const first = at(layout, 'triage[a]');
    const second = at(layout, 'triage[b]');
    expect(first.x).toBeGreaterThan(deck.x + NODE_W);
    expect(second.x).toBeGreaterThan(first.x + first.width);
    expect(first.y + INSTANCE_H / 2).toBe(deck.y + NODE_H / 2);
    expect(first.layer).toBe(deck.layer);
  });
});

describe('gutters and labels', () => {
  const over = { over_task: 'seed', over_field: 'items', max_fanout: null };
  const doc: WorkflowGraphDoc = {
    workflow_type: 'playbook',
    result: null,
    nodes: [
      task('seed'),
      task('triage', { kind: 'route', fanout: over }),
      task('fix', { fanout: over }),
      task('report'),
    ],
    edges: [
      edge('seed', 'triage'),
      edge('triage', 'fix', {
        aligned: true,
        when: { route: 'triage', question: 'tier', labels: ['high', 'low'] },
      }),
      edge('fix', 'report'),
    ],
  };
  const x = (layout: ReturnType<typeof layoutWorkflow>, name: string) =>
    layout.nodes.find((n) => n.node.name === name)?.x ?? 0;

  it('sizes each gutter to the labels drawn in it, not to the widest in the graph', () => {
    const layout = layoutWorkflow(doc);
    expect(x(layout, 'triage') - x(layout, 'seed') - NODE_W).toBe(LAYER_GAP);
    expect(x(layout, 'fix') - x(layout, 'triage') - NODE_W).toBeGreaterThan(
      'tier: high|low'.length * 5.6
    );
    expect(x(layout, 'report') - x(layout, 'fix') - NODE_W).toBe(LAYER_GAP);
  });

  it('centres a label in the gutter before the task that reads it', () => {
    const layout = layoutWorkflow(doc);
    const read = layout.edges.find((e) => e.to === 'fix');
    if (read === undefined) throw new Error('when edge missing');
    const gutterMiddle = (x(layout, 'triage') + NODE_W + x(layout, 'fix')) / 2;
    expect(Math.abs(read.labelX - gutterMiddle)).toBeLessThan(0.5);
  });

  it('keeps a bent edge label out of the layer it bends through', () => {
    const bent: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: null,
      nodes: [task('root'), task('mid'), task('sink')],
      edges: [edge('root', 'mid'), edge('mid', 'sink'), edge('root', 'sink', { keyed: ['notes'] })],
    };
    const layout = layoutWorkflow(bent);
    const skip = layout.edges.find((e) => e.from === 'root' && e.to === 'sink');
    if (skip === undefined) throw new Error('bent edge missing');
    expect(skip.labelX).toBeGreaterThan(x(layout, 'mid') + NODE_W);
    expect(skip.labelX).toBeLessThan(x(layout, 'sink'));
  });

  /// A layer of one card does not send an edge bending through it past a taller layer elsewhere.
  it('runs a bend just past the cards of the layer it crosses', () => {
    const lanes: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: null,
      nodes: ['root', 'a1', 'a2', 'a3', 'b', 'sink'].map((name) => task(name)),
      edges: [
        edge('root', 'a1'),
        edge('root', 'a2'),
        edge('root', 'a3'),
        edge('a1', 'b'),
        edge('a2', 'b'),
        edge('a3', 'b'),
        edge('b', 'sink'),
        edge('root', 'sink'),
      ],
    };
    const layout = layoutWorkflow(lanes);
    const skip = layout.edges.find((e) => e.from === 'root' && e.to === 'sink');
    if (skip === undefined) throw new Error('skip edge missing');
    const ys = [...skip.path.matchAll(/C [\d.-]+ [\d.-]+, [\d.-]+ [\d.-]+, [\d.-]+ ([\d.-]+)/g)].map(
      (m) => Number(m[1])
    );
    const bottom = (names: string[]) =>
      Math.max(
        ...layout.nodes.filter((n) => names.includes(n.node.name)).map((n) => n.y + n.height)
      );
    const [throughA, throughB] = ys;
    expect(throughA).toBeGreaterThan(bottom(['a1', 'a2', 'a3']));
    expect(throughB).toBeGreaterThan(bottom(['b']));
    expect(throughB).toBeLessThan(bottom(['a1', 'a2', 'a3']));
  });
});

describe('fitting the view', () => {
  it('draws a small graph at the largest zoom, centred', () => {
    const view = fitViewport({ width: 200, height: 100 }, { width: 1000, height: 600 });
    expect(view.zoom).toBe(FIT_MAX_ZOOM);
    expect(view.x).toBeCloseTo(12 + (976 - 200 * FIT_MAX_ZOOM) / 2);
    expect(view.y).toBeCloseTo(40 + (548 - 100 * FIT_MAX_ZOOM) / 2);
  });

  it('fills the room with a graph that fits between the bounds', () => {
    const view = fitViewport({ width: 1000, height: 300 }, { width: 1124, height: 800 });
    expect(view.zoom).toBeCloseTo(1.1);
    expect(view.x).toBeCloseTo(12);
    expect(view.y).toBeCloseTo(40 + (748 - 330) / 2);
  });

  /// Shrinking a long graph to fit makes every label unreadable: it is drawn at the floor and from
  /// its start, and the reader pans along it.
  it('holds a long graph at the legible floor and shows it from its start', () => {
    const view = fitViewport({ width: 3000, height: 300 }, { width: 1000, height: 400 });
    expect(view.zoom).toBe(FIT_MIN_ZOOM);
    expect(view.x).toBe(12);
    expect(view.y).toBeCloseTo(40 + (348 - 300 * FIT_MIN_ZOOM) / 2);
  });

  it('anchors a graph taller than the room at its top', () => {
    const view = fitViewport({ width: 400, height: 3000 }, { width: 1000, height: 400 });
    expect(view.zoom).toBe(FIT_MIN_ZOOM);
    expect(view.y).toBe(40);
  });

  it('survives a canvas not measured yet', () => {
    const view = fitViewport({ width: 400, height: 300 }, { width: 0, height: 0 });
    expect(view.zoom).toBe(FIT_MIN_ZOOM);
    expect(Number.isFinite(view.x) && Number.isFinite(view.y)).toBe(true);
  });

  it('sizes the frame to hold the graph at the floor and no more than it needs at the top', () => {
    expect(frameHeights({ width: 2000, height: 354 }, false)).toEqual({
      min: Math.ceil(354 * FIT_MIN_ZOOM + 52),
      max: Math.ceil(354 * FIT_MAX_ZOOM + 52),
    });
    expect(frameHeights({ width: 400, height: 40 }, false)).toEqual({ min: 240, max: 240 });
  });

  /// The task panel opens inside the frame, so a short graph must not leave it a sliver.
  it('gives an open task panel room to read in, however short the graph', () => {
    expect(frameHeights({ width: 400, height: 40 }, true)).toEqual({ min: 480, max: 480 });
    const tall = frameHeights({ width: 400, height: 900 }, true);
    expect(tall).toEqual(frameHeights({ width: 400, height: 900 }, false));
  });
});

/// A run draws a plain command task as a pill sized to what it says; a mapped, routed or agent
/// task keeps its card, and a compiled plan keeps every card.
describe('pills', () => {
  const over = { over_task: 'seed', over_field: 'items', max_fanout: null };
  const doc: WorkflowGraphDoc = {
    workflow_type: 'run',
    result: null,
    nodes: [
      task('seed'),
      task('triage', { kind: 'route', fanout: over }),
      task('fan', { fanout: over }),
      task('think', { kind: 'agent' }),
      task('report'),
    ],
    edges: [
      edge('seed', 'triage'),
      edge('seed', 'fan'),
      edge('seed', 'think'),
      edge('triage', 'report'),
      edge('fan', 'report'),
      edge('think', 'report'),
    ],
  };
  const PILLS = { trailing: new Map<string, number>() };
  const at = (layout: ReturnType<typeof layoutWorkflow>, name: string) => {
    const found = layout.nodes.find((n) => n.node.name === name);
    if (found === undefined) throw new Error(`${name} is not in the layout`);
    return found;
  };

  it('pills only a plain command task, and only when asked', () => {
    expect(isPill(task('a'), PILLS)).toBe(true);
    expect(isPill(task('a'), null)).toBe(false);
    expect(isPill(task('a', { fanout: over }), PILLS)).toBe(false);
    expect(isPill(task('a', { kind: 'route' }), PILLS)).toBe(false);
    expect(isPill(task('a', { kind: 'agent' }), PILLS)).toBe(false);
  });

  it('sizes a pill to its name and what trails it, between a floor and a card', () => {
    expect(pillWidth('ab', 0)).toBe(PILL_MIN_W);
    expect(pillWidth('decision_audit', 0)).toBeGreaterThan(pillWidth('report', 0));
    expect(pillWidth('report', 6)).toBeGreaterThan(pillWidth('report', 0));
    expect(pillWidth('x'.repeat(80), 40)).toBe(NODE_W);
  });

  it('draws pills one line high and keeps cards for everything else', () => {
    const layout = layoutWorkflow(doc, 'LR', { pills: PILLS });
    expect([at(layout, 'seed').width, at(layout, 'seed').height]).toEqual([
      pillWidth('seed', 0),
      PILL_H,
    ]);
    for (const name of ['triage', 'fan', 'think']) {
      expect([at(layout, name).width, at(layout, name).height]).toEqual([NODE_W, NODE_H]);
    }
    const plan = layoutWorkflow(doc);
    expect([at(plan, 'seed').width, at(plan, 'seed').height]).toEqual([NODE_W, NODE_H]);
  });

  it('widens a pill for the figures it trails', () => {
    const plain = layoutWorkflow(doc, 'LR', { pills: PILLS });
    const costed = layoutWorkflow(doc, 'LR', { pills: { trailing: new Map([['report', 5]]) } });
    expect(at(costed, 'report').width).toBe(pillWidth('report', 5));
    expect(at(costed, 'report').width).toBeGreaterThan(at(plain, 'report').width);
  });

  /// A layer of pills is only as deep as its widest pill, which is what makes a run graph short.
  it('makes a layer of pills shallower than a layer of cards, and centres a pill among cards', () => {
    const pilled = layoutWorkflow(doc, 'LR', { pills: PILLS });
    const carded = layoutWorkflow(doc);
    expect(at(pilled, 'triage').x - at(carded, 'triage').x).toBe(pillWidth('seed', 0) - NODE_W);
    expect(pilled.width).toBeLessThan(carded.width);
    const mixed = layoutWorkflow(
      { ...doc, nodes: [...doc.nodes, task('side')], edges: [...doc.edges, edge('seed', 'side')] },
      'LR',
      { pills: PILLS }
    );
    const side = at(mixed, 'side');
    expect(side.x + side.width / 2).toBe(at(mixed, 'think').x + NODE_W / 2);
  });

  it('turns a pill on its side top down', () => {
    const layout = layoutWorkflow(doc, 'TD', { pills: PILLS });
    expect([at(layout, 'seed').width, at(layout, 'seed').height]).toEqual([
      pillWidth('seed', 0),
      PILL_H,
    ]);
    expect(at(layout, 'triage').y).toBe(at(layout, 'seed').y + PILL_H + LAYER_GAP);
  });
});

/// What an edge carries is plumbing: shown on demand, so it never widens a gutter.
describe('plumbing labels', () => {
  it('leaves a gutter narrow for an edge that only narrows a field or stages a file', () => {
    const doc: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: null,
      nodes: [task('scan', { emits_files: ['NOTES.md'] }), task('fix')],
      edges: [edge('scan', 'fix', { keyed: ['notes', 'lanes'] })],
    };
    const layout = layoutWorkflow(doc);
    const [scan, fix] = ['scan', 'fix'].map((name) => layout.nodes.find((n) => n.node.name === name));
    expect((fix?.x ?? 0) - (scan?.x ?? 0) - NODE_W).toBe(LAYER_GAP);
    expect(layout.edges[0]?.plumbing).toBe('notes[item] · lanes[item] · NOTES.md');
    expect(layout.edges[0]?.reads).toBeNull();
    expect(layout.edges[0]?.label).toBeNull();
  });
});
