import { useMemo, useState } from 'react';
import { $api } from '../api/client';
import { cn, Empty, ExternalLinks, Section, SectionBody, SectionHeader, Tooltip } from '../ui';
import {
  barShare,
  blockedLine,
  formatSecs,
  meldRow,
  runGridView,
  type GridCell,
  type GridRow,
  type GridSegment,
} from './runGrid';
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

/// How many of a melded block's attempts its tooltip names before it stops counting them out.
const LISTED = 6;

interface SegmentProps {
  row: GridRow;
  segment: GridSegment;
  picked: boolean;
  onPick: (iter: number) => void;
}

/// The iterations a block covers. A range only reads as a range when the run reported every
/// iteration in it; a sparse run is listed out rather than claiming attempts that never happened.
function coverage(iters: number[]): string {
  const [first] = iters;
  const last = iters[iters.length - 1];
  if (first === undefined || last === undefined) return '';
  if (iters.length === 1) return `${first}`;
  return last - first + 1 === iters.length ? `${first}–${last}` : iters.join(', ');
}

/// What a block's tooltip says: the iterations it covers, then the attempts themselves. A block of
/// one reads as the attempt it is; a melded one names the status once and lists what each
/// iteration under it spent.
function segmentDetail(row: GridRow, segment: GridSegment): string {
  const [first] = segment.cells;
  if (first === undefined) return `${row.task} · iters ${coverage(segment.iters)}`;
  const spent = (cell: GridCell) =>
    [cell.secs === null ? null : formatSecs(cell.secs), cell.costUsd === null ? null : formatCost(cell.costUsd)]
      .filter((part): part is string => part !== null)
      .join(' · ');
  if (segment.cells.length === 1) {
    return [
      `${row.task} · iter ${first.iter}`,
      `${first.status}${spent(first) === '' ? '' : ` · ${spent(first)}`}`,
      blockedLine(first.blocked),
      first.note === '' ? null : first.note,
    ]
      .filter((line): line is string => line !== null)
      .join('\n');
  }
  const head = `${row.task} · iters ${coverage(segment.cells.map((cell) => cell.iter))} · ${first.status} ×${segment.cells.length}`;
  const listed = segment.cells
    .slice(0, LISTED)
    .map((cell) => `iter ${cell.iter}${spent(cell) === '' ? '' : ` · ${spent(cell)}`}`);
  const rest = segment.cells.length - LISTED;
  return [head, ...listed, rest > 0 ? `+${rest} more` : null]
    .filter((line): line is string => line !== null)
    .join('\n');
}

/// One block of attempts: a stretch of iterations a task reported the same status across, drawn as
/// one wide swatch rather than a row of identical squares. A stretch a task sat out is drawn as an
/// empty box rather than left blank, so the columns still read as a grid down a row that only ran
/// once. Picking a block opens the first attempt under it.
function Segment({ row, segment, picked, onPick }: SegmentProps) {
  const [first] = segment.cells;
  const style = { gridColumn: `span ${segment.span}` };
  if (first === undefined) {
    return <span aria-hidden className="h-3.5 border border-rule" style={{ ...style, opacity: 0.45 }} />;
  }

  const covers =
    segment.cells.length === 1
      ? `iteration ${first.iter}`
      : `iterations ${coverage(segment.cells.map((cell) => cell.iter))}`;

  // The tooltip's trigger is the grid item, not the button, so the span is carried by a wrapper
  // the button then fills.
  return (
    <span style={style} className="grid">
      <Tooltip content={<span className="whitespace-pre-line">{segmentDetail(row, segment)}</span>} delay={150}>
        <button
          type="button"
          data-testid="grid-cell"
          data-task={row.task}
          data-iter={first.iter}
          data-status={first.status}
          data-span={segment.span}
          aria-label={`${row.task} ${covers} ${first.status}`}
          aria-pressed={picked}
          onClick={() => onPick(first.iter)}
          className={cn(
            'flex h-3.5 w-full cursor-pointer items-center justify-center border font-mono text-micro leading-none text-paper',
            swatchOf(first.status),
            picked ? 'border-ink outline-1 outline-offset-1 outline-ink' : 'border-transparent'
          )}
        >
          {segment.cells.length > 1 && segment.cells.length}
        </button>
      </Tooltip>
    </span>
  );
}

