import { describe, expect, it } from 'vitest';
import {
  latestResults,
  runGraphView,
  targetLabel,
  toneOf,
  UNDECLARED_OUTPUTS,
  type GraphOutput,
  type OutputTarget,
  type PlanTask,
  type TaskResult,
} from './taskGraph';

const task = (name: string, depends_on: string[] = [], session = ''): PlanTask => ({
  name,
  kind: 'command',
  depends_on,
  session,
  needs: 'all',
  required: true,
});

const result = (iter: number, taskName: string, status: string): TaskResult => ({
  iter,
  task: taskName,
  status,
  note: '',
  cost_usd: null,
  secs: null,
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
    const folded = runGraphView(tasks, results);
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
      { from: 'propose-a', to: 'pick', join: 'all', required: true },
      { from: 'propose-b', to: 'pick', join: 'all', required: true },
      { from: 'pick', to: 'measure', join: 'all', required: true },
    ]);
    expect(g.runtime.get('propose-a')?.tone).toBe('pass');
    expect(g.runtime.get('measure')?.tone).toBe('fail');
    expect(g.runtime.has('pick')).toBe(false);
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
    });
  });

  it('links downstream work from the leaves of a mapped task subgraph', () => {
    const g = view(
      [task('scan'), task('triage', ['scan']), task('roundup', ['scan', 'triage'])],
      [result(0, 'triage[one]', 'pass'), result(0, 'triage[two]', 'pass')],
    );
    expect(g.edges).toEqual([
      { from: 'scan', to: 'triage', join: 'all', required: true },
      { from: 'triage[one]', to: 'roundup', join: 'all', required: true },
      { from: 'triage[two]', to: 'roundup', join: 'all', required: true },
      { from: 'triage', to: 'triage[one]', join: 'all', required: true },
      { from: 'triage', to: 'triage[two]', join: 'all', required: true },
    ]);
    expect(g.edges).not.toContainEqual({
      from: 'triage',
      to: 'roundup',
      join: 'all',
      required: true,
    });
    expect(g.edges).not.toContainEqual({
      from: 'scan',
      to: 'roundup',
      join: 'all',
      required: true,
    });
  });

  it('links an implicit epilogue report after the ordinary graph sinks', () => {
    const g = view([
      task('scan'),
      task('card', ['scan']),
      { ...task('publish-report'), kind: 'report' },
    ]);
    expect(g.edges).toEqual([
      { from: 'scan', to: 'card', join: 'all', required: true },
      { from: 'card', to: 'publish-report', join: 'all', required: true },
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
    expect(g.edges).toEqual([{ from: 'propose', to: 'measure', join: 'all', required: true }]);
  });

  it('drops edges to unknown tasks and keeps a cycle the wire carried', () => {
    const g = view([task('a', ['ghost', 'b']), task('b', ['a'])]);
    expect(g.edges).toEqual([
      { from: 'b', to: 'a', join: 'all', required: true },
      { from: 'a', to: 'b', join: 'all', required: true },
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
    const folded = runGraphView(plan, [], [
      output('draft-pr', 1, address('owner/repo'), 'publish'),
      output('gpu-capture', 2, null, null),
    ]);
    const drawn = [...folded.outputs.entries()];
    expect(drawn.map(([, o]) => o.kind)).toEqual(['draft-pr', 'gpu-capture']);
    expect(drawn.every(([, o]) => !o.undeclared)).toBe(true);

    const [pr, capture] = drawn.map(([name]) => name);
    expect(folded.graph.nodes.map((n) => n.name)).toContain(pr);
    expect(folded.graph.edges).toContainEqual({ from: 'publish', to: pr, join: 'all', required: true });
    // `publish` is the plan's only sink, so the unattached bound lands there too.
    expect(folded.graph.edges).toContainEqual({ from: 'publish', to: capture, join: 'all', required: true });
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
    const folded = runGraphView(plan, [], [
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
    const folded = runGraphView(plan, [], [
      output('draft-pr', 2, null, null, 'engine-default'),
      output('gpu-capture', 100, null, null, 'engine-default'),
    ]);
    expect(folded.outputs.size).toBe(0);
    expect(folded.graph.nodes.map((n) => n.name)).toEqual(['scan', 'work', 'publish']);
    expect(folded.engineDefaults).toHaveLength(2);
  });

  /// Two bounds of the same kind are two nodes, not one overwriting the other.
  it('keeps one node per bound even when the kinds repeat', () => {
    const folded = runGraphView(plan, [], [
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
    const folded = runGraphView(
      [task('scan'), task('measure', ['scan']), task('notify', ['scan'])],
      [],
      [output('chat-message', 4, address('operator-channel'), null)],
    );
    const [name] = [...folded.outputs.keys()];
    expect(folded.graph.edges).toContainEqual({ from: 'measure', to: name, join: 'all', required: true });
    expect(folded.graph.edges).toContainEqual({ from: 'notify', to: name, join: 'all', required: true });
    expect(folded.graph.edges).not.toContainEqual({ from: 'scan', to: name, join: 'all', required: true });
  });

  /// A bound naming a task this plan does not carry still renders, off the sink. An output the
  /// graph dropped would be showing less than the pack disclosed.
  it('falls back to the sink when the named producer is not in the plan', () => {
    const folded = runGraphView(plan, [], [output('deploy', 1, address('cluster'), 'deploy_candidate')]);
    const [name] = [...folded.outputs.keys()];
    expect(folded.graph.edges).toContainEqual({ from: 'publish', to: name, join: 'all', required: true });
  });

  /// Absent-legacy: the revision stored no exposure. That is not a pack that writes nothing, so it
  /// gets its own marker node rather than silence.
  it('draws an explicit undeclared marker for a revision that stored no exposure', () => {
    const folded = runGraphView(plan, [], null);
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
    });
  });

  /// A pack that declares an empty output set is a pack that declared: no marker, no nodes.
  it('draws nothing extra for a pack that declares no outputs', () => {
    const folded = runGraphView(plan, [], []);
    expect(folded.outputs.size).toBe(0);
    expect(folded.graph.nodes.map((n) => n.name)).toEqual(['scan', 'work', 'publish']);
  });
});
