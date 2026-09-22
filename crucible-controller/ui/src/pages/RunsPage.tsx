import { useMemo, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import type { OnChangeFn, SortingState } from '@tanstack/react-table';
import { $api } from '../api/client';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  Delta,
  Empty,
  formatStamp,
  Identifier,
  Mono,
  PageHeader,
  PrLink,
  QueryState,
  Score,
  Sparkline,
  Status,
  statusTone,
  Toolbar,
  ToolbarActions,
  ToolbarGroup,
  ToolbarSearch,
  useDataTable,
} from '../ui';
import type { SparklineTone, ToolbarOption } from '../ui';
import { absoluteTime, relativeTime } from './journeyView';
import { formatCost, formatScore, scoresImproveDownward, transportLossLabel } from './runReport';
import { runStatusColor } from './runStatus';
import { detailPath } from './launchView';

type RunRow = components['schemas']['RunRowDto'];

const PAGE_SIZE = 50;

// Sortable columns map to the API's `sort` keys (server-side); the rest are display-only.
type SortKey = 'created' | 'best_score' | 'cost';
type SortDir = 'asc' | 'desc';

const SORT_KEYS: Record<string, SortKey> = {
  best_score: 'best_score',
  cost_usd: 'cost',
  created: 'created',
};

const STATUS_OPTIONS: readonly ToolbarOption<string>[] = [
  { value: '', label: 'All' },
  { value: 'running', label: 'Running' },
  { value: 'finished', label: 'Finished' },
  { value: 'incomplete', label: 'Incomplete' },
  { value: 'escalated', label: 'Escalated' },
  { value: 'failed', label: 'Failed' },
];

const EMPTY_ROWS: RunRow[] = [];

function vsBaseline(series: readonly number[], best: number | null | undefined): number | null {
  const base = series[0];
  if (base === undefined || base === 0 || best === null || best === undefined) return null;
  return ((best - base) / base) * 100;
}

function sparklineTone(series: readonly number[], best: number | null | undefined): SparklineTone {
  const delta = vsBaseline(series, best);
  if (delta === null) return 'neutral';
  const improved = scoresImproveDownward(series, best) ? delta < 0 : delta > 0;
  return improved ? 'win' : 'bad';
}

const helper = createDataColumnHelper<RunRow>();

