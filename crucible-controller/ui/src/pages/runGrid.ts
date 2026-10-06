// A run's task results folded into a grid: one row per task, one column per iteration, one cell
// per attempt. Pure functions so the folding is testable without a run — same split as taskGraph.ts,
// whose `latestResults` keeps only where a task ended up. This keeps every attempt it made.

import {
  mappedFrom,
  sum,
  timedResults,
  toneOf,
  type PlanTask,
  type TaskResult,
  type TaskTone,
} from './taskGraph';
import type { ExternalLinkRef } from '../ui';

type TaskAgent = NonNullable<TaskResult['agent']>;

/// One attempt, in the column of the iteration it reported from.
export interface GridCell {
  iter: number;
  status: string;
  tone: TaskTone;
  secs: number | null;
  costUsd: number | null;
  note: string;
  /// Why the executor never dispatched the task; set exactly when `status` is `blocked`.
  blocked: { reason: string; task: string | null } | null;
  /// The external results the attempt reported, in declaration order.
  links: ExternalLinkRef[];
  /// What the attempt ran on, as the engine resolved it; null for a command task and for a run
  /// logged before the engine reported it.
  agent: TaskAgent | null;
}

/// What a row's attempts ran on. A field is null when they did not all run on the same thing: a
/// mapped parent whose instances took different models has no one model to name.
export interface RowAgent {
  harness: string | null;
  model: string | null;
  effort: string | null;
}

export interface GridRow {
  task: string;
  /// A mapped instance (`node[item]`) is indented under the task it was mapped from.
  mapped: boolean;
  /// One entry per column, null where the task reported nothing that iteration.
  cells: (GridCell | null)[];
  attempts: number;
  /// Summed across attempts; null when no attempt carried a figure.
  secs: number | null;
  costUsd: number | null;
  /// How the task's latest attempt ended.
  tone: TaskTone;
  /// What the row ran on; null for a task that runs no agent. A mapped parent takes its
  /// instances' agreement rather than its own attempts, which it has none of.
  agent: RowAgent | null;
}

export interface RunGrid {
  iters: number[];
  rows: GridRow[];
  /// The widest row total, so duration bars share one scale. Null when nothing is timed.
  maxSecs: number | null;
  /// Attempt counts by status, in the order the statuses first appear.
  counts: { status: string; tone: TaskTone; count: number }[];
}

/// Plan order, dependencies first. Kahn's algorithm over `depends_on`, breaking ties by declaration
/// order so a plan whose tasks are independent reads the way its author wrote it. A dependency cycle
/// the planner let through leaves tasks unemitted; they follow in declaration order rather than
/// vanishing from the grid.
function planOrder(tasks: PlanTask[]): string[] {
  const known = new Set(tasks.map((t) => t.name));
  const pending = new Map(
    tasks.map((t) => [t.name, new Set(t.depends_on.filter((dep) => known.has(dep)))]),
  );
  const order: string[] = [];
  const emitted = new Set<string>();
  let progress = true;
  while (progress) {
    progress = false;
    for (const t of tasks) {
      if (emitted.has(t.name)) continue;
      const waiting = pending.get(t.name);
      if (waiting === undefined || [...waiting].some((dep) => !emitted.has(dep))) continue;
      order.push(t.name);
      emitted.add(t.name);
      progress = true;
    }
  }
  for (const t of tasks) if (!emitted.has(t.name)) order.push(t.name);
  return order;
}

/// Every task that has a row: the plan's own tasks in dependency order, each followed by the mapped
/// instances the run spawned from it, then anything that reported without being declared.
function rowOrder(tasks: PlanTask[], results: TaskResult[]): { task: string; mapped: boolean }[] {
  const declared = new Set(tasks.map((t) => t.name));
  const instances = new Map<string, string[]>();
  const loose: string[] = [];
  const seen = new Set<string>();
  for (const r of results) {
    if (declared.has(r.task) || seen.has(r.task)) continue;
    seen.add(r.task);
    const from = mappedFrom(r.task);
    if (from !== null && declared.has(from)) {
      instances.set(from, [...(instances.get(from) ?? []), r.task]);
    } else {
      loose.push(r.task);
    }
  }

  const rows: { task: string; mapped: boolean }[] = [];
  for (const name of planOrder(tasks)) {
    rows.push({ task: name, mapped: false });
    for (const instance of instances.get(name) ?? []) rows.push({ task: instance, mapped: true });
  }
  for (const name of loose) rows.push({ task: name, mapped: false });
  return rows;
}

function cellOf(r: TaskResult): GridCell {
  return {
    iter: r.iter,
    status: r.status,
    tone: toneOf(r.status),
    secs: r.secs ?? null,
    costUsd: r.cost_usd ?? null,
    note: r.note,
    blocked: r.blocked ? { reason: r.blocked.reason, task: r.blocked.task ?? null } : null,
    links: r.links,
    agent: r.agent ?? null,
  };
}

/// What a set of attempts ran on. Null when nothing reported an agent; a field is null when the
/// attempts did not all name the same one.
export function rowAgent(cells: (GridCell | null)[]): RowAgent | null {
  const agents = cells.flatMap((cell) => (cell?.agent == null ? [] : [cell.agent]));
  const [first] = agents;
  if (first === undefined) return null;
  const one = (pick: (a: TaskAgent) => string): string | null =>
    agents.every((a) => pick(a) === pick(first)) ? pick(first) : null;
  return { harness: one((a) => a.harness), model: one((a) => a.model), effort: one((a) => a.effort) };
}