interface GridBodyProps {
  runId: string;
  graph: RunGraph;
  /// The inference provider the run's launch pinned; blank when it resolved the defaults.
  provider: string;
}

/// One of the three agent columns. A command task leaves them blank; a row whose attempts did not
/// all run on the same thing has no one value to name and takes a dash.
function AgentCell({ task, col, value }: { task: string; col: string; value: string | null }) {
  return (
    <span
      data-task-agent={task}
      data-col={col}
      className="truncate text-micro text-ink-3"
      title={value ?? 'the attempts did not all run on the same one'}
    >
      {value === null ? '—' : value}
    </span>
  );
}

/// Every attempt a run made, as tasks down and iterations across. The graph shows where each task
/// ended up; this shows how it got there — which task was retried, which one flapped between
/// iterations, and which one the run spent its time in. Picking a cell reads that task's evidence.
function GridBody({ runId, graph, provider }: GridBodyProps) {
  const grid = useMemo(() => runGridView(graph.tasks, graph.results), [graph]);
  const [picked, setPicked] = useState<Picked | null>(null);

  if (grid.iters.length === 0) {
    return (
      <SectionBody>
        <Empty
          title="Nothing reported"
          description="No task of this run has reported an attempt yet."
        />
      </SectionBody>
    );
  }

  const cells = `repeat(${grid.iters.length}, 0.875rem)`;
  // The duration bar and figure only earn their columns when some attempt was timed, and the
  // agent columns only when some task ran one.
  const timed = grid.maxSecs !== null;
  const ran = grid.rows.some((row) => row.agent !== null);
  const columns = [
    '12rem',
    ...(ran ? ['7rem', '8rem', '4rem'] : []),
    ...(timed ? ['4.5rem', '3.5rem'] : []),
    '3.5rem',
    'max-content',
  ].join(' ');

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
          {ran && (
            <>
              <span className="text-micro tracking-label text-ink-3 uppercase">Provider</span>
              <span className="text-micro tracking-label text-ink-3 uppercase">Model</span>
              <span className="text-micro tracking-label text-ink-3 uppercase">Effort</span>
            </>
          )}
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

              {ran && (
                <>
                  <AgentCell task={row.task} col="provider" value={row.agent === null ? '' : provider} />
                  <AgentCell task={row.task} col="model" value={row.agent === null ? '' : row.agent.model} />
                  <AgentCell task={row.task} col="effort" value={row.agent === null ? '' : row.agent.effort} />
                </>
              )}

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
                {meldRow(row.cells, grid.iters).map((segment) => (
                  <Segment
                    key={segment.iters[0]}
                    row={row}
                    segment={segment}
                    picked={picked?.task === row.task && segment.iters.includes(picked.iter)}
                    onPick={(iter) => setPicked({ task: row.task, iter })}
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
                {pickedCell.agent !== null && (
                  <span className="text-ink-3">
                    {[pickedCell.agent.harness, pickedCell.agent.model, pickedCell.agent.effort]
                      .filter((part) => part !== '')
                      .join(' · ')}
                  </span>
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
          {pickedCell !== null && <ExternalLinks links={pickedCell.links} className="mt-2" />}
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
  // The provider's name lives on the run's launch pin, not in anything the engine logs. Shares
  // the detail endpoint's cache key with the rest of the page.
  const detail = $api.useQuery('get', '/api/runs/{run_id}', {
    params: { path: { run_id: runId } },
  });
  const graph: RunGraph | undefined = query.data;

  if (graph === undefined || graph.tasks.length === 0) return null;
  return (
    <Section>
      <SectionHeader title="Grid" note={`${graph.results.length} attempts`} />
      <GridBody runId={runId} graph={graph} provider={detail.data?.run.agent_provider ?? ''} />
    </Section>
  );
}
