import { describe, expect, it } from 'vitest';
import type { ExternalLinkRef } from '../ui/ExternalLink';
import {
  fanoutStates,
  latestResults,
  nodeLinks,
  planEdge,
  runGraphView,
  targetLabel,
  toneOf,
  UNDECLARED_OUTPUTS,
  whenOf,
  type FanOutCount,
  type GraphOutput,
  type OutputTarget,
  type PlanTask,
  type RouteDecision,
  type TaskResult,
} from './taskGraph';

/// What an edge carries when it is neither conditional, per element, nor narrowed.
const PLAIN = { when: null, aligned: false, keyed: [] };

const task = (name: string, depends_on: string[] = [], session = ''): PlanTask => ({
  name,
  kind: 'command',
  depends_on,
  session,
  needs: 'all',
  required: true,
  over: '',
  max_fanout: 0,
  when: '',
  keyed: [],
});

/// A task mapped over a producer's field, as the wire writes it.
const mapped = (name: string, over: string, depends_on: string[]): PlanTask => ({
  ...task(name, depends_on),
  over,
  max_fanout: 0,
});

const fold = (
  tasks: PlanTask[],
  results: TaskResult[] = [],
  outputs: GraphOutput[] | null = [],
  fanout: FanOutCount[] = [],
  running = false,
  decisions: RouteDecision[] = [],
) => runGraphView({ tasks, results, outputs, fanout, decisions, running });

const result = (iter: number, taskName: string, status: string): TaskResult => ({
  iter,
  task: taskName,
  status,
  note: '',
  cost_usd: null,
  secs: null,
  links: [],
  repairs: [],
});

describe('status folding', () => {
  it('keeps the highest iteration per task', () => {
    const latest = latestResults([
      result(0, 'measure', 'pass'),
      result(2, 'measure', 'fail'),
      result(1, 'measure', 'pass'),
    ]);
    expect(latest.get('measure')?.status).toBe('fail');
  });

  it('reads pass, fail, and transport, and nothing else', () => {
    expect(toneOf('pass')).toBe('pass');
    expect(toneOf('fail')).toBe('fail');
    expect(toneOf('transport')).toBe('fail');
    expect(toneOf('skipped')).toBe('none');
    expect(toneOf(null)).toBe('none');
  });
});

