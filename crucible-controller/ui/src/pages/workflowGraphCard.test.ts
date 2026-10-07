import { describe, expect, it } from 'vitest';
import {
  badgeFor,
  decisionLines,
  decisionRows,
  detailRows,
  fanoutLine,
  fanoutRows,
  metaFor,
  runsLine,
  runtimeLine,
  runtimeRows,
  sourceFor,
} from './workflowGraphCard';
import type { FanOutState, TaskRuntime } from './taskGraph';
import type { WorkflowGraphNode } from './workflowGraphLayout';

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

function valueOf(node: WorkflowGraphNode, label: string): string | undefined {
  return detailRows(node).find((row) => row.label === label)?.value;
}

describe('the badge', () => {
  it('names what runs the task, and does not refuse a kind it has not met', () => {
    expect(badgeFor(task('a', { kind: 'agent' }))).toBe('AGENT');
    expect(badgeFor(task('a', { kind: 'command' }))).toBe('CMD');
    expect(badgeFor(task('a', { kind: 'engine' }))).toBe('ENGINE');
    expect(badgeFor(task('a', { kind: 'other' }))).toBe('TASK');
  });

  it('calls a mapped task a map, whatever its instances run', () => {
    const over = { over_task: 'fan', over_field: 'idea', max_fanout: null };
    expect(badgeFor(task('a', { kind: 'agent', fanout: over }))).toBe('MAP');
    expect(badgeFor(task('a', { kind: 'command', fanout: over }))).toBe('MAP');
    expect(badgeFor(task('a[one]', { kind: 'agent' }))).toBe('AGENT');
  });
});

describe('the line under the name', () => {
  it('maps fan-out onto the node that maps over it, capped or not', () => {
    expect(
      metaFor(task('w', { fanout: { over_task: 'fan', over_field: 'idea', max_fanout: 4 } }))
    ).toBe('over fan.idea ≤4');
    expect(
      metaFor(task('w', { fanout: { over_task: 'fan', over_field: 'idea', max_fanout: null } }))
    ).toBe('over fan.idea');
  });

  it('falls back to where a task runs, then to the session it is bound to, then to nothing', () => {
    expect(metaFor(task('a', { isolation: 'worktree' }))).toBe('in worktree');
    expect(metaFor(task('a', { session: 'solver' }))).toBe('⟳ solver');
    expect(metaFor(task('a', { isolation: 'worktree', session: 'solver' }))).toBe('in worktree');
    expect(metaFor(task('a', { emits: ['spec'] }))).toBeNull();
    expect(metaFor(task('a'))).toBeNull();
  });

  /// Fan-out wins the line: it is what makes one task many.
  it('says what a mapped task maps over rather than where it runs', () => {
    const node = task('w', {
      isolation: 'worktree',
      fanout: { over_task: 'fan', over_field: 'idea', max_fanout: 2 },
    });
    expect(metaFor(node)).toBe('over fan.idea ≤2');
  });
});

describe('what the card says a task runs', () => {
  it('is the knobs an agent runs with, whichever of them the pack pinned', () => {
    expect(runsLine(task('a', { kind: 'agent', model: 'opus', effort: 'high' }))).toBe(
      'opus · high'
    );
    expect(runsLine(task('a', { kind: 'agent', model: 'opus' }))).toBe('opus');
    expect(runsLine(task('a', { kind: 'agent', effort: 'high' }))).toBe('high');
    expect(runsLine(task('a', { kind: 'agent' }))).toBe('pack defaults');
  });

  it('is the command line itself for anything else, and nothing when there is none', () => {
    expect(runsLine(task('a', { command: './run.sh --one' }))).toBe('./run.sh --one');
    expect(runsLine(task('a'))).toBeNull();
    expect(runsLine(task('a', { kind: 'engine', command: '' }))).toBeNull();
  });

  /// A command's own model field, if a newer engine ever sends one, is not the card's business.
  it('does not read agent knobs off a command', () => {
    expect(runsLine(task('a', { kind: 'command', model: 'opus', command: 'x' }))).toBe('x');
  });
});

