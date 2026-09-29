import { useQueryClient } from '@tanstack/react-query';
import { useMemo } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { $api } from '../api/client';
import type { components } from '../api/schema';
import { narrow } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import {
  Applied,
  Button,
  createDataColumnHelper,
  DataTable,
  Empty,
  Facets,
  formatStamp,
  Identifier,
  Mono,
  PageHeader,
  QueryState,
  Section,
  SectionBody,
  SectionHeader,
  Status,
  statusTone,
  useDataTable,
} from '../ui';
import type { AppliedFilter, FacetRow } from '../ui';
import { issueStatusColor } from './issueStatus';
import { relativeTime } from './journeyView';
import { launchPath, relaunchPath } from './launchView';
import { formatCost, transportLossLabel } from './runReport';
import {
  DEFAULT_FILTERS,
  parseRunFilters,
  runFilterParams,
  runInContext,
  runsView,
  type RunFilters,
} from './playbookRunsView';

type PlaybookRunDto = components['schemas']['PlaybookRunDto'];
type OneShotDto = components['schemas']['OneShotView'];

const EMPTY_RUNS: PlaybookRunDto[] = [];
const EMPTY_ONE_SHOTS: OneShotDto[] = [];
const EMPTY_OWNED: { id: string; owner: string }[] = [];

const ONE_SHOT_COLOR: Record<string, string> = {
  pending: 'blue',
  fired: 'green',
  canceled: 'grey',
  failed: 'red',
};

const runHelper = createDataColumnHelper<PlaybookRunDto>();

const runColumns = runHelper.columns([
  runHelper.accessor('status', {
    header: 'Status',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ row }) => {
      const lost = transportLossLabel(row.original.transport_losses);
      return (
        <span className="flex flex-wrap items-center gap-2">
          <Status
            status={row.original.status}
            tone={statusTone(issueStatusColor(row.original.status))}
            pulse={row.original.status === 'running'}
          />
          {lost && <Status status={lost} tone="red" pulse={false} />}
        </span>
      );
    },
  }),
  runHelper.accessor('playbook', {
    header: 'Playbook',
    enableSorting: false,
    cell: ({ row }) => {
      const { key, playbook, draft_version, parked_reason } = row.original;
      return (
        <div className="min-w-0 max-w-[60ch]">
          <Link to={launchPath(key)} className="font-mono text-data font-semibold text-ink hover:underline">
            {playbook}
            {draft_version === null || draft_version === undefined ? null : (
              <span className="ml-1.5 font-normal text-ink-3">draft v{draft_version}</span>
            )}
          </Link>
          {parked_reason ? (
            <p className="m-0 mt-0.5 truncate text-ink-3" title={parked_reason}>
              {parked_reason}
            </p>
          ) : null}
        </div>
      );
    },
  }),
  runHelper.accessor('created_by', {
    header: 'Launched by',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => getValue() ?? '—',
  }),
  runHelper.accessor('created_at', {
    header: 'When',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => <span title={formatStamp(getValue())}>{relativeTime(getValue()) ?? '—'}</span>,
  }),
  runHelper.display({
    id: 'cost',
    header: 'Cost',
    meta: { align: 'end', shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) => formatCost(row.original.cost_usd),
  }),
  runHelper.display({
    id: 'relaunch',
    header: '',
    meta: { pad: 'tight', shrink: true },
    cell: ({ row }) => (
      <Button
        className="uppercase"
        render={<Link to={relaunchPath(row.original.playbook, row.original.key)} />}
      >
        Relaunch
      </Button>
    ),
  }),
]);

