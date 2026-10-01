import { describe, expect, it } from 'vitest';
import { barShare, blockedLine, formatSecs, meldRow, runGridView } from './runGrid';
import type { PlanTask, TaskResult } from './taskGraph';

const task = (name: string, depends_on: string[] = []): PlanTask => ({
  name,
  kind: 'command',
  depends_on,
  session: '',
  needs: 'all',
  required: true,
  over: '',
  max_fanout: 0,
});

const result = (
  iter: number,
  taskName: string,
  status: string,
  extra: Partial<TaskResult> = {},
): TaskResult => ({
  iter,
  task: taskName,
  status,
  note: '',
  cost_usd: null,
  secs: null,
  blocked: null,
  links: [],
  ...extra,
});

describe('blocked cells', () => {
  it('carries the typed reason and names the task it points at', () => {
    const grid = runGridView(
      [task('brief'), task('deliver', ['brief'])],
      [
        result(0, 'brief', 'fail'),
        result(0, 'deliver', 'blocked', {
          note: 'required task brief failed',
          blocked: { reason: 'required_task_failed', task: 'brief' },
        }),
      ],
    );
    const deliver = grid.rows.find((r) => r.task === 'deliver');
    expect(deliver?.cells[0]?.blocked).toEqual({ reason: 'required_task_failed', task: 'brief' });
    expect(blockedLine(deliver?.cells[0]?.blocked ?? null)).toBe('blocked: required_task_failed (brief)');
    expect(blockedLine({ reason: 'budget_ceiling', task: null })).toBe('blocked: budget_ceiling');
    expect(blockedLine(null)).toBeNull();
    expect(grid.rows.find((r) => r.task === 'brief')?.cells[0]?.blocked).toBeNull();
  });
});

describe('columns', () => {
  it('is one column per iteration that reported, in order', () => {
    const grid = runGridView(
      [task('scan')],
      [result(2, 'scan', 'pass'), result(0, 'scan', 'fail'), result(1, 'scan', 'fail')],
    );
    expect(grid.iters).toEqual([0, 1, 2]);
  });

  it('leaves a gap where a task did not report', () => {
    const grid = runGridView(
      [task('scan'), task('fix', ['scan'])],
      [result(0, 'scan', 'pass'), result(1, 'scan', 'pass'), result(1, 'fix', 'pass')],
    );
    const fix = grid.rows.find((row) => row.task === 'fix');
    expect(fix?.cells.map((cell) => cell?.status ?? null)).toEqual([null, 'pass']);
  });

  it('keeps the last report when a task reports twice in one iteration', () => {
    const grid = runGridView([task('scan')], [result(0, 'scan', 'fail'), result(0, 'scan', 'pass')]);
    expect(grid.rows[0]?.cells[0]?.status).toBe('pass');
    expect(grid.rows[0]?.attempts).toBe(2);
  });

  it('has no columns when nothing has reported', () => {
    const grid = runGridView([task('scan')], []);
    expect(grid.iters).toEqual([]);
    expect(grid.rows).toHaveLength(1);
    expect(grid.rows[0]?.tone).toBe('none');
  });
});

describe('row order', () => {
  it('puts dependencies before the tasks that wait on them', () => {
    const grid = runGridView(
      [task('report', ['fix']), task('fix', ['scan']), task('scan')],
      [result(0, 'scan', 'pass')],
    );
    expect(grid.rows.map((row) => row.task)).toEqual(['scan', 'fix', 'report']);
  });

  it('breaks ties by declaration order', () => {
    const grid = runGridView([task('b'), task('a'), task('c')], []);
    expect(grid.rows.map((row) => row.task)).toEqual(['b', 'a', 'c']);
  });

  it('still emits every task when the plan holds a cycle', () => {
    const grid = runGridView([task('a', ['b']), task('b', ['a'])], []);
    expect(grid.rows.map((row) => row.task)).toEqual(['a', 'b']);
  });

  it('hangs mapped instances under the task they were mapped from', () => {
    const grid = runGridView(
      [task('triage'), task('report', ['triage'])],
      [result(0, 'triage[1]', 'pass'), result(0, 'triage[2]', 'fail'), result(0, 'report', 'pass')],
    );
    expect(grid.rows.map((row) => row.task)).toEqual([
      'triage',
      'triage[1]',
      'triage[2]',
      'report',
    ]);
    expect(grid.rows.map((row) => row.mapped)).toEqual([false, true, true, false]);
  });

  it('keeps a result the plan never declared rather than dropping it', () => {
    const grid = runGridView([task('scan')], [result(0, 'ghost', 'pass')]);
    expect(grid.rows.map((row) => row.task)).toEqual(['scan', 'ghost']);
    expect(grid.rows[1]?.mapped).toBe(false);
  });

  it('does not treat a bracket on an undeclared prefix as a mapped instance', () => {
    const grid = runGridView([task('scan')], [result(0, 'other[1]', 'pass')]);
    expect(grid.rows.find((row) => row.task === 'other[1]')?.mapped).toBe(false);
  });
});

