// Pure derivation of the run's cost breakdown from the task-graph results
// (`GET /api/runs/{run_id}/graph` → results[]). Kept free of React so the aggregation and the
// reconcile-against-run-total rule stay unit-testable.

export interface TaskCostInput {
  iter: number;
  task: string;
  cost_usd?: number | null;
}

export interface TaskCostRow {
  task: string;
  total: number;
  byIter: ReadonlyMap<number, number>;
}

export interface CostBreakdown {
  /// One row per task with any nonzero cost, most expensive first (task name breaks ties).
  rows: TaskCostRow[];
  /// Distinct iterations carrying any task cost, ascending.
  iters: number[];
  tasksTotal: number;
  runTotal: number | null;
  /// run.cost_usd minus the graded-task sum, when the run total is known and meaningfully larger.
  /// Agent-turn spend lands on the propose task, but budget spend outside tasks exists.
  outsideTasks: number | null;
}

// Below half a cent the difference is float noise, not spend worth a footnote.
const RECONCILE_EPSILON = 0.005;

/// Fold task results into a per-task cost table. `null` when no result carries a nonzero cost —
/// the caller renders nothing rather than an all-dash table.
export function buildCostBreakdown(
  results: readonly TaskCostInput[],
  runTotal: number | null | undefined,
): CostBreakdown | null {
  const byTask = new Map<string, Map<number, number>>();
  const iterSet = new Set<number>();
  let tasksTotal = 0;
  for (const r of results) {
    const cost = r.cost_usd ?? 0;
    if (cost <= 0) continue;
    tasksTotal += cost;
    iterSet.add(r.iter);
    const iters = byTask.get(r.task) ?? new Map<number, number>();
    iters.set(r.iter, (iters.get(r.iter) ?? 0) + cost);
    byTask.set(r.task, iters);
  }
  if (byTask.size === 0) return null;

  const rows: TaskCostRow[] = [...byTask.entries()]
    .map(([task, byIter]) => ({
      task,
      total: [...byIter.values()].reduce((a, b) => a + b, 0),
      byIter,
    }))
    .sort((a, b) => b.total - a.total || a.task.localeCompare(b.task));

  const run = runTotal ?? null;
  const outsideTasks =
    run !== null && run - tasksTotal > RECONCILE_EPSILON ? run - tasksTotal : null;
  return {
    rows,
    iters: [...iterSet].sort((a, b) => a - b),
    tasksTotal,
    runTotal: run,
    outsideTasks,
  };
}
