import { useMemo, useState } from 'react';
import { $api } from '../api/client';
import { Check } from './formControls';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  Empty,
  Identifier,
  Mono,
  PageHeader,
  QueryState,
  Toolbar,
  ToolbarActions,
  ToolbarSearch,
  useDataTable,
} from '../ui';
import { StaleBadge } from './StaleBadge';
import { detailPath } from './launchView';

type IssueDto = components['schemas']['IssueDto'];

const EMPTY_ISSUES: IssueDto[] = [];

const helper = createDataColumnHelper<IssueDto>();

export function InboxPage() {
  useLiveEvents();

  const [searchFilter, setSearchFilter] = useState('');
  const [selectedKeys, setSelectedKeys] = useState<Set<string>>(new Set());
  const [bulkResult, setBulkResult] = useState<{ success: number; failed: number } | null>(null);

  const whoami = $api.useQuery('get', '/api/whoami');
  const isOperator = whoami.data?.role === 'admin' || whoami.data?.role === 'operator';

  const issues = $api.useQuery('get', '/api/issues', {
    params: {
      query: {
        status: 'parked',
        exclude_kind: 'playbook',
      },
    },
  });

  const unparkMutation = $api.useMutation('post', '/api/issues/{key}/unpark');

  const data = issues.data ?? EMPTY_ISSUES;

  const filteredData = useMemo(() => {
    if (!searchFilter) return data;
    const search = searchFilter.toLowerCase();
    return data.filter(
      (issue) =>
        issue.key.toLowerCase().includes(search) ||
        (issue.parked_reason != null && issue.parked_reason.text.toLowerCase().includes(search)),
    );
  }, [data, searchFilter]);

  const handleUnpark = async (key: string) => {
    try {
      await unparkMutation.mutateAsync({
        params: {
          path: {
            key,
          },
        },
        body: {
          reason: null,
        },
      });
      void issues.refetch();
    } catch (error) {
      console.error('Unpark failed:', error);
    }
  };

  const handleBulkUnpark = async () => {
    const results = await Promise.allSettled(
      Array.from(selectedKeys).map((key) =>
        unparkMutation.mutateAsync({
          params: {
            path: { key },
          },
          body: {
            reason: null,
          },
        })
      )
    );

    const success = results.filter((r) => r.status === 'fulfilled').length;
    const failed = results.filter((r) => r.status === 'rejected').length;

    setBulkResult({ success, failed });
    setSelectedKeys(new Set());
    void issues.refetch();

    setTimeout(() => setBulkResult(null), 5000);
  };

  const toggleSelection = (key: string) => {
    setSelectedKeys((prev) => {
      const next = new Set(prev);
      if (next.has(key)) {
        next.delete(key);
      } else {
        next.add(key);
      }
      return next;
    });
  };

  const toggleSelectAll = () => {
    if (selectedKeys.size === filteredData.length) {
      setSelectedKeys(new Set());
    } else {
      setSelectedKeys(new Set(filteredData.map((issue) => issue.key)));
    }
  };

  const buildColumns = () => {
    const allSelected = selectedKeys.size === filteredData.length && filteredData.length > 0;
    const select = helper.display({
      id: 'select',
      meta: { pad: 'tight', shrink: true },
      header: () => (
        <Check label="Select all parked issues" checked={allSelected} onChange={toggleSelectAll} />
      ),
      cell: ({ row }) => (
        <Check
          label={`Select ${row.original.key}`}
          checked={selectedKeys.has(row.original.key)}
          onChange={() => {
            toggleSelection(row.original.key);
          }}
        />
      ),
    });

    const actions = helper.display({
      id: 'actions',
      header: 'Actions',
      meta: { pad: 'tight', shrink: true },
      cell: ({ row }) => (
        <Button
          disabled={unparkMutation.isPending}
          onClick={() => {
            void handleUnpark(row.original.key);
          }}
        >
          UNPARK
        </Button>
      ),
    });

    const base = helper.columns([
      helper.accessor('key', {
        id: 'identifier',
        header: 'Key',
        enableSorting: false,
        meta: { pad: 'tight', shrink: true },
        cell: ({ getValue }) => (
          <Identifier to={detailPath(getValue())} title={getValue()}>
            {getValue()}
          </Identifier>
        ),
      }),
      helper.display({
        id: 'reason',
        header: 'Reason',
        meta: { wrap: true, width: '52%' },
        cell: ({ row }) => (
          <span className="flex items-start gap-2">
            <span className="text-ink-2">{row.original.parked_reason?.text || '—'}</span>
            <StaleBadge
              staleClosable={row.original.stale_closable}
              parkedReason={row.original.parked_reason}
            />
          </span>
        ),
      }),
      helper.accessor('parked_by', {
        header: 'Parked by',
        enableSorting: false,
        meta: { shrink: true },
        cell: ({ getValue }) => <Mono>{getValue() || '—'}</Mono>,
      }),
    ]);

    if (!isOperator) return base;
    return [select, ...base, actions];
  };

  const table = useDataTable({
    columns: buildColumns(),
    data: filteredData,
    getRowId: (issue) => issue.key,
  });

  return (
    <>
      <PageHeader
        eyebrow="Queue"
        title="Inbox"
        description="Issues the loop parked and stopped spending on. Unpark one to put it back in the queue."
      />

      <Toolbar>
        <ToolbarSearch
          value={searchFilter}
          onChange={setSearchFilter}
          placeholder="Search by key or reason…"
          aria-label="Search parked issues"
        />
        {isOperator && (
          <ToolbarActions>
            {bulkResult && (
              <Mono size="label" tone={bulkResult.failed > 0 ? 'amber' : 'green'} className="flex items-center border-r border-rule px-2.5">
                unparked {bulkResult.success}
                {bulkResult.failed > 0 ? `, ${bulkResult.failed} failed` : ''}
              </Mono>
            )}
            <Button
              variant="filled"
              disabled={selectedKeys.size === 0 || unparkMutation.isPending}
              onClick={() => {
                void handleBulkUnpark();
              }}
            >
              UNPARK SELECTED ({selectedKeys.size})
            </Button>
          </ToolbarActions>
        )}
      </Toolbar>

      <QueryState query={issues} noun="INBOX">
        <DataTable
          table={table}
          empty={
            <Empty
              title={searchFilter ? 'NO MATCHING ISSUES' : 'NOTHING PARKED'}
              description={
                searchFilter
                  ? 'No parked issues match the search filter.'
                  : 'No issues are currently parked.'
              }
            />
          }
          footer={
            <>
              {filteredData.length} of {data.length} parked
            </>
          }
        />
      </QueryState>
    </>
  );
}