const columns = helper.columns([
  helper.accessor('run_id', {
    header: 'Run',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => <Identifier to={`/runs/${getValue()}`}>{getValue()}</Identifier>,
  }),
  helper.accessor('issue_key', {
    header: 'Issue',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => {
      const key = getValue();
      return key ? (
        <Identifier to={detailPath(key)}>{key}</Identifier>
      ) : (
        <Mono tone="ink-3">—</Mono>
      );
    },
  }),
  helper.accessor('repo', {
    header: 'Repo',
    enableSorting: false,
    meta: { hideNarrow: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => getValue() ?? '—',
  }),
  helper.accessor('status', {
    header: 'Status',
    enableSorting: false,
    cell: ({ getValue, row }) => {
      const lost = transportLossLabel(row.original.transport_losses);
      return (
        <span className="flex flex-wrap items-center gap-2">
          <Status status={getValue()} tone={statusTone(runStatusColor(getValue()))} />
          {lost && <Status status={lost} tone="red" pulse={false} />}
        </span>
      );
    },
  }),
  helper.display({
    id: 'iterations',
    header: 'Iterations',
    meta: { pad: 'tight', shrink: true },
    cell: ({ row }) => {
      const { score_series: series, best_score: best } = row.original;
      if (series.length === 0) return <Mono tone="ink-3">—</Mono>;
      return <Sparkline values={series} tone={sparklineTone(series, best)} />;
    },
  }),
  helper.display({
    id: 'iter_count',
    header: 'Iter',
    meta: { align: 'end', shrink: true, className: 'font-mono text-data text-ink-3' },
    cell: ({ row }) => row.original.score_series.length || '—',
  }),
  helper.accessor('best_score', {
    header: 'Best score',
    meta: { align: 'end' },
    cell: ({ getValue }) => <Score value={formatScore(getValue())} />,
  }),
  helper.display({
    id: 'vs_base',
    header: 'vs base',
    meta: { align: 'end', shrink: true },
    cell: ({ row }) => {
      const { score_series: series, best_score: best } = row.original;
      return (
        <Delta
          percent={vsBaseline(series, best)}
          betterWhen={scoresImproveDownward(series, best) ? 'lower' : 'higher'}
        />
      );
    },
  }),
  helper.accessor('cost_usd', {
    header: 'Cost',
    meta: { align: 'end', className: 'font-mono text-data' },
    cell: ({ getValue }) => formatCost(getValue()),
  }),
  helper.accessor('pr_url', {
    header: 'PR',
    enableSorting: false,
    cell: ({ getValue }) => {
      const url = getValue();
      return url ? <PrLink url={url} compact /> : <Mono tone="ink-3">—</Mono>;
    },
  }),
  helper.accessor('created', {
    header: 'Started',
    meta: { className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => {
      const created = getValue();
      const relative = relativeTime(created);
      return (
        <span title={absoluteTime(created) ?? undefined}>{relative ?? formatStamp(created)}</span>
      );
    },
  }),
]);

export function RunsPage() {
  useLiveEvents();

  // Filters seed from the URL (`/runs?status=running` from the dashboard funnel/chips) and live
  // in state from there — in-page filter changes don't rewrite the URL.
  const [searchParams] = useSearchParams();
  const [status, setStatus] = useState(searchParams.get('status') ?? '');
  const [repo, setRepo] = useState(searchParams.get('repo') ?? '');
  const [sort, setSort] = useState<SortKey>('created');
  const [dir, setDir] = useState<SortDir>('desc');
  const [page, setPage] = useState(0);

  const query = {
    ...(status ? { status } : {}),
    ...(repo ? { repo } : {}),
    kind: 'autoresearch' as const,
    sort,
    dir,
    limit: PAGE_SIZE,
    offset: page * PAGE_SIZE,
  };

  const runs = $api.useQuery('get', '/api/runs', { params: { query } });
  const rows = runs.data ?? EMPTY_ROWS;
  const shownCost = useMemo(
    () => rows.reduce((total, run) => total + (run.cost_usd ?? 0), 0),
    [rows],
  );

  const sortColumn = sort === 'cost' ? 'cost_usd' : sort;
  const sorting: SortingState = [{ id: sortColumn, desc: dir === 'desc' }];
  const onSortingChange: OnChangeFn<SortingState> = (updater) => {
    const next = typeof updater === 'function' ? updater(sorting) : updater;
    const first = next[0];
    const key = first ? SORT_KEYS[first.id] : undefined;
    if (key === undefined) return;
    setSort(key);
    setDir(first.desc ? 'desc' : 'asc');
    setPage(0);
  };

  const table = useDataTable({
    columns,
    data: rows,
    manualSorting: true,
    state: { sorting },
    onSortingChange,
  });

  return (
    <>
      <PageHeader
        eyebrow="Execution"
        title="Runs"
        description="Each run drives one issue through iterations until the gate stops improving."
      />

      <Toolbar>
        <ToolbarGroup
          label="Status"
          options={STATUS_OPTIONS}
          value={status}
          onChange={(value) => {
            setStatus(value);
            setPage(0);
          }}
        />
        <ToolbarSearch
          value={repo}
          onChange={(value) => {
            setRepo(value);
            setPage(0);
          }}
          placeholder="Filter by repository (owner/repo)…"
          aria-label="Filter by repository"
        />
        <ToolbarActions>
          <Button variant="filled" render={<a href="/api/export/runs.parquet" download />}>
            RUNS.PARQUET
          </Button>
          <Button variant="filled" render={<a href="/api/export/iterations.parquet" download />}>
            ITERATIONS.PARQUET
          </Button>
        </ToolbarActions>
      </Toolbar>

      <QueryState query={runs} noun="RUNS">
        <DataTable
          table={table}
          empty={<Empty title="NO RUNS" description="Nothing has been dispatched in this window." />}
          footer={
            <>
              Showing {rows.length} · {formatCost(shownCost)} across shown runs
            </>
          }
        />
        <div className="flex items-center gap-2.5 border-b border-rule-hard bg-surface px-4.5 py-1.5">
          <Button
            disabled={page === 0}
            onClick={() => {
              setPage((p) => Math.max(0, p - 1));
            }}
          >
            ◂ PREV
          </Button>
          <Mono size="label" tone="ink-3" uppercase>
            Page {page + 1}
          </Mono>
          <Button
            disabled={rows.length < PAGE_SIZE}
            onClick={() => {
              setPage((p) => p + 1);
            }}
          >
            NEXT ▸
          </Button>
        </div>
      </QueryState>
    </>
  );
}