describe('totals', () => {
  it('sums duration and cost across attempts', () => {
    const grid = runGridView(
      [task('scan')],
      [
        result(0, 'scan', 'fail', { secs: 4, cost_usd: 0.25 }),
        result(1, 'scan', 'pass', { secs: 6, cost_usd: 0.75 }),
      ],
    );
    expect(grid.rows[0]?.secs).toBe(10);
    expect(grid.rows[0]?.costUsd).toBe(1);
    expect(grid.rows[0]?.attempts).toBe(2);
  });

  it('leaves a total null when no attempt carried a figure', () => {
    const grid = runGridView([task('scan')], [result(0, 'scan', 'pass')]);
    expect(grid.rows[0]?.secs).toBeNull();
    expect(grid.rows[0]?.costUsd).toBeNull();
    expect(grid.maxSecs).toBeNull();
  });

  it('treats a run where every attempt reports 0s as untimed', () => {
    const grid = runGridView(
      [task('scan'), task('fix')],
      [
        result(0, 'scan', 'pass', { secs: 0 }),
        result(0, 'fix', 'pass', { secs: 0 }),
        result(1, 'fix', 'pass', { secs: 0 }),
      ],
    );
    expect(grid.maxSecs).toBeNull();
    expect(grid.rows.map((row) => row.secs)).toEqual([null, null]);
    expect(grid.rows[1]?.cells.map((cell) => cell?.secs)).toEqual([null, null]);
  });

  it('keeps a 0s attempt when another attempt in the run was timed', () => {
    const grid = runGridView(
      [task('scan'), task('fix')],
      [result(0, 'scan', 'pass', { secs: 0 }), result(0, 'fix', 'pass', { secs: 8 })],
    );
    expect(grid.maxSecs).toBe(8);
    expect(grid.rows[0]?.secs).toBe(0);
  });

  it('takes the tone of the latest attempt, not the first', () => {
    const grid = runGridView(
      [task('scan')],
      [result(1, 'scan', 'pass'), result(0, 'scan', 'fail')],
    );
    expect(grid.rows[0]?.tone).toBe('pass');
  });

  it('scales bars against the widest row', () => {
    const grid = runGridView(
      [task('scan'), task('fix')],
      [result(0, 'scan', 'pass', { secs: 2 }), result(0, 'fix', 'pass', { secs: 8 })],
    );
    expect(grid.maxSecs).toBe(8);
    expect(barShare(2, grid.maxSecs)).toBeCloseTo(0.25);
    expect(barShare(8, grid.maxSecs)).toBe(1);
  });
});

describe('counts', () => {
  it('tallies attempts by status', () => {
    const grid = runGridView(
      [task('scan'), task('fix')],
      [
        result(0, 'scan', 'pass'),
        result(0, 'fix', 'fail'),
        result(1, 'fix', 'pass'),
        result(1, 'scan', 'skipped'),
      ],
    );
    expect(grid.counts).toEqual([
      { status: 'pass', tone: 'pass', count: 2 },
      { status: 'fail', tone: 'fail', count: 1 },
      { status: 'skipped', tone: 'none', count: 1 },
    ]);
  });
});

describe('formatting', () => {
  it('writes durations at the scale they are read', () => {
    expect(formatSecs(null)).toBe('—');
    expect(formatSecs(4.25)).toBe('4.3s');
    expect(formatSecs(42.4)).toBe('42s');
    expect(formatSecs(95)).toBe('1m 35s');
    expect(formatSecs(7260)).toBe('2h 1m');
  });

  it('draws a visible bar for a row far below the widest', () => {
    expect(barShare(0.001, 1000)).toBe(0.02);
    expect(barShare(null, 10)).toBe(0);
    expect(barShare(5, null)).toBe(0);
    expect(barShare(5, 0)).toBe(0);
  });
});

describe('reported links', () => {
  it('ride the attempt that reported them, retry and all', () => {
    const pr = {
      url: 'https://github.com/o/r/pull/9',
      provider: 'github',
      kind: 'pull_request',
      label: '#9',
    };
    const grid = runGridView(
      [task('deliver')],
      [
        result(0, 'deliver', 'fail'),
        result(1, 'deliver', 'pass', { links: [pr] }),
      ],
    );
    expect(grid.rows[0].cells[0]?.links).toEqual([]);
    expect(grid.rows[0].cells[1]?.links).toEqual([pr]);
  });
});

