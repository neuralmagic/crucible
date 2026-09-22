import { useQueryClient } from '@tanstack/react-query';
import { Link } from 'react-router-dom';
import { $api } from '../api/client';
import type { components } from '../api/schema';
import { narrow } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  Empty,
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
import { issueStatusColor } from './issueStatus';
import { launchPath, relaunchPath } from './launchView';
import { formatCost, transportLossLabel } from './runReport';

type PlaybookRunDto = components['schemas']['PlaybookRunDto'];
type OneShotDto = components['schemas']['OneShotView'];

const EMPTY_RUNS: PlaybookRunDto[] = [];
const EMPTY_ONE_SHOTS: OneShotDto[] = [];

const ONE_SHOT_COLOR: Record<string, string> = {
  pending: 'blue',
  fired: 'green',
  canceled: 'grey',
  failed: 'red',
};

/// The frozen param snapshot, collapsed. It is what a relaunch re-renders the form from, so the
/// exact values a run was authorized with stay readable next to its outcome.
function ParamSnapshot({ params }: { params: PlaybookRunDto['params'] }) {
  const entries = Object.entries(params);
  if (entries.length === 0) return <Mono tone="ink-3">no params</Mono>;
  return (
    <details>
      <summary className="cursor-pointer font-mono text-data text-ink-2">
        {entries.length} param{entries.length === 1 ? '' : 's'}
      </summary>
      <dl className="mt-1 grid grid-cols-[auto_1fr] gap-x-3 font-mono text-data">
        {entries.map(([name, value]) => (
          <div key={name} className="contents">
            <dt className="text-ink-3">{name}</dt>
            <dd className="m-0 break-all text-ink-2">{String(value)}</dd>
          </div>
        ))}
      </dl>
    </details>
  );
}

const runHelper = createDataColumnHelper<PlaybookRunDto>();

const runColumns = runHelper.columns([
  runHelper.accessor('key', {
    header: 'Launch',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={launchPath(getValue())}>{getValue()}</Identifier>
    ),
  }),
  runHelper.accessor('playbook', {
    header: 'Playbook',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
  }),
  runHelper.accessor('origin', {
    header: 'Origin',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
  }),
  runHelper.accessor('status', {
    header: 'Outcome',
    enableSorting: false,
    cell: ({ row }) => {
      const lost = transportLossLabel(row.original.transport_losses);
      return (
        <span className="flex flex-wrap items-center gap-2">
          <Status
            status={row.original.parked_reason ?? row.original.status}
            tone={statusTone(issueStatusColor(row.original.status))}
            pulse={row.original.status === 'running'}
          />
          {lost && <Status status={lost} tone="red" pulse={false} />}
        </span>
      );
    },
  }),
  runHelper.display({
    id: 'params',
    header: 'Snapshot',
    cell: ({ row }) => <ParamSnapshot params={row.original.params} />,
  }),
  runHelper.display({
    id: 'ceilings',
    header: 'Ceilings',
    meta: { className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) => `$${row.original.max_cost.toFixed(2)} / ${row.original.max_time}`,
  }),
  runHelper.display({
    id: 'cost',
    header: 'Cost',
    meta: { align: 'end', className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) => formatCost(row.original.cost_usd),
  }),
  runHelper.accessor('advance_dedupe', {
    header: 'Dedupe',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-3' },
    cell: ({ getValue }) => (getValue() ? 'advances' : 'untouched'),
  }),
  runHelper.accessor('created_at', {
    header: 'Launched',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => formatStamp(getValue()),
  }),
  runHelper.display({
    id: 'relaunch',
    header: '',
    meta: { pad: 'tight', shrink: true },
    cell: ({ row }) => (
      <Button render={<Link to={relaunchPath(row.original.playbook, row.original.key)} />}>RELAUNCH</Button>
    ),
  }),
]);

export function PlaybookRunsPage() {
  const runs = $api.useQuery('get', '/api/playbook-runs');
  const oneShots = $api.useQuery('get', '/api/one-shots');
  const owner = useOwnerContext();
  const oneShotRows = narrow(oneShots.data ?? EMPTY_ONE_SHOTS, owner.context, (row) => row.owner_principal);

  const runRows = runs.data ?? EMPTY_RUNS;
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

      <QueryState query={runs} noun="RUNS">
        <DataTable
          table={table}
          empty={<Empty title="NO PLAYBOOK RUNS" />}
          footer={<>Showing {runRows.length}</>}
        />
      </QueryState>

      <Section>
        <SectionHeader title="Deferred one-shots" />
        <SectionBody>
          <QueryState query={oneShots} noun="ONE-SHOTS">
            <OneShotList rows={oneShotRows} />
          </QueryState>
        </SectionBody>
      </Section>
    </>
  );
}

function OneShotList({ rows }: { rows: OneShotDto[] }) {
  if (rows.length === 0) return <Empty title="NOTHING DEFERRED" />;
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
      {mutation.isPending ? 'CANCELLING…' : 'CANCEL'}
    </Button>
  );
}
