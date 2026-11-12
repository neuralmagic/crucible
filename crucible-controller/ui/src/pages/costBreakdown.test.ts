import { describe, expect, it } from 'vitest';
import {
  buildCostBreakdown,
  iterDisplayCosts,
  type TaskCostInput,
  type TaskCostRow,
} from './costBreakdown';

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

describe('iterDisplayCosts', () => {
  const row = (inputs: TaskCostInput[]): TaskCostRow => {
    const b = buildCostBreakdown(inputs, null);
    if (b === null || b.rows.length === 0) throw new Error('fixture produced no rows');
    return b.rows[0];
  };

  it('sums the iter cells to the row total when independent rounding would not', () => {
    const row0 = row([
      r(1, 'propose', 0.1268564),
      r(2, 'propose', 0.2094645),
      r(3, 'propose', 0.3084733),
    ]);
    const cells = iterDisplayCosts(row0);
    const cents = [...cells.values()].map((v) => Math.round(v * 100));
    expect(cents).toEqual([12, 21, 31]);
    expect(cents.reduce((a, b) => a + b, 0)).toBe(Math.round(row0.total * 100));
  });

  it('leaves cells whose rounding already sums alone', () => {
    const cells = iterDisplayCosts(row([r(1, 'propose', 0.1), r(2, 'propose', 0.2), r(3, 'propose', 0.3)]));
    expect([...cells.entries()].sort((a, b) => a[0] - b[0]).map(([, v]) => v)).toEqual([0.1, 0.2, 0.3]);
  });

  it('gives a spare cent to the cell with the largest fractional remainder', () => {
    const cells = iterDisplayCosts(row([r(1, 'propose', 0.104), r(2, 'propose', 0.101), r(3, 'propose', 0.101)]));
    expect(cells.get(1)).toBeCloseTo(0.11);
    expect(cells.get(2)).toBeCloseTo(0.10);
    expect(cells.get(3)).toBeCloseTo(0.10);
  });

  it('breaks remainder ties by iteration', () => {
    const cells = iterDisplayCosts(row([r(2, 'propose', 0.105), r(1, 'propose', 0.105)]));
    expect(cells.get(1)).toBeCloseTo(0.11);
    expect(cells.get(2)).toBeCloseTo(0.10);
  });
});
