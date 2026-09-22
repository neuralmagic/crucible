import { useMemo, useState } from 'react';
import { $api } from '../api/client';
import { cn, Empty, Section, SectionBody, SectionHeader, Tooltip } from '../ui';
import { barShare, blockedLine, formatSecs, type GridCell, type GridRow, runGridView } from './runGrid';
import { formatCost } from './runReport';
import { TaskEvidence } from './TaskEvidence';
import type { RunGraph } from './RunTaskGraph';

/// The swatch a status is drawn in. Beyond pass and fail the executor reports why a task never
/// produced a verdict, and those reasons do not read alike: work that was cut short is not work
/// that was never asked for.
const STATUS_SWATCH: Record<string, string> = {
  pass: 'bg-green',
  fail: 'bg-red',
  transport: 'bg-red',
  blocked: 'bg-amber',
  truncated: 'bg-amber',
  skipped: 'bg-rule-hard',
};

function swatchOf(status: string): string {
  return STATUS_SWATCH[status] ?? 'bg-ink-3';
}

interface Picked {
  task: string;
  iter: number;
}

interface CellProps {
  row: GridRow;
  cell: GridCell | null;
  iter: number;
  picked: boolean;
  onPick: () => void;
}

/// One attempt. An iteration a task sat out is drawn as an empty box rather than left blank, so the
/// columns still read as a grid down a row that only ran once.
function Cell({ row, cell, iter, picked, onPick }: CellProps) {
  if (cell === null) {
    return (
      <span
        aria-hidden
        className="h-3.5 w-3.5 border border-rule"
        style={{ opacity: 0.45 }}
      />
    );
  }

  const detail = [
    `${row.task} · iter ${iter}`,
    `${cell.status}${cell.secs === null ? '' : ` · ${formatSecs(cell.secs)}`}`,
    cell.costUsd === null ? null : formatCost(cell.costUsd),
    blockedLine(cell.blocked),
    cell.note === '' ? null : cell.note,
  ]
    .filter((line): line is string => line !== null)
    .join('\n');

  return (
    <Tooltip content={<span className="whitespace-pre-line">{detail}</span>} delay={150}>
      <button
        type="button"
        data-testid="grid-cell"
        data-task={row.task}
        data-iter={iter}
        data-status={cell.status}
        aria-label={`${row.task} iteration ${iter} ${cell.status}`}
        aria-pressed={picked}
        onClick={onPick}
        className={cn(
          'h-3.5 w-3.5 cursor-pointer border',
          swatchOf(cell.status),
          picked ? 'border-ink outline-1 outline-offset-1 outline-ink' : 'border-transparent'
        )}
      />
    </Tooltip>
  );
}

interface GridBodyProps {
  runId: string;
  graph: RunGraph;
}