describe('the panel rows', () => {
  it('always states how the task is scheduled, whatever the wire left out', () => {
    const rows = detailRows(task('a'));
    expect(rows.map((row) => row.label)).toEqual([
      'kind',
      'required',
      'needs',
      'join',
      'isolation',
    ]);
    expect(valueOf(task('a'), 'isolation')).toBe('shared workspace');
    expect(valueOf(task('a', { isolation: 'worktree' }), 'isolation')).toBe('worktree');
    expect(valueOf(task('a', { required: false }), 'required')).toBe('no — advisory');
    expect(valueOf(task('a'), 'required')).toBe('yes');
  });

  it('adds the knobs the turn runs with only when the document carries them', () => {
    const node = task('a', {
      kind: 'agent',
      session: 'survey',
      harness: 'claude',
      model: 'opus',
      effort: 'high',
    });
    expect(valueOf(node, 'session')).toBe('survey');
    expect(valueOf(node, 'harness')).toBe('claude');
    expect(valueOf(node, 'model')).toBe('opus');
    expect(valueOf(node, 'effort')).toBe('high');
    expect(detailRows(task('a')).map((row) => row.label)).not.toContain('session');
  });

  it('spells out the fan-out a card can only abbreviate', () => {
    const capped = task('w', { fanout: { over_task: 'fan', over_field: 'idea', max_fanout: 3 } });
    expect(valueOf(capped, 'maps over')).toBe('fan.idea');
    expect(valueOf(capped, 'max instances')).toBe('3');

    const uncapped = task('w', {
      fanout: { over_task: 'fan', over_field: 'idea', max_fanout: null },
    });
    expect(valueOf(uncapped, 'max instances')).toBe('uncapped');
    expect(detailRows(task('a')).map((row) => row.label)).not.toContain('maps over');
  });

  it('lists what the task emits, fields and files apart', () => {
    const node = task('a', { emits: ['spec', 'notes'], emits_files: ['out.json'] });
    expect(valueOf(node, 'emits')).toBe('spec notes');
    expect(valueOf(node, 'emits files')).toBe('out.json');
    expect(detailRows(task('a')).map((row) => row.label)).not.toContain('emits');
  });
});

describe('the source the panel shows', () => {
  it('is the prompt for an agent and the command for anything else', () => {
    expect(sourceFor(task('a', { kind: 'agent', prompt: 'READ THE PAPER' }))).toEqual({
      label: 'prompt',
      body: 'READ THE PAPER',
    });
    expect(sourceFor(task('a', { command: './run.sh' }))).toEqual({
      label: 'runs',
      body: './run.sh',
    });
    expect(sourceFor(task('a'))).toBeNull();
  });
});

function ran(overrides: Partial<TaskRuntime> = {}): TaskRuntime {
  return {
    tone: 'pass',
    status: 'pass',
    reported: true,
    latestIter: 2,
    attempts: 1,
    costUsd: null,
    secs: null,
    note: '',
    ...overrides,
  };
}

function rowValue(runtime: TaskRuntime, label: string): string | undefined {
  return runtimeRows(runtime).find((row) => row.label === label)?.value;
}

describe('what a run adds to the card', () => {
  it('states the iteration, and the rest only when there is something to state', () => {
    expect(runtimeLine(ran())).toBe('iter 2');
    expect(runtimeLine(ran({ attempts: 3, costUsd: 0.75, secs: 42 }))).toBe(
      'iter 2 · 3 attempts · $0.75 · 42s'
    );
  });

  it('reads a long task in minutes', () => {
    expect(runtimeLine(ran({ secs: 90 }))).toBe('iter 2 · 1m 30s');
    expect(runtimeLine(ran({ secs: 59.6 }))).toBe('iter 2 · 60s');
  });

  it('spells the run state out in the panel, unstated figures included', () => {
    const runtime = ran({ status: 'fail', tone: 'fail', attempts: 2, costUsd: 1.5 });
    expect(rowValue(runtime, 'status')).toBe('fail');
    expect(rowValue(runtime, 'attempts')).toBe('2');
    expect(rowValue(runtime, 'iteration')).toBe('2');
    expect(rowValue(runtime, 'cost')).toBe('$1.50');
    expect(rowValue(runtime, 'took')).toBe('—');
  });
});

describe('what a run made of a mapped task', () => {
  const spread = (overrides: Partial<FanOutState> = {}): FanOutState => ({
    items: 4,
    started: 4,
    passed: 4,
    notTaken: 0,
    other: [],
    running: false,
    ...overrides,
  });
  const failed = (count: number) => [{ status: 'fail', count }];

  it('reads what passed against what went in', () => {
    expect(fanoutLine(spread())).toBe('4 · 4 passed');
    expect(fanoutLine(spread({ passed: 3, other: failed(1) }))).toBe('4 · 3 passed · 1 failed');
  });

  /// The case the panel exists for: 20 tickets went in, 4 needed an agent, 16 never started.
  it('says how many of the items never started', () => {
    expect(fanoutLine(spread({ items: 20, started: 4, passed: 4 }))).toBe(
      '20 · 4 passed · 16 never started'
    );
  });

  /// A run still going may yet reach them, so they are not a verdict yet.
  it('says the items it has not reached are pending while the run is going', () => {
    expect(fanoutLine(spread({ items: 20, started: 4, passed: 4, running: true }))).toBe(
      '20 · 4 passed · 16 pending'
    );
  });

  /// An instance that settled without running is neither a pass nor a failure, and on a triage
  /// fan-out it is most of the run.
  it('names every status its instances reported in', () => {
    const state = spread({
      items: 20,
      started: 20,
      passed: 2,
      other: [
        { status: 'skipped', count: 17 },
        { status: 'blocked', count: 1 },
      ],
    });
    expect(fanoutLine(state)).toBe('20 · 2 passed · 17 skipped · 1 blocked');
  });

  it('says zero for a fan-out over nothing rather than going quiet', () => {
    expect(fanoutLine(spread({ items: 0, started: 0, passed: 0 }))).toBe('0 items');
  });

  /// A run with no stored session knows what started, not what was asked for.
  it('reports what started when the item count is unknown', () => {
    expect(fanoutLine(spread({ items: null, passed: 3, other: failed(1) }))).toBe(
      '4 started · 3 passed · 1 failed'
    );
  });

  /// A task refanned across iterations starts more instances than the last fan-out asked for,
  /// and "5 of 3 passed" is not a count anyone can read.
  it('drops a width the run has already run past', () => {
    expect(fanoutLine(spread({ items: 3, started: 5, passed: 5 }))).toBe('5 started · 5 passed');
    expect(fanoutLine(spread({ items: 0, started: 2, passed: 2 }))).toBe('2 started · 2 passed');
  });

  it('spells the counts out in the panel, the unknown one included', () => {
    const value = (state: FanOutState, label: string) =>
      fanoutRows(state).find((row) => row.label === label)?.value;
    expect(value(spread({ items: 20, started: 4, passed: 4 }), 'items')).toBe('20');
    expect(value(spread({ items: 20, started: 4, passed: 4 }), 'started')).toBe('4');
    expect(value(spread({ other: failed(2) }), 'fail')).toBe('2');
    expect(value(spread({ items: null }), 'items')).toBe('—');
  });
});

