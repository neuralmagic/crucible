import { describe, expect, it } from 'vitest';
import {
  countCrossings,
  layoutWorkflow,
  NODE_H,
  NODE_W,
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
    ...overrides,
  };
}

function edge(
  from: string,
  to: string,
  overrides: Partial<WorkflowGraphDoc['edges'][number]> = {}
): WorkflowGraphDoc['edges'][number] {
  return { from, to, join: 'all', required: true, ...overrides };
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
  it('labels an edge with the join and the files the producer stages', () => {
    const layout = layoutWorkflow(PACK);
    const joining = layout.edges.find((e) => e.to === 'final');
    expect(joining?.label).toBe('passed · out.json');
    expect(layout.edges.find((e) => e.to === 'fan')?.label).toBeNull();
    expect(layout.edges.find((e) => e.to === 'check')?.required).toBe(false);
  });

  it('folds several staged files into one label with a count', () => {
    const doc: WorkflowGraphDoc = {
      workflow_type: 'playbook',
      result: 'sink',
      nodes: [task('src', { emits_files: ['A.md', 'B.md', 'C.md'] }), task('sink')],
      edges: [edge('src', 'sink')],
    };
    expect(layoutWorkflow(doc).edges[0]?.label).toBe('A.md +2');
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