describe('the graph document a run folds to', () => {
  const view = (tasks: PlanTask[], results: TaskResult[] = []) => {
    const folded = fold(tasks, results);
    return {
      node: (name: string) => folded.graph.nodes.find((n) => n.name === name),
      edges: folded.graph.edges,
      names: folded.graph.nodes.map((n) => n.name),
      runtime: folded.runtime,
    };
  };

  it('carries each admitted task, its dependencies, and how it ended', () => {
    const g = view(
      [
        task('propose-a'),
        task('propose-b'),
        task('pick', ['propose-a', 'propose-b']),
        task('measure', ['pick']),
      ],
      [result(0, 'propose-a', 'pass'), result(0, 'measure', 'transport')],
    );
    expect(g.names).toEqual(['propose-a', 'propose-b', 'pick', 'measure']);
    expect(g.edges).toEqual([
      { from: 'propose-a', to: 'pick', join: 'all', required: true, ...PLAIN },
      { from: 'propose-b', to: 'pick', join: 'all', required: true, ...PLAIN },
      { from: 'pick', to: 'measure', join: 'all', required: true, ...PLAIN },
    ]);
    expect(g.runtime.get('propose-a')?.tone).toBe('pass');
    expect(g.runtime.get('measure')?.tone).toBe('fail');
    expect(g.runtime.get('pick')).toMatchObject({ status: 'never ran', reported: false });
  });

  it('maps a kind the document has no case for onto a plain task', () => {
    const g = view([{ ...task('rank'), kind: 'top_k', needs: 'quorum' }]);
    expect(g.node('rank')?.kind).toBe('other');
    expect(g.node('rank')?.needs).toBe('other');
    expect(view([{ ...task('turn'), kind: 'agent' }]).node('turn')?.kind).toBe('agent');
  });

  it('carries the session binding for the card to badge', () => {
    const g = view([task('propose', [], 'solver'), task('measure')]);
    expect(g.node('propose')?.session).toBe('solver');
    expect(g.node('measure')?.session).toBeNull();
  });

  it('sums what a task spent across its attempts and counts them', () => {
    const attempt = (iter: number, status: string, cost: number | null, secs: number | null) => ({
      ...result(iter, 'measure', status),
      note: `attempt ${iter}`,
      cost_usd: cost,
      secs,
    });
    const g = view(
      [task('measure')],
      [attempt(0, 'fail', 0.25, 30), attempt(1, 'fail', null, null), attempt(2, 'pass', 0.5, 12)],
    );
    const runtime = g.runtime.get('measure');
    expect(runtime?.attempts).toBe(3);
    expect(runtime?.latestIter).toBe(2);
    expect(runtime?.status).toBe('pass');
    expect(runtime?.costUsd).toBe(0.75);
    expect(runtime?.secs).toBe(42);
    expect(runtime?.note).toBe('attempt 2');
  });

  it('leaves cost and duration unstated when no attempt carried a figure', () => {
    const g = view([task('measure')], [result(0, 'measure', 'pass')]);
    expect(g.runtime.get('measure')?.costUsd).toBeNull();
    expect(g.runtime.get('measure')?.secs).toBeNull();
  });

  it('leaves duration unstated when every attempt in the run reports 0s', () => {
    const g = view(
      [task('measure'), task('judge')],
      [
        { ...result(0, 'measure', 'pass'), secs: 0, cost_usd: 0.5 },
        { ...result(0, 'judge', 'pass'), secs: 0 },
      ],
    );
    expect(g.runtime.get('measure')?.secs).toBeNull();
    expect(g.runtime.get('measure')?.costUsd).toBe(0.5);
    expect(g.runtime.get('judge')?.secs).toBeNull();
  });

  it('hangs an instance off the task it was mapped from, borrowing its kind', () => {
    const g = view(
      [{ ...task('summarize', ['read']), kind: 'command' }, task('read')],
      [result(1, 'summarize[flashinfer]', 'fail')],
    );
    expect(g.node('summarize[flashinfer]')?.kind).toBe('command');
    expect(g.edges).toContainEqual({
      from: 'summarize',
      to: 'summarize[flashinfer]',
      join: 'all',
      required: true,
      ...PLAIN,
    });
  });

  it('links downstream work from the leaves of a mapped task subgraph', () => {
    const g = view(
      [task('scan'), task('triage', ['scan']), task('roundup', ['scan', 'triage'])],
      [result(0, 'triage[one]', 'pass'), result(0, 'triage[two]', 'pass')],
    );
    expect(g.edges).toEqual([
      { from: 'scan', to: 'triage', join: 'all', required: true, ...PLAIN },
      { from: 'triage[one]', to: 'roundup', join: 'all', required: true, ...PLAIN },
      { from: 'triage[two]', to: 'roundup', join: 'all', required: true, ...PLAIN },
      { from: 'triage', to: 'triage[one]', join: 'all', required: true, ...PLAIN },
      { from: 'triage', to: 'triage[two]', join: 'all', required: true, ...PLAIN },
    ]);
    expect(g.edges).not.toContainEqual({
      from: 'triage',
      to: 'roundup',
      join: 'all',
      required: true,
      ...PLAIN,
    });
    expect(g.edges).not.toContainEqual({
      from: 'scan',
      to: 'roundup',
      join: 'all',
      required: true,
      ...PLAIN,
    });
  });

  it('links an implicit epilogue report after the ordinary graph sinks', () => {
    const g = view([
      task('scan'),
      task('card', ['scan']),
      { ...task('publish-report'), kind: 'report' },
    ]);
    expect(g.edges).toEqual([
      { from: 'scan', to: 'card', join: 'all', required: true, ...PLAIN },
      { from: 'card', to: 'publish-report', join: 'all', required: true, ...PLAIN },
    ]);
  });

  it('keeps results for tasks missing from the plan as edgeless nodes', () => {
    const g = view(
      [task('propose'), task('measure', ['propose'])],
      [result(0, 'propose', 'pass'), result(0, 'full-eval', 'pass'), result(1, 'ghost', 'fail')],
    );
    expect(g.names).toEqual(['propose', 'measure', 'full-eval', 'ghost']);
    expect(g.node('ghost')?.kind).toBe('other');
    expect(g.runtime.get('ghost')?.tone).toBe('fail');
    expect(g.edges).toEqual([{ from: 'propose', to: 'measure', join: 'all', required: true, ...PLAIN }]);
  });

  it('drops edges to unknown tasks and keeps a cycle the wire carried', () => {
    const g = view([task('a', ['ghost', 'b']), task('b', ['a'])]);
    expect(g.edges).toEqual([
      { from: 'b', to: 'a', join: 'all', required: true, ...PLAIN },
      { from: 'a', to: 'b', join: 'all', required: true, ...PLAIN },
    ]);
  });
});

