import { describe, expect, it } from 'vitest';
import {
  badgeFor,
  detailRows,
  metaFor,
  runsLine,
  runtimeLine,
  runtimeRows,
  sourceFor,
} from './workflowGraphCard';
import type { TaskRuntime } from './taskGraph';
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