describe('a task the run never reported on', () => {
  const never: TaskRuntime = {
    tone: 'none',
    status: 'never ran',
    reported: false,
    latestIter: 0,
    attempts: 0,
    costUsd: null,
    secs: null,
    note: '',
  };

  /// An iteration and a zero cost would be inventions: nothing reported, so nothing is stated.
  it('states its status and no figures at all', () => {
    expect(runtimeLine(never)).toBeNull();
    expect(runtimeRows(never)).toEqual([{ label: 'status', value: 'never ran' }]);
  });
});

describe('a route', () => {
  const route = (overrides: Partial<WorkflowGraphNode> = {}) =>
    task('triage', { kind: 'route', questions: ['scope', 'tier'], ...overrides });
  const over = { over_task: 'scan', over_field: 'items', max_fanout: 120 };

  it('is badged as a decision whether or not it is mapped', () => {
    expect(badgeFor(route())).toBe('DECIDE');
    expect(badgeFor(route({ fanout: over }))).toBe('DECIDE');
  });

  it('says what it maps over like any mapped task', () => {
    expect(metaFor(route({ fanout: over }))).toBe('over scan.items ≤120');
  });

  it('asks its questions until it has answers', () => {
    expect(runsLine(route())).toBe('scope? tier?');
    expect(runsLine(route({ questions: [] }))).toBeNull();
  });

  it('reads one decision as its label and a mapped tally most frequent first', () => {
    const one = { task: 'triage', question: 'tier', labels: [{ label: 'high', count: 1 }] };
    const tally = {
      task: 'triage',
      question: 'tier',
      labels: [
        { label: 'high', count: 80 },
        { label: 'low', count: 2 },
        { label: 'skip', count: 38 },
      ],
    };
    expect(decisionLines([one])).toEqual(['tier: high']);
    expect(decisionLines([tally])).toEqual(['tier: high 80 · skip 38 · low 2']);
    expect(decisionRows([tally])).toEqual([{ label: 'tier', value: 'high 80 · skip 38 · low 2' }]);
  });

  it('lists its questions in the panel', () => {
    const rows = detailRows(route());
    expect(rows.find((row) => row.label === 'questions')?.value).toBe('scope tier');
  });
});

describe('a task that reads a route or narrows a field', () => {
  it('lists its when and the references it narrows', () => {
    const rows = detailRows(
      task('fix', {
        when: { route: 'triage', question: 'tier', labels: ['high', 'low'] },
        keyed: ['scan.notes', 'scan.owners'],
      })
    );
    const value = (label: string) => rows.find((row) => row.label === label)?.value;
    expect(value('when')).toBe('triage.tier in high|low');
    expect(value('narrows')).toBe('scan.notes scan.owners');
  });

  it('leaves both rows out when it does neither', () => {
    const labels = detailRows(task('plain')).map((row) => row.label);
    expect(labels).not.toContain('when');
    expect(labels).not.toContain('narrows');
    expect(labels).not.toContain('questions');
  });
});

describe('a fan-out a when partitioned', () => {
  const state: FanOutState = {
    items: 120,
    started: 120,
    passed: 80,
    notTaken: 38,
    other: [{ status: 'fail', count: 2 }],
    running: false,
  };

  it('counts not-taken instances apart from failed ones', () => {
    expect(fanoutLine(state)).toBe('120 · 80 passed · 38 not taken · 2 failed');
  });

  it('spells the not-taken count out in the panel', () => {
    expect(fanoutRows(state).find((row) => row.label === 'not taken')?.value).toBe('38');
  });
});