describe('the declared outputs a graph terminates in', () => {
  const address = (address: string): OutputTarget => ({ kind: 'address', address });

  const output = (
    kind: string,
    count: number,
    target: OutputTarget | null,
    attached_to: string | null,
    source: GraphOutput['source'] = 'manifest',
  ): GraphOutput => ({ kind, count, target, attached_to, source });

  const plan = [task('scan'), task('work', ['scan']), task('publish', ['work'])];

  it('hangs a bound off the task that spends it and a homeless one off every sink', () => {
    const folded = fold(plan, [], [
      output('draft-pr', 1, address('owner/repo'), 'publish'),
      output('gpu-capture', 2, null, null),
    ]);
    const drawn = [...folded.outputs.entries()];
    expect(drawn.map(([, o]) => o.kind)).toEqual(['draft-pr', 'gpu-capture']);
    expect(drawn.every(([, o]) => !o.undeclared)).toBe(true);

    const [pr, capture] = drawn.map(([name]) => name);
    expect(folded.graph.nodes.map((n) => n.name)).toContain(pr);
    expect(folded.graph.edges).toContainEqual({ from: 'publish', to: pr, join: 'all', required: true, ...PLAIN });
    // `publish` is the plan's only sink, so the unattached bound lands there too.
    expect(folded.graph.edges).toContainEqual({ from: 'publish', to: capture, join: 'all', required: true, ...PLAIN });
    expect(folded.outputs.get(pr)).toEqual({
      kind: 'draft-pr',
      count: 1,
      target: address('owner/repo'),
      attachedTo: 'publish',
      undeclared: false,
      source: 'manifest',
    });
    expect(folded.outputs.get(capture)?.target).toBeNull();
    expect(folded.engineDefaults).toEqual([]);
  });

  /// An engine default is a bound the pack never asked for: it is listed, never drawn, so a pack
  /// declaring nothing reads as one. A bound with no source comes from an older controller and
  /// still counts as declared.
  it('keeps engine-default bounds out of the graph and lists them apart', () => {
    const folded = fold(plan, [], [
      output('image-push', 100, null, null, 'engine-default'),
      output('tracker-comment', 3, address('PROJ-1'), 'publish'),
      output('chat-message', 8, address('operator-channel'), null, null),
    ]);
    expect([...folded.outputs.values()].map((o) => [o.kind, o.source])).toEqual([
      ['tracker-comment', 'manifest'],
      ['chat-message', 'unknown'],
    ]);
    expect(folded.graph.nodes.some((n) => n.name.startsWith('image-push'))).toBe(false);
    expect(folded.engineDefaults.map((o) => [o.kind, o.count])).toEqual([['image-push', 100]]);
  });

  it('draws nothing and lists everything for a pack that declares no outputs', () => {
    const folded = fold(plan, [], [
      output('draft-pr', 2, null, null, 'engine-default'),
      output('gpu-capture', 100, null, null, 'engine-default'),
    ]);
    expect(folded.outputs.size).toBe(0);
    expect(folded.graph.nodes.map((n) => n.name)).toEqual(['scan', 'work', 'publish']);
    expect(folded.engineDefaults).toHaveLength(2);
  });

  /// Two bounds of the same kind are two nodes, not one overwriting the other.
  it('keeps one node per bound even when the kinds repeat', () => {
    const folded = fold(plan, [], [
      output('tracker-comment', 3, address('PROJ-1'), 'publish'),
      output('tracker-comment', 1, { kind: 'scope', scope: 'PROJ-', param: 'issue' }, null),
    ]);
    expect(folded.outputs.size).toBe(2);
    expect([...folded.outputs.values()].map((o) => targetLabel(o.target))).toEqual([
      'PROJ-1',
      'PROJ- (param issue)',
    ]);
  });

  /// Every terminal is a place the run could have ended, so a homeless bound hangs off all of them
  /// rather than off whichever one happens to be first.
  it('hangs a homeless bound off every sink when the plan has more than one', () => {
    const folded = fold(
      [task('scan'), task('measure', ['scan']), task('notify', ['scan'])],
      [],
      [output('chat-message', 4, address('operator-channel'), null)],
    );
    const [name] = [...folded.outputs.keys()];
    expect(folded.graph.edges).toContainEqual({ from: 'measure', to: name, join: 'all', required: true, ...PLAIN });
    expect(folded.graph.edges).toContainEqual({ from: 'notify', to: name, join: 'all', required: true, ...PLAIN });
    expect(folded.graph.edges).not.toContainEqual({ from: 'scan', to: name, join: 'all', required: true, ...PLAIN });
  });

  /// A bound naming a task this plan does not carry still renders, off the sink. An output the
  /// graph dropped would be showing less than the pack disclosed.
  it('falls back to the sink when the named producer is not in the plan', () => {
    const folded = fold(plan, [], [output('deploy', 1, address('cluster'), 'deploy_candidate')]);
    const [name] = [...folded.outputs.keys()];
    expect(folded.graph.edges).toContainEqual({ from: 'publish', to: name, join: 'all', required: true, ...PLAIN });
  });

  /// Absent-legacy: the revision stored no exposure. That is not a pack that writes nothing, so it
  /// gets its own marker node rather than silence.
  it('draws an explicit undeclared marker for a revision that stored no exposure', () => {
    const folded = fold(plan, [], null);
    const marker = folded.outputs.get(UNDECLARED_OUTPUTS);
    expect(marker).toEqual({
      kind: 'outputs undeclared',
      count: 0,
      target: null,
      attachedTo: null,
      undeclared: true,
      source: 'unknown',
    });
    expect(folded.graph.nodes.map((n) => n.name)).toContain(UNDECLARED_OUTPUTS);
    expect(folded.graph.edges).toContainEqual({
      from: 'publish',
      to: UNDECLARED_OUTPUTS,
      join: 'all',
      required: true,
      ...PLAIN,
    });
  });

  /// A pack that declares an empty output set is a pack that declared: no marker, no nodes.
  it('draws nothing extra for a pack that declares no outputs', () => {
    const folded = fold(plan, [], []);
    expect(folded.outputs.size).toBe(0);
    expect(folded.graph.nodes.map((n) => n.name)).toEqual(['scan', 'work', 'publish']);
  });
});