/// Every attempt a run made, as tasks down and iterations across. The graph shows where each task
/// ended up; this shows how it got there — which task was retried, which one flapped between
/// iterations, and which one the run spent its time in. Picking a cell reads that task's evidence.
function GridBody({ runId, graph }: GridBodyProps) {
  const grid = useMemo(() => runGridView(graph.tasks, graph.results), [graph]);
  const [picked, setPicked] = useState<Picked | null>(null);

  if (grid.iters.length === 0) {
    return (
      <SectionBody>
        <Empty
          title="NOTHING REPORTED"
          description="No task of this run has reported an attempt yet."
        />
      </SectionBody>
    );
  }

  const cells = `repeat(${grid.iters.length}, 0.875rem)`;
  // The duration bar and figure only earn their columns when some attempt was timed.
  const timed = grid.maxSecs !== null;
  const columns = timed ? '12rem 4.5rem 3.5rem 3.5rem max-content' : '12rem 3.5rem max-content';

  const pickedRow = picked === null ? undefined : grid.rows.find((row) => row.task === picked.task);
  const pickedCell =
    picked === null || pickedRow === undefined
      ? null
      : (pickedRow.cells[grid.iters.indexOf(picked.iter)] ?? null);

  return (
    <>
      <SectionBody padded={false} className="overflow-x-auto px-4.5 py-3">
        <div
          role="table"
          aria-label="Task attempts by iteration"
          data-testid="run-grid"
          className="grid w-max items-center gap-x-3 gap-y-1 font-mono"
          style={{ gridTemplateColumns: columns }}
        >
          <span className="sticky left-0 z-10 bg-surface text-micro tracking-label text-ink-3 uppercase">
            Task
          </span>
          {timed && (
            <span className="col-span-2 text-micro tracking-label text-ink-3 uppercase">Duration</span>
          )}
          <span className="text-right text-micro tracking-label text-ink-3 uppercase">Cost</span>
          <span className="grid gap-px" style={{ gridTemplateColumns: cells }}>
            {grid.iters.map((iter) => (
              <span
                key={iter}
                className="text-center text-micro text-ink-3"
                title={`Iteration ${iter}`}
              >
                {iter}
              </span>
            ))}
          </span>

          {grid.rows.map((row) => (
            <div key={row.task} className="contents">
              <button
                type="button"
                data-task-row={row.task}
                title={row.task}
                onClick={() => {
                  const last = [...row.cells].reverse().find((cell) => cell !== null);
                  if (last !== undefined && last !== null) setPicked({ task: row.task, iter: last.iter });
                }}
                className={cn(
                  'sticky left-0 z-10 block truncate bg-surface text-left text-data hover:text-ink',
                  row.mapped && 'pl-3 text-ink-3',
                  picked?.task === row.task ? 'text-ink' : 'text-ink-2'
                )}
              >
                {row.mapped ? `└ ${row.task.slice(row.task.indexOf('['))}` : row.task}
              </button>

              {timed && (
                <>
                  <span className="relative block h-2.5 bg-sunk" title={formatSecs(row.secs)}>
                    <span
                      aria-hidden
                      className={cn('absolute inset-y-0 left-0', row.tone === 'fail' ? 'bg-red' : 'bg-ink-3')}
                      style={{ width: `${barShare(row.secs, grid.maxSecs) * 100}%` }}
                    />
                  </span>
                  <span className="text-right text-micro text-ink-3">{formatSecs(row.secs)}</span>
                </>
              )}
              <span className="text-right text-micro text-ink-3">{formatCost(row.costUsd)}</span>

              <span className="grid gap-px" style={{ gridTemplateColumns: cells }}>
                {grid.iters.map((iter, index) => (
                  <Cell
                    key={iter}
                    row={row}
                    cell={row.cells[index] ?? null}
                    iter={iter}
                    picked={picked?.task === row.task && picked.iter === iter}
                    onPick={() => setPicked({ task: row.task, iter })}
                  />
                ))}
              </span>
            </div>
          ))}
        </div>
      </SectionBody>

      <SectionBody className="flex flex-wrap items-center gap-3 border-t border-rule py-2">
        {grid.counts.map((count) => (
          <span key={count.status} className="flex items-center gap-1.5 font-mono text-micro tracking-label text-ink-3 uppercase">
            <span aria-hidden className={cn('h-2.5 w-2.5', swatchOf(count.status))} />
            {count.status} {count.count}
          </span>
        ))}
      </SectionBody>

      {picked !== null && pickedRow !== undefined && (
        <SectionBody className="border-t border-rule-hard">
          <p className="m-0 flex flex-wrap items-baseline gap-x-3 font-mono text-data text-ink-2">
            <span className="text-data-lg text-ink">{picked.task}</span>
            <span className="text-micro tracking-label text-ink-3 uppercase">
              iter {picked.iter}
            </span>
            {pickedCell === null ? (
              <span className="text-ink-3">did not report this iteration</span>
            ) : (
              <>
                <span>{pickedCell.status}</span>
                {pickedCell.secs !== null && (
                  <span className="text-ink-3">{formatSecs(pickedCell.secs)}</span>
                )}
                {pickedCell.costUsd !== null && (
                  <span className="text-ink-3">{formatCost(pickedCell.costUsd)}</span>
                )}
              </>
            )}
            <button
              type="button"
              onClick={() => setPicked(null)}
              className="ml-auto border border-rule-hard px-1.5 py-0.5 text-micro tracking-label text-ink-2 uppercase hover:border-ink hover:text-ink"
            >
              Close
            </button>
          </p>
          {pickedCell !== null && pickedCell.note !== '' && (
            <p className="mt-1 mb-0 font-mono text-data break-words text-ink-3">{pickedCell.note}</p>
          )}
          <TaskEvidence key={picked.task} runId={runId} task={picked.task} />
        </SectionBody>
      )}
    </>
  );
}

/// The run's attempts on the grid surface. Shares the graph endpoint's cache key with
/// `RunTaskGraph`, so drawing both costs one fetch.
export function RunTaskGrid({ runId }: { runId: string }) {
  const query = $api.useQuery(
    'get',
    '/api/runs/{run_id}/graph',
    { params: { path: { run_id: runId } } },
    // A transient 5xx (a pod mid-rollout) must not blank the section for the session.
    { retry: 2 }
  );
  const graph: RunGraph | undefined = query.data;

  if (graph === undefined || graph.tasks.length === 0) return null;
  return (
    <Section>
      <SectionHeader title="Grid" note={`${graph.results.length} attempts`} />
      <GridBody runId={runId} graph={graph} />
    </Section>
  );
}
