import { $api } from '../api/client';
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
  Status,
  useDataTable,
} from '../ui';
import type { StatusTone } from '../ui';
import { scheduleView, signinNeededCount, type ScheduleDto, type ScheduleState } from './schedulesView';

const EMPTY: ScheduleDto[] = [];

const TONE: Record<ScheduleState, StatusTone> = {
  signin: 'red',
  disabled: 'grey',
  failing: 'amber',
  live: 'green',
};

const helper = createDataColumnHelper<ScheduleDto>();

const columns = helper.columns([
  helper.accessor('playbook', {
    header: 'Playbook',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue, row }) => (
      <Identifier
        to={
          row.original.target_kind === 'draft_head'
            ? `/playbooks/drafts/${encodeURIComponent(getValue())}`
            : `/playbooks/${encodeURIComponent(getValue())}`
        }
      >
        {getValue()}{row.original.target_kind === 'draft_head' ? ' · MUTABLE' : ''}
      </Identifier>
    ),
  }),
  helper.display({
    id: 'revision',
    header: 'Target',
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) =>
      row.original.target_kind === 'draft_head'
        ? `draft head · v${row.original.eligible_draft_version ?? '—'}`
        : `adopted · ${(row.original.adopted_rev ?? '').slice(0, 7)}`,
  }),
  helper.display({
    id: 'recurrence',
    header: 'Fires',
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) => `${row.original.cron_expr} (${row.original.tz})`,
  }),
  helper.accessor('owner_principal', {
    header: 'Owner',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => getValue() ?? '—',
  }),
  helper.accessor('last_fired_at', {
    header: 'Last fired',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => formatStamp(getValue()),
  }),
  helper.display({
    id: 'state',
    header: 'State',
    meta: { shrink: true },
    cell: ({ row }) => {
      const view = scheduleView(row.original);
      return <Status status={view.headline} tone={TONE[view.state]} pulse={false} />;
    },
  }),
  helper.display({
    id: 'detail',
    header: 'Why',
    meta: { wrap: true },
    cell: ({ row }) => {
      const view = scheduleView(row.original);
      return (
        <div className="flex items-start gap-3">
          <Mono size="data" tone={view.state === 'live' ? 'ink-3' : 'ink-2'}>
            {view.detail}
          </Mono>
          {view.signInHref !== null && (
            <Button variant="filled" className="whitespace-nowrap" render={<a href={view.signInHref} />}>
              SIGN IN
            </Button>
          )}
        </div>
      );
    },
  }),
]);

/// Every schedule and whether it will actually fire: disabled, its owner's last failed group
/// refresh, and the sign-in that clears one.
export function SchedulesPage() {
  const schedules = $api.useQuery('get', '/api/schedules');
  const owner = useOwnerContext();
  const rows = narrow(schedules.data ?? EMPTY, owner.context, (row) => row.owner_principal);
  const table = useDataTable({ columns, data: rows, getRowId: (row) => row.id });
  const needSignin = signinNeededCount(rows);

  return (
    <>
      <PageHeader
        eyebrow="Queue"
        title="Schedules"
        description="Recurring playbook launches, when each fires next, and what is stopping the ones that will not."
      />

      <QueryState query={schedules} noun="SCHEDULES">
        <DataTable
          table={table}
          empty={
            <Empty
              title="NO SCHEDULES"
              description="Schedule a playbook from its launch form to see it here."
            />
          }
          footer={
            <>
              {rows.length} schedule{rows.length === 1 ? '' : 's'}
              {needSignin > 0 && `, ${needSignin} waiting on sign-in`}
            </>
          }
        />
      </QueryState>
    </>
  );
}
