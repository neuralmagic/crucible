import { $api } from '../api/client';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
import {
  createDataColumnHelper,
  DataTable,
  Empty,
  Identifier,
  Mono,
  PageHeader,
  QueryState,
  useDataTable,
} from '../ui';

type RepoHealthDto = components['schemas']['RepoHealthDto'];

const EMPTY_REPOS: RepoHealthDto[] = [];

const helper = createDataColumnHelper<RepoHealthDto>();

function countColumn(id: 'new' | 'scoped' | 'awaiting_approval' | 'running' | 'pr_open' | 'parked' | 'done', header: string) {
  return helper.accessor(id, {
    header,
    enableSorting: false,
    meta: { align: 'end', shrink: true },
    cell: ({ getValue }) => <Mono tone={getValue() === 0 ? 'ink-3' : 'ink-2'}>{getValue()}</Mono>,
  });
}

const columns = helper.columns([
  helper.accessor('repo', {
    header: 'Repo',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={`/issues?repo=${encodeURIComponent(getValue())}`}>{getValue()}</Identifier>
    ),
  }),
  countColumn('new', 'New'),
  countColumn('scoped', 'Scoped'),
  countColumn('awaiting_approval', 'Awaiting'),
  countColumn('running', 'Running'),
  countColumn('pr_open', 'PR open'),
  countColumn('parked', 'Parked'),
  countColumn('done', 'Done'),
  helper.accessor('total', {
    header: 'Total',
    enableSorting: false,
    meta: { align: 'end', shrink: true },
    cell: ({ getValue }) => (
      <Mono weight="semibold" tone="ink">
        {getValue()}
      </Mono>
    ),
  }),
  helper.accessor('watermark', {
    header: 'Watermark',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-3' },
    cell: ({ getValue }) => getValue() || '—',
  }),
]);

export function ReposPage() {
  useLiveEvents();

  const repos = $api.useQuery('get', '/api/repos');
  const rows = repos.data ?? EMPTY_REPOS;

  const table = useDataTable({
    columns,
    data: rows,
    getRowId: (repo) => repo.repo,
  });

  return (
    <>
      <PageHeader
        eyebrow="Records"
        title="Repos"
        description="Every watched repository, its issue counts by status, and how far upstream polling has read."
      />

      <QueryState query={repos} noun="REPOS">
        <DataTable
          table={table}
          empty={<Empty title="NO REPOSITORIES" description="No repositories tracked yet." />}
          footer={
            <>
              {rows.length} repositor{rows.length === 1 ? 'y' : 'ies'}
            </>
          }
        />
      </QueryState>
    </>
  );
}