/// The triage plan a real CVE run admits: one agent scans, the next is mapped over what it
/// emitted, and a command rounds the instances up.
describe('what a run made of a mapped task', () => {
  const plan = [
    task('scan'),
    mapped('triage', 'scan.issues', ['scan']),
    task('roundup', ['triage']),
  ];
  const ran = [
    result(0, 'scan', 'pass'),
    result(0, 'triage[1027]', 'pass'),
    result(0, 'triage[952]', 'fail'),
    result(0, 'roundup', 'pass'),
  ];

  it('counts the items emitted against the instances that started and how they ended', () => {
    const folded = fold(plan, ran, [], [{ task: 'triage', items: 20, not_taken: 0 }]);
    expect(folded.fanout.get('triage')).toEqual({
      items: 20,
      started: 2,
      passed: 1,
      notTaken: 0,
      other: [{ status: 'fail', count: 1 }],
      running: false,
    });
  });

  /// A run whose session never reached the controller cannot say how wide the mapping was asked
  /// to be; what started is still known, and an unknown width must not read as none.
  it('leaves the item count unknown when the wire carries no width', () => {
    expect(fold(plan, ran).fanout.get('triage')).toEqual({
      items: null,
      started: 2,
      passed: 1,
      notTaken: 0,
      other: [{ status: 'fail', count: 1 }],
      running: false,
    });
  });

  it('keeps a fan-out whose producer emitted nothing, as zero rather than absent', () => {
    const folded = fold(
      [task('scan'), mapped('triage', 'scan.issues', ['scan'])],
      [result(0, 'scan', 'pass')],
      [],
      [{ task: 'triage', items: 0, not_taken: 0 }],
    );
    expect(folded.fanout.get('triage')).toEqual({
      items: 0,
      started: 0,
      passed: 0,
      notTaken: 0,
      other: [],
      running: false,
    });
  });

  it('carries the producer field onto the node so the card draws it as a deck', () => {
    const node = fold(plan, ran).graph.nodes.find((n) => n.name === 'triage');
    expect(node?.fanout).toEqual({ over_task: 'scan', over_field: 'issues', max_fanout: null });
    expect(fold(plan, ran).graph.nodes.find((n) => n.name === 'scan')?.fanout).toBeNull();
  });

  it('reads a declared cap as the cap and an uncapped mapping as none', () => {
    const capped = [{ ...mapped('triage', 'scan.issues', ['scan']), max_fanout: 4 }];
    expect(fold(capped).graph.nodes[0]?.fanout?.max_fanout).toBe(4);
  });

  /// The CVE case: twenty tickets were handed to the mapped task and only a couple needed an
  /// agent. The rest settled without running, which is neither a pass nor a failure and is most
  /// of what the run did.
  it('keeps every status its instances reported, not just pass and fail', () => {
    const settled = [
      result(0, 'scan', 'pass'),
      result(0, 'triage[1]', 'pass'),
      result(0, 'triage[2]', 'skipped'),
      result(0, 'triage[3]', 'skipped'),
      result(0, 'triage[4]', 'blocked'),
    ];
    expect(fold(plan, settled, [], [{ task: 'triage', items: 4, not_taken: 0 }]).fanout.get('triage')).toEqual({
      items: 4,
      started: 4,
      passed: 1,
      notTaken: 0,
      other: [
        { status: 'skipped', count: 2 },
        { status: 'blocked', count: 1 },
      ],
      running: false,
    });
  });

  it('counts instances against their mapped task alone', () => {
    const states = fanoutStates(plan, latestResults(ran), [], false);
    expect([...states.keys()]).toEqual(['triage']);
  });
});

