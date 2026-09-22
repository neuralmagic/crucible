import { describe, expect, it } from 'vitest';
import { barShare, blockedLine, formatSecs, runGridView } from './runGrid';
import type { PlanTask, TaskResult } from './taskGraph';

const task = (name: string, depends_on: string[] = []): PlanTask => ({
  name,
  kind: 'command',
  depends_on,
  session: '',
  needs: 'all',
  required: true,
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