export function PlaybookRunsPage() {
  const runs = $api.useQuery('get', '/api/playbook-runs');
  const oneShots = $api.useQuery('get', '/api/one-shots');
  const playbooks = $api.useQuery('get', '/api/playbooks');
  const drafts = $api.useQuery('get', '/api/playbook-drafts');
  const owner = useOwnerContext();
  const oneShotRows = narrow(oneShots.data ?? EMPTY_ONE_SHOTS, owner.context, (row) => row.owner_principal);

  const [searchParams, setSearchParams] = useSearchParams();
  const filters = parseRunFilters(searchParams);
  const patch = (next: Partial<RunFilters>) => {
    setSearchParams(runFilterParams({ ...filters, ...next }), { replace: true });
  };

  const owners = useMemo(
    () => new Map([...(playbooks.data ?? EMPTY_OWNED), ...(drafts.data ?? EMPTY_OWNED)].map((p) => [p.id, p.owner])),
    [playbooks.data, drafts.data],
  );
  const inContext = useMemo(
    () => (runs.data ?? EMPTY_RUNS).filter((run) => runInContext(run, owner.context, (id) => owners.get(id))),
    [runs.data, owner.context, owners],
  );
  const view = runsView(inContext, filters);

  const facetRows: FacetRow[] = [
    {
      label: 'Status',
      options: view.status,
      value: filters.status,
      onChange: (status) => {
        patch({ status });
      },
    },
    {
      label: 'Playbook',
      options: view.playbook,
      value: filters.playbook,
      onChange: (playbook) => {
        patch({ playbook });
      },
      maxVisible: 8,
    },
    {
      label: 'Origin',
      options: view.origin,
      value: filters.origin,
      onChange: (origin) => {
        patch({ origin });
      },
    },
  ];

  const applied: AppliedFilter[] = [];
  for (const axis of ['status', 'playbook', 'origin'] as const) {
    if (filters[axis]) {
      applied.push({
        label: `${axis}: ${filters[axis]}`,
        onClear: () => {
          patch({ [axis]: '' });
        },
      });
    }
  }
  if (view.hiddenDrafts > 0) {
    applied.push({
      label: `${view.hiddenDrafts} draft run${view.hiddenDrafts === 1 ? '' : 's'} hidden`,
      onClear: () => {
        patch({ drafts: true });
      },
    });
  }

  const runRows = view.rows;
  const table = useDataTable({
    columns: runColumns,
    data: runRows,
    getRowId: (run) => run.key,
  });

  return (
    <>
      <PageHeader
        eyebrow="Execution"
        title="Playbook runs"
        description="Ad-hoc playbook launches with the values they froze, what they cost, and the deferred ones still waiting."
      />

      <Facets rows={facetRows} />
      <Applied
        shown={runRows.length}
        total={inContext.length}
        noun="runs"
        filters={applied}
        onClearAll={() => {
          setSearchParams(runFilterParams(DEFAULT_FILTERS), { replace: true });
        }}
      />
      <QueryState query={runs} noun="runs">
        <DataTable
          table={table}
          empty={<Empty title={inContext.length === 0 ? 'No playbook runs' : 'No runs match these filters'} />}
          footer={
            <>
              Showing {runRows.length} of {inContext.length}
              {filters.drafts ? (
                <>
                  {' · '}
                  <button
                    type="button"
                    className="cursor-pointer border-0 bg-transparent p-0 font-mono text-ink-3 underline hover:text-ink"
                    onClick={() => {
                      patch({ drafts: false });
                    }}
                  >
                    hide draft runs
                  </button>
                </>
              ) : null}
            </>
          }
        />
      </QueryState>

      {oneShots.isSuccess && oneShotRows.length === 0 ? null : (
        <Section>
          <SectionHeader title="Deferred one-shots" />
          <SectionBody>
            <QueryState query={oneShots} noun="one-shots">
              <OneShotList rows={oneShotRows} />
            </QueryState>
          </SectionBody>
        </Section>
      )}
    </>
  );
}

function OneShotList({ rows }: { rows: OneShotDto[] }) {
  return (
    <ul className="m-0 list-none p-0">
      {rows.map((row) => (
        <li key={row.id} className="flex items-center gap-3 border-b border-rule py-1.5 last:border-b-0">
          <Status status={row.status} tone={statusTone(ONE_SHOT_COLOR[row.status] ?? 'grey')} />
          <Mono size="data">{row.playbook}</Mono>
          <Mono size="data" tone="ink-2">
            fires {formatStamp(row.fire_at)}
          </Mono>
          {row.fired_key ? (
            <Identifier to={launchPath(row.fired_key)}>{row.fired_key}</Identifier>
          ) : null}
          <span className="flex-1" />
          {row.actions.includes('delete') && row.status === 'pending' ? (
            <CancelButton id={row.id} />
          ) : null}
        </li>
      ))}
    </ul>
  );
}

function CancelButton({ id }: { id: string }) {
  const queryClient = useQueryClient();
  const mutation = $api.useMutation('delete', '/api/one-shots/{id}');
  return (
    <Button
      className="uppercase"
      disabled={mutation.isPending}
      onClick={() => {
        mutation.mutate(
          { params: { path: { id } } },
          {
            onSuccess: () => {
              void queryClient.invalidateQueries({ queryKey: ['get', '/api/one-shots'] });
            },
          }
        );
      }}
    >
      {mutation.isPending ? 'Cancelling…' : 'Cancel'}
    </Button>
  );
}