describe('a declared task nothing reported on', () => {
  const plan = [task('scan'), mapped('triage', 'scan.issues', ['scan']), task('roundup', ['triage'])];

  it('never ran once the run is over, and is pending while it is not', () => {
    const over = fold(plan, [result(0, 'scan', 'pass')]);
    expect(over.runtime.get('roundup')).toMatchObject({
      status: 'never ran',
      reported: false,
      tone: 'none',
    });
    const going = fold(plan, [result(0, 'scan', 'pass')], [], [], true);
    expect(going.runtime.get('roundup')?.status).toBe('pending');
  });

  /// A mapped task is the deck its instances came out of, so its own silence is not a verdict.
  it('says nothing about a mapped task whose instances reported', () => {
    const folded = fold(plan, [result(0, 'scan', 'pass'), result(0, 'triage[1027]', 'pass')]);
    expect(folded.runtime.has('triage')).toBe(false);
    expect(folded.runtime.get('scan')?.reported).toBe(true);
  });
});

describe('the external results a node links out to', () => {
  const plan = [task('scan'), mapped('triage', 'scan.issues', ['scan']), task('roundup', ['triage'])];
  const linked = (iter: number, taskName: string, links: ExternalLinkRef[]): TaskResult => ({
    ...result(iter, taskName, 'pass'),
    links,
  });
  const pr = (n: number): ExternalLinkRef => ({
    url: `https://github.com/o/r/pull/${n}`,
    provider: 'github',
    kind: 'pull_request',
    label: `#${n}`,
  });
  const issue = (key: string): ExternalLinkRef => ({
    url: `https://example.atlassian.net/browse/${key}`,
    provider: 'jira',
    kind: 'issue',
    label: key,
  });

  it('gives a task every url it reported, once, in the order it reported them', () => {
    const drawn = nodeLinks(plan, [
      linked(0, 'scan', [pr(1)]),
      linked(1, 'scan', [pr(1), issue('ENG-9')]),
    ]);
    expect(drawn.get('scan')).toEqual({ kind: 'links', links: [pr(1), issue('ENG-9')] });
  });

  it('tallies a mapped task by provider and leaves its instances their own', () => {
    const drawn = nodeLinks(plan, [
      linked(0, 'triage[1]', [pr(1)]),
      linked(0, 'triage[2]', [pr(2), issue('ENG-9')]),
    ]);
    expect(drawn.get('triage')).toEqual({
      kind: 'counts',
      counts: [
        { provider: 'github', count: 2 },
        { provider: 'jira', count: 1 },
      ],
    });
    expect(drawn.get('triage[2]')).toEqual({ kind: 'links', links: [pr(2), issue('ENG-9')] });
  });

  it('counts a url two instances both reported once', () => {
    const drawn = nodeLinks(plan, [linked(0, 'triage[1]', [pr(1)]), linked(0, 'triage[2]', [pr(1)])]);
    expect(drawn.get('triage')).toEqual({ kind: 'counts', counts: [{ provider: 'github', count: 1 }] });
  });

  /// A fan-out over nothing reports under the mapped task's own name, and the deck is still a
  /// deck: what it reported is tallied there with whatever its instances reported.
  it('tallies a mapped task that reported under its own name', () => {
    const drawn = nodeLinks(plan, [linked(0, 'triage', [pr(1)]), linked(0, 'triage[2]', [pr(2)])]);
    expect(drawn.get('triage')).toEqual({ kind: 'counts', counts: [{ provider: 'github', count: 2 }] });
  });

  it('draws nothing for a task that reported none, and nothing for a url it may not follow', () => {
    const drawn = nodeLinks(plan, [
      result(0, 'scan', 'pass'),
      linked(0, 'roundup', [{ ...pr(1), url: 'javascript:alert(1)' }]),
    ]);
    expect(drawn.size).toBe(0);
  });

  it('reaches the folded view', () => {
    expect(fold(plan, [linked(0, 'scan', [pr(1)])]).links.get('scan')).toEqual({
      kind: 'links',
      links: [pr(1)],
    });
  });
});