describe('what a row ran on', () => {
  const agent = (model: string, effort = 'low') => ({ harness: 'claude', model, effort });

  it('reads off the attempts, and a command task ran on nothing', () => {
    const grid = runGridView(
      [task('analyze'), task('build')],
      [
        result(0, 'analyze', 'pass', { agent: agent('glm-5.3') }),
        result(0, 'build', 'pass'),
      ],
    );
    expect(grid.rows[0].agent).toEqual({ harness: 'claude', model: 'glm-5.3', effort: 'low' });
    expect(grid.rows[1].agent).toBeNull();
  });

  it('names a mapped parent only what every instance agreed on', () => {
    const grid = runGridView(
      [task('analyze')],
      [
        result(0, 'analyze[a]', 'pass', { agent: agent('glm-5.3') }),
        result(0, 'analyze[b]', 'pass', { agent: agent('glm-5.3', 'high') }),
      ],
    );
    const parent = grid.rows[0];
    expect(parent.task).toBe('analyze');
    expect(parent.agent).toEqual({ harness: 'claude', model: 'glm-5.3', effort: null });
  });

  it('leaves a mapped parent with no instance that ran an agent running nothing', () => {
    const grid = runGridView(
      [task('fan')],
      [result(0, 'fan[a]', 'pass'), result(0, 'fan[b]', 'pass')],
    );
    expect(grid.rows[0].agent).toBeNull();
  });

  it('takes the attempts of a retried task together, not its last one alone', () => {
    const grid = runGridView(
      [task('analyze')],
      [
        result(0, 'analyze', 'fail', { agent: agent('glm-5.3') }),
        result(1, 'analyze', 'pass', { agent: agent('glm-5.4') }),
      ],
    );
    expect(grid.rows[0].agent).toEqual({ harness: 'claude', model: null, effort: 'low' });
  });
});

describe('melding a row', () => {
  const run = (statuses: (string | null)[]) => {
    const iters = statuses.map((_, i) => i);
    const results = statuses
      .map((status, i) => (status === null ? null : result(i, 'work', status)))
      .filter((r): r is TaskResult => r !== null);
    const grid = runGridView([task('work')], results);
    expect(grid.iters).toEqual(iters.filter((i) => statuses[i] !== null));
    return meldRow(grid.rows[0].cells, grid.iters);
  };

  it('joins adjacent iterations of the same status and breaks on every change', () => {
    const melded = run(['pass', 'pass', 'pass', 'pass', 'pass', 'fail', 'pass', 'pass', 'pass']);
    expect(melded.map((s) => [s.cells[0]?.status, s.span])).toEqual([
      ['pass', 5],
      ['fail', 1],
      ['pass', 3],
    ]);
    expect(melded[0].iters).toEqual([0, 1, 2, 3, 4]);
    expect(melded[2].cells.map((c) => c.iter)).toEqual([6, 7, 8]);
  });

  it('never joins statuses that merely look alike', () => {
    const melded = run(['fail', 'transport', 'blocked', 'truncated']);
    expect(melded.map((s) => s.span)).toEqual([1, 1, 1, 1]);
  });

  it('melds a stretch a task sat out, and keeps it apart from what it reported', () => {
    const grid = runGridView(
      [task('work'), task('other')],
      [
        result(0, 'other', 'pass'),
        result(1, 'other', 'pass'),
        result(2, 'work', 'pass'),
        result(3, 'work', 'pass'),
      ],
    );
    const work = grid.rows[0];
    expect(work.task).toBe('work');
    const melded = meldRow(work.cells, grid.iters);
    expect(melded.map((s) => [s.cells.length, s.span])).toEqual([
      [0, 2],
      [2, 2],
    ]);
    expect(melded[0].iters).toEqual([0, 1]);
  });

  it('spans every column exactly once', () => {
    const melded = run(['pass', 'pass', 'fail', 'fail', 'fail', 'pass']);
    expect(melded.reduce((wide, s) => wide + s.span, 0)).toBe(6);
    expect(melded.flatMap((s) => s.iters)).toEqual([0, 1, 2, 3, 4, 5]);
  });

  it('leaves an attempt that said something of its own in its own cell', () => {
    const grid = runGridView(
      [task('work')],
      [
        result(0, 'work', 'fail'),
        result(1, 'work', 'fail', { note: 'exit 7: the endpoint refused' }),
        result(2, 'work', 'fail'),
        result(3, 'work', 'fail'),
      ],
    );
    const melded = meldRow(grid.rows[0].cells, grid.iters);
    expect(melded.map((s) => [s.iters, s.span])).toEqual([
      [[0], 1],
      [[1], 1],
      [[2, 3], 2],
    ]);
  });

  it('keeps a blocked attempt and one that reported a link out of a block', () => {
    const grid = runGridView(
      [task('work')],
      [
        result(0, 'work', 'blocked', { blocked: { reason: 'budget_ceiling', task: null } }),
        result(1, 'work', 'blocked', { blocked: { reason: 'budget_ceiling', task: null } }),
        result(2, 'work', 'pass', {
          links: [{ url: 'https://github.com/o/r/pull/9', provider: 'github', kind: 'pull_request', label: '#9' }],
        }),
        result(3, 'work', 'pass'),
      ],
    );
    expect(meldRow(grid.rows[0].cells, grid.iters).map((s) => s.span)).toEqual([1, 1, 1, 1]);
  });

  it('has nothing to meld in an empty row', () => {
    expect(meldRow([], [])).toEqual([]);
  });
});
