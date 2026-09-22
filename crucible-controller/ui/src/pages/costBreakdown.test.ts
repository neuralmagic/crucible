import { describe, expect, it } from 'vitest';
import { buildCostBreakdown, type TaskCostInput } from './costBreakdown';

const r = (iter: number, task: string, cost: number | null | undefined): TaskCostInput => ({
  iter,
  task,
  cost_usd: cost,
});

describe('buildCostBreakdown', () => {
  it('is null when no result carries a nonzero cost', () => {
    expect(buildCostBreakdown([], 5)).toBeNull();
    expect(buildCostBreakdown([r(1, 'propose', null), r(1, 'measure', 0)], 5)).toBeNull();
    expect(buildCostBreakdown([r(1, 'propose', undefined)], null)).toBeNull();
  });

  it('sums per task across iterations and sorts most expensive first', () => {
    const b = buildCostBreakdown(
      [r(1, 'measure', 0.5), r(1, 'propose', 2.0), r(2, 'propose', 1.5), r(2, 'measure', 0.25)],
      null,
    );
    expect(b).not.toBeNull();
    if (b === null) return;
    expect(b.rows.map((x) => x.task)).toEqual(['propose', 'measure']);
    expect(b.rows[0].total).toBeCloseTo(3.5);
    expect(b.rows[1].total).toBeCloseTo(0.75);
    expect(b.rows[0].byIter.get(2)).toBeCloseTo(1.5);
    expect(b.iters).toEqual([1, 2]);
    expect(b.tasksTotal).toBeCloseTo(4.25);
    expect(b.runTotal).toBeNull();
    expect(b.outsideTasks).toBeNull();
  });

  it('breaks total ties by task name', () => {
    const b = buildCostBreakdown([r(1, 'zeta', 1.0), r(1, 'alpha', 1.0)], null);
    expect(b?.rows.map((x) => x.task)).toEqual(['alpha', 'zeta']);
  });

  it('collapses repeated task+iter attempts into one cell', () => {
    const b = buildCostBreakdown([r(1, 'propose', 1.0), r(1, 'propose', 0.5)], null);
    expect(b?.rows[0].byIter.get(1)).toBeCloseTo(1.5);
    expect(b?.tasksTotal).toBeCloseTo(1.5);
  });

  it('footnotes spend outside graded tasks when the run total is larger', () => {
    const b = buildCostBreakdown([r(1, 'propose', 3.0)], 4.2);
    expect(b?.runTotal).toBe(4.2);
    expect(b?.outsideTasks).toBeCloseTo(1.2);
  });

  it('skips the footnote for float noise or a smaller run total', () => {
    const noise = buildCostBreakdown([r(1, 'propose', 3.0)], 3.004);
    expect(noise?.outsideTasks).toBeNull();
    const smaller = buildCostBreakdown([r(1, 'propose', 3.0)], 2.5);
    expect(smaller?.runTotal).toBe(2.5);
    expect(smaller?.outsideTasks).toBeNull();
  });

  it('ignores negative costs', () => {
    const b = buildCostBreakdown([r(1, 'propose', -1.0), r(1, 'measure', 2.0)], null);
    expect(b?.rows.map((x) => x.task)).toEqual(['measure']);
    expect(b?.tasksTotal).toBeCloseTo(2.0);
  });
});