/// The one line a blocked cell's tooltip adds: the typed reason, and the task it names.
export function blockedLine(blocked: GridCell['blocked']): string | null {
  if (blocked === null) return null;
  return blocked.task === null ? `blocked: ${blocked.reason}` : `blocked: ${blocked.reason} (${blocked.task})`;
}

/// Fold a run's plan and results into the grid. A task that reported twice in one iteration keeps
/// the last report: that is where the attempt ended, and the column holds one cell.
export function runGridView(tasks: PlanTask[], reported: TaskResult[]): RunGrid {
  const results = timedResults(reported);
  const iters = [...new Set(results.map((r) => r.iter))].sort((a, b) => a - b);
  const column = new Map(iters.map((iter, index) => [iter, index]));

  const attempts = new Map<string, TaskResult[]>();
  for (const r of results) attempts.set(r.task, [...(attempts.get(r.task) ?? []), r]);

  const rows = rowOrder(tasks, results).map(({ task, mapped }): GridRow => {
    const mine = attempts.get(task) ?? [];
    const cells: (GridCell | null)[] = iters.map(() => null);
    for (const r of mine) {
      const index = column.get(r.iter);
      if (index !== undefined) cells[index] = cellOf(r);
    }
    const last = mine.reduce<TaskResult | null>(
      (best, r) => (best === null || r.iter >= best.iter ? r : best),
      null,
    );
    return {
      task,
      mapped,
      cells,
      attempts: mine.length,
      secs: sum(mine.map((r) => r.secs)),
      costUsd: sum(mine.map((r) => r.cost_usd)),
      tone: last === null ? 'none' : toneOf(last.status),
      agent: rowAgent(cells),
    };
  });

  // A mapped parent runs nothing itself; what it ran on is what its instances agreed on.
  const instanceCells = new Map<string, (GridCell | null)[]>();
  for (const row of rows) {
    if (!row.mapped) continue;
    const from = mappedFrom(row.task);
    if (from === null) continue;
    instanceCells.set(from, [...(instanceCells.get(from) ?? []), ...row.cells]);
  }
  for (const row of rows) {
    const cells = instanceCells.get(row.task);
    if (cells !== undefined) row.agent = rowAgent(cells);
  }

  const maxSecs = rows.reduce<number | null>(
    (widest, row) => (row.secs === null ? widest : Math.max(widest ?? 0, row.secs)),
    null,
  );

  const tally = new Map<string, number>();
  for (const r of results) tally.set(r.status, (tally.get(r.status) ?? 0) + 1);
  const counts = [...tally].map(([status, count]) => ({ status, tone: toneOf(status), count }));

  return { iters, rows, maxSecs, counts };
}

/// A stretch of adjacent iterations a row reported the same status across, drawn as one block.
export interface GridSegment {
  /// How many columns the block spans.
  span: number;
  /// The attempts it melds, in iteration order. Empty for a stretch the task sat out.
  cells: GridCell[];
  /// The iterations it covers, in order.
  iters: number[];
}

/// An attempt that said nothing but its status. One that left a note, a block reason or a link has
/// something of its own to read, so it keeps its own cell rather than disappearing into a block.
function plain(cell: GridCell): boolean {
  return cell.note === '' && cell.blocked === null && cell.links.length === 0;
}

/// A row's cells melded on run boundaries: adjacent iterations that reported the same status
/// become one block, and a stretch the task sat out becomes one gap. Nothing is merged across a
/// change of status, so the order the run went in is still the order the row reads in.
export function meldRow(cells: (GridCell | null)[], iters: number[]): GridSegment[] {
  const segments: GridSegment[] = [];
  cells.forEach((cell, index) => {
    const iter = iters[index];
    if (iter === undefined) return;
    const last = segments[segments.length - 1];
    // A block only ever holds plain cells, so its first one speaks for the rest.
    const head = last?.cells[0];
    const joins =
      last !== undefined &&
      (cell === null
        ? last.cells.length === 0
        : head !== undefined && head.status === cell.status && plain(head) && plain(cell));
    if (last !== undefined && joins) {
      last.span += 1;
      last.iters.push(iter);
      if (cell !== null) last.cells.push(cell);
      return;
    }
    segments.push({ span: 1, cells: cell === null ? [] : [cell], iters: [iter] });
  });
  return segments;
}

/// A duration as the grid writes it: seconds under a minute, then minutes, then hours. Null stays
/// an em dash rather than a zero, which would read as a task that took no time.
export function formatSecs(secs: number | null): string {
  if (secs === null) return '—';
  if (secs < 60) return `${secs < 10 ? secs.toFixed(1) : Math.round(secs)}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${Math.round(secs % 60)}s`;
  return `${Math.floor(secs / 3600)}h ${Math.round((secs % 3600) / 60)}m`;
}

/// The bar's share of the widest row. A timed row always draws something, so a fast task reads as
/// fast rather than as untimed.
export function barShare(secs: number | null, maxSecs: number | null): number {
  if (secs === null || maxSecs === null || maxSecs <= 0) return 0;
  return Math.max(0.02, secs / maxSecs);
}
