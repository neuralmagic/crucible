import { useState } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { $api } from '../api/client';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
import { formatError } from '../api/errors';
import {
  createDataColumnHelper,
  DataTable,
  Empty,
  Identifier,
  Mono,
  PageHeader,
  QueryState,
  Spinner,
  Status,
  statusTone,
  Toolbar,
  ToolbarGroup,
  useDataTable,
} from '../ui';
import type { ToolbarOption } from '../ui';
import { TURN_KINDS, TURN_STATES, turnDuration, truncateReason, turnStateColor } from './turns';
import { Stamp } from './Stamp';

type WorkPodDto = components['schemas']['WorkPodDto'];

const ALL_OPTION: ToolbarOption<string> = { value: '', label: 'All' };

const STATE_FILTERS: readonly ToolbarOption<string>[] = [
  ALL_OPTION,
  ...TURN_STATES.map((state) => ({ value: state, label: state })),
];

const KIND_FILTERS: readonly ToolbarOption<string>[] = [
  ALL_OPTION,
  ...TURN_KINDS.map((kind) => ({ value: kind, label: kind })),
];

const EMPTY_TURNS: WorkPodDto[] = [];

const helper = createDataColumnHelper<WorkPodDto>();

const columns = helper.columns([
  helper.accessor('pod_name', {
    header: 'Pod',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => <Identifier>{getValue()}</Identifier>,
  }),
  helper.accessor('kind', {
    header: 'Kind',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => (
      <Mono size="label" uppercase tone="ink-3">
        {getValue()}
      </Mono>
    ),
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
  helper.accessor('state', {
    header: 'State',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ row }) => (
      <span className="flex items-center gap-2.5">
        <Status status={row.original.state} tone={statusTone(turnStateColor(row.original.state))} />
        {row.original.state === 'running' && (
          <Link
            to={`/turns/${encodeURIComponent(row.original.pod_name)}/live`}
            className="font-mono text-label uppercase tracking-label text-ink-3 underline-offset-2 hover:text-ink hover:underline"
          >
            live
          </Link>
        )}
      </span>
    ),
  }),
  helper.display({
    id: 'error',
    header: 'Failure reason',
    meta: { wrap: true, width: '32%', className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) => truncateReason(row.original.error) ?? <Mono tone="ink-3">—</Mono>,
  }),
  helper.accessor('cost_tag', {
    header: 'Cost tag',
    enableSorting: false,
    meta: { className: 'font-mono text-data text-ink-2' },
  }),
  helper.accessor('created_at', {
    header: 'Created',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => <Stamp iso={getValue()} />,
  }),
  helper.accessor('terminal_at', {
    header: 'Ended',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => <Stamp iso={getValue()} />,
  }),
  helper.display({
    id: 'duration',
    header: 'Duration',
    meta: { align: 'end', className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) => turnDuration(row.original.created_at, row.original.terminal_at) ?? '—',
  }),
]);

export function TurnsPage() {
  useLiveEvents();

  // Filters seed from the URL (`/turns?state=failed` from the dashboard failures card) and live
  // in state from there — in-page filter changes don't rewrite the URL.
  const [searchParams] = useSearchParams();
  const [state, setState] = useState(searchParams.get('state') ?? '');
  const [kind, setKind] = useState(searchParams.get('kind') ?? '');

  const turns = $api.useQuery('get', '/api/turns', {
    params: {
      query: {
        state: state || undefined,
        kind: kind || undefined,
      },
    },
  });

  const rows = turns.data ?? EMPTY_TURNS;

  const table = useDataTable({
    columns,
    data: rows,
    getRowId: (turn) => turn.pod_name,
  });

  return (
    <>
      <PageHeader
        eyebrow="Execution"
        title="Turns"
        description="Every work pod the controller has launched: ranking, scoping, and loop turns."
      />

      <Toolbar>
        <ToolbarGroup label="State" options={STATE_FILTERS} value={state} onChange={setState} />
        <ToolbarGroup label="Kind" options={KIND_FILTERS} value={kind} onChange={setKind} />
      </Toolbar>

      <QueryState query={turns} noun="TURNS">
        <DataTable
          table={table}
          renderSubRow={(row) => <TurnDetail podName={row.original.pod_name} />}
          empty={<Empty title="NO TURNS" description="No work pods match the current filters." />}
          footer={
            <>
              {rows.length} turn{rows.length === 1 ? '' : 's'}
            </>
          }
        />
      </QueryState>
    </>
  );
}

/// The full `result`/`error` for one turn. `GET /api/turns` clips both (a failed turn's error is an
/// anyhow chain wrapping pod log material, and the list is unpaginated), so the expanded row goes
/// back to the detail endpoint for the whole text.
function TurnDetail({ podName }: { podName: string }) {
  const detail = $api.useQuery('get', '/api/turns/{pod_name}', {
    params: { path: { pod_name: podName } },
  });

  if (detail.isPending) {
    return (
      <div className="px-4.5 py-3.5">
        <Spinner />
      </div>
    );
  }

  if (detail.isError) {
    return (
      <div className="px-4.5 py-3.5 text-ink-2">
        Error loading turn details: {formatError(detail.error)}
      </div>
    );
  }

  const { error, result } = detail.data;

  if (!error?.text && !result?.text) {
    return (
      <div className="px-4.5 py-3.5">
        <Mono tone="ink-3">No failure reason or result recorded for this turn.</Mono>
      </div>
    );
  }

  return (
    <div className="grid gap-3.5 px-4.5 py-3.5">
      {error?.text && (
        <div>
          <Mono size="micro" tone="ink-3" uppercase className="tracking-section">
            Failure reason
          </Mono>
          <pre className="m-0 mt-1 font-mono text-data leading-[1.5] whitespace-pre-wrap wrap-anywhere text-ink">
            {error.text}
          </pre>
        </div>
      )}
      {result?.text && (
        <div>
          <Mono size="micro" tone="ink-3" uppercase className="tracking-section">
            Result
          </Mono>
          <pre className="m-0 mt-1 font-mono text-data leading-[1.5] whitespace-pre-wrap wrap-anywhere text-ink-2">
            {result.text}
          </pre>
        </div>
      )}
    </div>
  );
}
