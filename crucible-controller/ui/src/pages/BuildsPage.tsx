import { useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
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
  statusTone,
  Toolbar,
  ToolbarGroup,
  Tooltip,
  useDataTable,
} from '../ui';
import type { ToolbarOption } from '../ui';
import { formatDuration } from './journeyView';

type BuildDto = components['schemas']['BuildDto'];

// A build is only rebuildable once it has landed on a terminal outcome — in-flight builds 409.
const TERMINAL_BUILD_STATES = new Set(['succeeded', 'failed', 'timed-out']);

const PAGE_SIZE = 50;

const STATE_FILTERS: readonly ToolbarOption<string>[] = [
  { value: '', label: 'All' },
  { value: 'pending', label: 'pending' },
  { value: 'dispatched', label: 'dispatched' },
  { value: 'succeeded', label: 'succeeded' },
  { value: 'failed', label: 'failed' },
  { value: 'timed-out', label: 'timed-out' },
];

const BACKEND_FILTERS: readonly ToolbarOption<string>[] = [
  { value: '', label: 'All' },
  { value: 'cluster', label: 'cluster' },
  { value: 'github-actions', label: 'github-actions' },
];

// A build's lifecycle color: succeeded green, in-flight blue, failure red/orange.
const BUILD_STATE_COLOR: Record<string, string> = {
  pending: 'grey',
  dispatched: 'blue',
  succeeded: 'green',
  failed: 'red',
  'timed-out': 'orange',
};

const EMPTY_BUILDS: BuildDto[] = [];

/// Whole seconds between two RFC3339 stamps, or null when either is missing.
function elapsed(from: string | null | undefined, to: string | null | undefined): number | null {
  if (!from || !to) return null;
  const a = new Date(from).getTime();
  const b = new Date(to).getTime();
  if (Number.isNaN(a) || Number.isNaN(b)) return null;
  return Math.max(0, Math.floor((b - a) / 1000));
}

/// Abbreviate a pinned `repo@sha256:<64hex>` for the table; the full ref rides the tooltip.
function shortDigest(ref: string): string {
  const at = ref.indexOf('@sha256:');
  if (at === -1) return ref;
  return `${ref.slice(0, at)}@sha256:${ref.slice(at + 8, at + 16)}…`;
}

const helper = createDataColumnHelper<BuildDto>();

const baseColumns = [
  helper.accessor('name', {
    header: 'Build',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => <Identifier>{getValue()}</Identifier>,
  }),
  helper.accessor('issue_key', {
    header: 'Issue',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => {
      const key = getValue();
      return key ? (
        <Identifier to={`/issues/${encodeURIComponent(key)}`}>{key}</Identifier>
      ) : (
        <Mono tone="ink-3">—</Mono>
      );
    },
  }),
  helper.accessor('backend', {
    header: 'Backend',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
  }),
  helper.accessor('state', {
    header: 'State',
    enableSorting: false,
    cell: ({ row }) => (
      <span className="flex items-center gap-2.5">
        <Status
          status={row.original.state}
          tone={statusTone(BUILD_STATE_COLOR[row.original.state] ?? 'grey')}
          pulse={row.original.state === 'dispatched'}
        />
        {row.original.evidence_url && (
          <a
            href={row.original.evidence_url}
            target="_blank"
            rel="noopener noreferrer"
            className="font-mono text-label uppercase tracking-label text-ink-3 underline-offset-2 hover:text-ink hover:underline"
          >
            log ↗
          </a>
        )}
      </span>
    ),
  }),
  helper.accessor('digest_ref', {
    id: 'image',
    header: 'Image',
    enableSorting: false,
    cell: ({ row }) => {
      const ref = row.original.digest_ref;
      if (!ref) {
        return (
          <Mono tone="ink-3">
            {row.original.image}:{row.original.tag}
          </Mono>
        );
      }
      return (
        <Tooltip content={ref}>
          <Mono>{shortDigest(ref)}</Mono>
        </Tooltip>
      );
    },
  }),
  helper.display({
    id: 'took',
    header: 'Took',
    meta: { align: 'end', className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) =>
      formatDuration(elapsed(row.original.dispatched_at, row.original.finished_at)) ?? '—',
  }),
  helper.accessor('created_at', {
    header: 'Created',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => formatStamp(getValue()),
  }),
];

const columns = helper.columns(baseColumns);

const adminColumns = helper.columns([
  ...baseColumns,
  helper.display({
    id: 'rebuild',
    header: 'Rebuild',
    meta: { pad: 'tight', shrink: true },
    cell: ({ row }) => (
      <RebuildButton id={row.original.id} name={row.original.name} state={row.original.state} />
    ),
  }),
]);

export function BuildsPage() {
  useLiveEvents();

  const [searchParams] = useSearchParams();
  const [state, setState] = useState(searchParams.get('state') ?? '');
  const [backend, setBackend] = useState(searchParams.get('backend') ?? '');
  const [page, setPage] = useState(0);

  const query = {
    ...(state ? { state } : {}),
    ...(backend ? { backend } : {}),
    limit: PAGE_SIZE,
    offset: page * PAGE_SIZE,
  };

  const whoami = $api.useQuery('get', '/api/whoami');
  const isAdmin = whoami.data?.role === 'admin';
  const builds = $api.useQuery('get', '/api/builds', { params: { query } });
  const rows = builds.data ?? EMPTY_BUILDS;

  const table = useDataTable({
    columns: isAdmin ? adminColumns : columns,
    data: rows,
    getRowId: (build) => String(build.id),
  });

  return (
    <>
      <PageHeader
        eyebrow="Execution"
        title="Builds"
        description="Every candidate image the broker has built, and what it pinned."
      />

      <Toolbar>
        <ToolbarGroup
          label="State"
          options={STATE_FILTERS}
          value={state}
          onChange={(value) => {
            setState(value);
            setPage(0);
          }}
        />
        <ToolbarGroup
          label="Backend"
          options={BACKEND_FILTERS}
          value={backend}
          onChange={(value) => {
            setBackend(value);
            setPage(0);
          }}
        />
      </Toolbar>

      <QueryState query={builds} noun="BUILDS">
        <DataTable
          table={table}
          empty={<Empty title="NO BUILDS" />}
          footer={<>Showing {rows.length}</>}
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

/// Admin-only force-rebuild: resets a terminal build back to `pending` so the existing `building`
/// reconcile re-drives it. Disabled (not hidden — the column stays put) while a build is in flight,
/// matching the endpoint's 409 on `pending`/`dispatched`.
function RebuildButton({ id, name, state }: { id: number; name: string; state: string }) {
  const queryClient = useQueryClient();
  const mutation = $api.useMutation('post', '/api/builds/{id}/rebuild');
  const rebuildable = TERMINAL_BUILD_STATES.has(state);

  return (
    <Tooltip content={rebuildable ? `Force-rebuild ${name}` : 'Only a terminal build can be rebuilt'}>
      <Button
        disabled={!rebuildable || mutation.isPending}
        onClick={() => {
          mutation.mutate(
            { params: { path: { id } } },
            {
              onSuccess: () => {
                void queryClient.invalidateQueries({ queryKey: ['get', '/api/builds'] });
              },
            },
          );
        }}
      >
        {mutation.isPending ? 'REBUILDING…' : 'REBUILD'}
      </Button>
    </Tooltip>
  );
}
