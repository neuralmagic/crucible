import { $api } from '../api/client';
import { narrow } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import {
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
import { STATE_TONE, webhookView, type WebhookDto } from './webhooksView';

const EMPTY: WebhookDto[] = [];

const helper = createDataColumnHelper<WebhookDto>();

const columns = helper.columns([
  helper.accessor('id', {
    header: 'Webhook',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={`/webhooks/${encodeURIComponent(getValue())}`}>{getValue().slice(0, 13)}</Identifier>
    ),
  }),
  helper.accessor('playbook', {
    header: 'Playbook',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={`/playbooks/${encodeURIComponent(getValue())}`}>{getValue()}</Identifier>
    ),
  }),
  helper.accessor('verifier', {
    header: 'Verifier',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
  }),
  helper.accessor('max_launches_per_hour', {
    header: 'Rate',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => `${getValue()}/h`,
  }),
  helper.accessor('owner_principal', {
    header: 'Owner',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => getValue() ?? '—',
  }),
  helper.accessor('last_delivery_at', {
    header: 'Last delivery',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => formatStamp(getValue()),
  }),
  helper.display({
    id: 'state',
    header: 'State',
    meta: { shrink: true },
    cell: ({ row }) => {
      const view = webhookView(row.original);
      return <Status status={view.headline} tone={STATE_TONE[view.state]} pulse={false} />;
    },
  }),
  helper.display({
    id: 'detail',
    header: 'Why',
    meta: { wrap: true },
    cell: ({ row }) => {
      const view = webhookView(row.original);
      return (
        <Mono size="data" tone={view.state === 'live' ? 'ink-3' : 'ink-2'}>
          {view.detail}
        </Mono>
      );
    },
  }),
]);

/// Every webhook the caller may read and whether it will launch: disabled, its owner's failed group
/// refresh, or when it last took a delivery.
export function WebhooksPage() {
  const webhooks = $api.useQuery('get', '/api/webhooks');
  const owner = useOwnerContext();
  const rows = narrow(webhooks.data ?? EMPTY, owner.context, (row) => row.owner_principal);
  const table = useDataTable({ columns, data: rows, getRowId: (row) => row.id });

  return (
    <>
      <PageHeader
        eyebrow="Queue"
        title="Webhooks"
        description="Playbooks that launch when a sender outside the controller posts a delivery, and what each delivery became."
      />

      <QueryState query={webhooks} noun="webhooks">
        <DataTable
          table={table}
          empty={
            <Empty
              title="No webhooks"
              description="Add a webhook from a playbook's page to see it here."
            />
          }
          footer={
            <>
              {rows.length} webhook{rows.length === 1 ? '' : 's'}
            </>
          }
        />
      </QueryState>
    </>
  );
}
