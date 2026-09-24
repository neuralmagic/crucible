import { Link } from 'react-router-dom';
import { $api } from '../api/client';
import { narrow } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import type { components } from '../api/schema';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  Empty,
  Identifier,
  Mono,
  PageHeader,
  QueryState,
  useDataTable,
} from '../ui';
import { sourceLabel } from './playbookSource';

type PlaybookDto = components['schemas']['PlaybookDto'];

const EMPTY: PlaybookDto[] = [];

const helper = createDataColumnHelper<PlaybookDto>();

const columns = helper.columns([
  helper.accessor('id', {
    header: 'Playbook',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => (
      <Identifier to={`/playbooks/${encodeURIComponent(getValue())}`}>
        {getValue()}
      </Identifier>
    ),
  }),
  helper.accessor('description', { header: 'Description', enableSorting: false }),
  helper.accessor('owner', {
    header: 'Owner',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
  }),
  helper.display({
    id: 'pin',
    header: 'Pinned at',
    meta: { className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) =>
      `${sourceLabel(row.original.source)} @ ${row.original.rev.slice(0, 7)}`,
  }),
  helper.accessor('schema_digest', {
    header: 'Form',
    enableSorting: false,
    cell: ({ getValue }) => (
      <Mono size="data" tone="ink-3">
        {getValue()}
      </Mono>
    ),
  }),
  helper.display({
    id: 'inspect',
    header: '',
    meta: { align: 'end' },
    cell: ({ row }) => (
      <Button render={<Link to={`/playbooks/${encodeURIComponent(row.original.id)}`} />}>
        INSPECT
      </Button>
    ),
  }),
  helper.display({
    id: 'launch',
    header: '',
    meta: { align: 'end' },
    cell: ({ row }) => (
      <Button render={<Link to={`/playbooks/${encodeURIComponent(row.original.id)}/launch`} />}>
        LAUNCH
      </Button>
    ),
  }),
]);

/// The registry: which packs can be launched, where each is pinned, and the digest of the form its
/// schema renders. Registration is an API call; this page is the way into a launch.
export function PlaybooksPage() {
  const playbooks = $api.useQuery('get', '/api/playbooks');
  const owner = useOwnerContext();
  const rows = narrow(playbooks.data ?? EMPTY, owner.context, (row) => row.owner);
  const table = useDataTable({ columns, data: rows, getRowId: (row) => row.id });

  return (
    <>
      <PageHeader
        eyebrow="Queue"
        title="Playbooks"
        description="Registered packs, pinned to a commit. Launching one runs its graph once against the values you supply."
        actions={
          <Button variant="filled" render={<Link to="/playbooks/import" />}>
            IMPORT
          </Button>
        }
      />
      <QueryState query={playbooks} noun="PLAYBOOKS">
        <DataTable
          table={table}
          empty={<Empty title="NO PLAYBOOKS REGISTERED" />}
          footer={<>Showing {rows.length}</>}
        />
      </QueryState>
    </>
  );
}