/// A mapped route over scan.items, a fix aligned with it that runs on two of its answers and
/// narrows scan.notes, and a roundup over the fold.
describe('a routed plan a run folds to', () => {
  const route = (name: string, over: string, depends_on: string[]): PlanTask => ({
    ...mapped(name, over, depends_on),
    kind: 'route',
  });
  const plan = [
    task('scan'),
    route('triage', 'scan.items', ['scan']),
    {
      ...mapped('fix', 'scan.items', ['scan', 'triage']),
      when: 'triage.tier in high|low',
      keyed: ['scan.notes'],
    },
    task('roll', ['fix']),
  ];
  const decided = (taskName: string, question: string, labels: [string, number][]) => ({
    task: taskName,
    question,
    labels: labels.map(([label, count]) => ({ label, count })),
  });
  const decisions = [
    decided('triage', 'tier', [
      ['high', 2],
      ['skip', 1],
    ]),
    decided('triage[a]', 'tier', [['high', 1]]),
  ];

  it('reads the wire when back into its route, question and labels', () => {
    expect(whenOf(plan[2] ?? task('none'))).toEqual({
      route: 'triage',
      question: 'tier',
      labels: ['high', 'low'],
    });
    expect(whenOf(task('plain'))).toBeNull();
  });

  it('draws a route as a route, asking the questions it recorded decisions for', () => {
    const folded = fold(plan, [result(0, 'triage[a]', 'pass')], [], [], false, decisions);
    const node = (name: string) => folded.graph.nodes.find((n) => n.name === name);
    expect(node('triage')?.kind).toBe('route');
    expect(node('triage')?.questions).toEqual(['tier']);
    expect(node('triage[a]')?.kind).toBe('route');
    expect(node('fix')?.keyed).toEqual(['scan.notes']);
    expect(node('fix')?.when?.labels).toEqual(['high', 'low']);
    expect(folded.decisions.get('triage')).toEqual([decisions[0]]);
    expect(folded.decisions.get('triage[a]')).toEqual([decisions[1]]);
  });

  it('marks the edge the when reads, the aligned edge, and the narrowed one', () => {
    const [scan, triage, fix, roll] = plan;
    if (scan === undefined || triage === undefined || fix === undefined || roll === undefined) {
      throw new Error('the plan has four tasks');
    }
    expect(planEdge(triage, fix)).toEqual({
      from: 'triage',
      to: 'fix',
      join: 'all',
      required: true,
      when: { route: 'triage', question: 'tier', labels: ['high', 'low'] },
      aligned: true,
      keyed: [],
    });
    expect(planEdge(scan, fix)).toMatchObject({ when: null, aligned: false, keyed: ['notes'] });
    expect(planEdge(scan, triage)).toMatchObject({ aligned: false, keyed: [] });
    expect(planEdge(fix, roll)).toMatchObject({ aligned: false });
  });

  /// Instance k of fix reads instance k of triage, so once both have expanded the edges pair up
  /// and the deck-to-deck edge alone carries the when.
  it('pairs the instances of an aligned edge once both ends have expanded', () => {
    const ran = [
      result(0, 'triage[a]', 'pass'),
      result(0, 'triage[b]', 'pass'),
      result(0, 'fix[a]', 'pass'),
    ];
    const edges = fold(plan, ran).graph.edges;
    const between = (from: string, to: string) => edges.filter((e) => e.from === from && e.to === to);
    expect(between('triage', 'fix')).toHaveLength(1);
    expect(between('triage', 'fix')[0]?.when?.question).toBe('tier');
    expect(between('triage[a]', 'fix[a]')).toEqual([
      { from: 'triage[a]', to: 'fix[a]', join: 'all', required: true, when: null, aligned: true, keyed: [] },
    ]);
    expect(edges.filter((e) => e.from === 'triage[b]' && e.to.startsWith('fix'))).toEqual([]);
    expect(between('triage[a]', 'fix')).toEqual([]);
  });

  it('runs every instance into an aligned consumer that has not expanded yet', () => {
    const edges = fold(plan, [result(0, 'triage[a]', 'pass')]).graph.edges;
    expect(edges.filter((e) => e.to === 'fix' && e.from.startsWith('triage'))).toMatchObject([
      { from: 'triage[a]', aligned: true },
    ]);
  });

  /// scan -> fix is implied by scan -> triage -> fix, but it is the edge fix narrows scan.notes
  /// across, so hiding it would hide the narrowing.
  it('keeps a narrowed edge a longer path already implies', () => {
    const edges = fold(plan).graph.edges;
    expect(edges.find((e) => e.from === 'scan' && e.to === 'fix')?.keyed).toEqual(['notes']);
  });
});

describe('instances a when left out', () => {
  const plan = [task('scan'), mapped('fix', 'scan.items', ['scan'])];

  it('counts not-taken instances apart from failed ones', () => {
    const ran = [
      result(0, 'fix[a]', 'pass'),
      result(0, 'fix[b]', 'not_taken'),
      result(0, 'fix[c]', 'not_taken'),
      result(0, 'fix[d]', 'fail'),
    ];
    expect(fold(plan, ran, [], [{ task: 'fix', items: 4, not_taken: 2 }]).fanout.get('fix')).toEqual({
      items: 4,
      started: 4,
      passed: 1,
      notTaken: 2,
      other: [{ status: 'fail', count: 1 }],
      running: false,
    });
  });

  /// The fold is the engine's own count, so an instance it settled not taken without a row of
  /// its own is still settled, not pending.
  it('takes the fold count when instance rows are missing', () => {
    const ran = [result(0, 'fix[a]', 'pass'), result(0, 'fix[b]', 'not_taken')];
    const state = fold(plan, ran, [], [{ task: 'fix', items: 5, not_taken: 4 }]).fanout.get('fix');
    expect(state).toMatchObject({ started: 5, passed: 1, notTaken: 4 });
  });
});
