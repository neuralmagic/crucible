import { AlertDialog } from '@base-ui-components/react/alert-dialog';
import { useQueryClient } from '@tanstack/react-query';
import type { SortingState, Updater } from '@tanstack/react-table';
import { useMemo, useState } from 'react';
import type { ReactNode } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
import {
  ALERT_POPUP,
  Applied,
  Button,
  createDataColumnHelper,
  DataTable,
  DIALOG_BACKDROP,
  DIALOG_TITLE,
  Empty,
  Facets,
  Identifier,
  Mono,
  PageHeader,
  PrLink,
  QueryState,
  Spinner,
  Status,
  statusTone,
  Toolbar,
  ToolbarActions,
  ToolbarSearch,
  useDataTable,
} from '../ui';
import type { AppliedFilter, FacetOption, FacetRow, MonoTone, ToolbarOption } from '../ui';
import { ISSUE_STATUSES, issueStatusColor, tierTone } from './issueStatus';
import { Stamp } from './Stamp';
import {
  DEFAULT_RECENCY,
  ISSUE_KINDS,
  ISSUE_KIND_LABELS,
  RECENCY_LABELS,
  RECENCY_PRESETS,
  UPSTREAM_STATES,
  parseKind,
  parseRecency,
  parseSortDir,
  parseSortKey,
  parseUpstream,
  recencyCutoff,
  splitRepo,
  type IssueSortKey,
} from './issuesFilters';
import { filterStaleClosable } from './parkedStale';
import { StaleBadge } from './StaleBadge';
import { KindLabel } from './KindLabel';
import { rankingRationale } from './rankProvenance';
import { detailPath } from './launchView';

type IssueDto = components['schemas']['IssueDto'];
type InputKindDto = components['schemas']['InputKindDto'];

const TIER_OPTIONS = ['T0', 'T1', 'T2', 'T3', 'N'];

const AFFINITY_OPTIONS = ['perf', 'perf-adjacent', 'unrelated'] as const;

const UPSTREAM_FILTERS: ToolbarOption<string>[] = [
  { value: '', label: 'Any' },
  ...UPSTREAM_STATES.map((state) => ({ value: state, label: state })),
];

const RECENCY_FILTERS: ToolbarOption<string>[] = RECENCY_PRESETS.map((preset) => ({
  value: preset,
  label: preset,
}));

/// Column id per sortable key. `updated` stays a valid wire sort (URL-seeded) but has no column,
/// so it maps to no sorted header.
const SORT_COLUMNS: Record<IssueSortKey, string | null> = {
  title: 'title',
  tier: 'tier',
  priority: 'priority',
  upstream: 'upstream',
  updated: null,
};

const EMPTY_ISSUES: IssueDto[] = [];

/// The short identifier the row is clicked by. Storage keys are addresses (`owner/repo#1489`,
/// `jira:{site}:ACME-8332`, `scenario:{uuid}`); this renders the part a human says out loud,
/// with the full key on hover. A scenario's uuid has no human form, so its tail stands in.
function displayKey(kind: InputKindDto, key: string): string {
  switch (kind.type) {
    case 'github':
      return `GH-${kind.number}`;
    case 'jira':
      return `${kind.project}-${kind.number}`;
    case 'scenario':
      return `SCN-${kind.id.slice(-6).toUpperCase()}`;
    case 'playbook':
      return `${kind.playbook.toUpperCase()}-${kind.launch.slice(-6).toUpperCase()}`;
    case 'unknown':
      return key;
  }
}

function affinityTone(affinity: string): MonoTone {
  if (affinity === 'perf') return 'green';
  if (affinity === 'perf-adjacent') return 'blue';
  return 'ink-3';
}

/// `owner/name` with the owner de-emphasized — multi-repo is real, the name is what you scan for.
function RepoCell({ repo }: { repo: string }) {
  const { owner, name } = splitRepo(repo);
  return (
    <>
      {owner && <span className="text-ink-3">{owner}/</span>}
      <span className="font-medium">{name}</span>
    </>
  );
}

type FacetCount = components['schemas']['FacetCount'];

/// Server counts, ordered by the domain's own vocabulary rather than alphabetically, with an "All"
/// option carrying the unfiltered total. A value the server never returned counts zero, which the
/// facet block renders inert.
function facetOptions(
  counts: FacetCount[] | undefined,
  order: readonly string[] | null,
  total: number | undefined,
  labelFor?: (value: string) => string,
  byCount = false,
): FacetOption[] {
  const known = counts !== undefined;
  const byValue = new Map((counts ?? []).map((c) => [c.value, c.count]));
  const ranked = byCount
    ? [...(counts ?? [])].sort((a, b) => b.count - a.count).map((c) => c.value)
    : null;
  const values = order ?? ranked ?? (counts ?? []).map((c) => c.value);
  return [
    { value: '', label: 'All', count: total },
    ...values.map((value) => ({
      value,
      label: labelFor ? labelFor(value) : value,
      title: labelFor ? value : undefined,
      count: known ? (byValue.get(value) ?? 0) : undefined,
    })),
  ];
}

/// `owner/name` renders as `name`. The owner is near-constant across the watched set and eats the
/// width the counts need.
function shortRepo(repo: string): string {
  const slash = repo.lastIndexOf('/');
  return slash === -1 ? repo : repo.slice(slash + 1);
}

const helper = createDataColumnHelper<IssueDto>();

const columns = helper.columns([
  helper.accessor('key', {
    id: 'identifier',
    header: 'Identifier',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ row }) => (
      <Identifier to={detailPath(row.original.key)} title={row.original.key}>
        {displayKey(row.original.kind, row.original.key)}
      </Identifier>
    ),
  }),
  helper.display({
    id: 'kind',
    header: 'Kind',
    meta: { shrink: true },
    cell: ({ row }) => <KindLabel kind={row.original.kind} />,
  }),
  helper.accessor('title', {
    header: 'Title',
    meta: { wrap: true, width: '42%' },
    cell: ({ row }) => (
      <span className="flex items-center gap-2">
        <Link
          to={detailPath(row.original.key)}
          className="font-medium underline-offset-2 hover:underline"
        >
          {row.original.title || row.original.key}
        </Link>
        {row.original.pr_url ? <PrLink url={row.original.pr_url} compact /> : null}
      </span>
    ),
  }),
  helper.accessor('repo', {
    header: 'Repo',
    enableSorting: false,
    meta: { hideNarrow: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => <RepoCell repo={getValue()} />,
  }),
  helper.display({
    id: 'status',
    header: 'Status',
    meta: { shrink: true },
    cell: ({ row }) => (
      <span className="flex items-center gap-2">
        <Status status={row.original.status} tone={statusTone(issueStatusColor(row.original.status))} />
        <StaleBadge
          staleClosable={row.original.stale_closable}
          parkedReason={row.original.parked_reason}
        />
      </span>
    ),
  }),
  helper.accessor('tier', {
    header: 'Tier',
    meta: { shrink: true },
    cell: ({ getValue }) => {
      const tier = getValue();
      if (!tier) return <Mono tone="ink-3">—</Mono>;
      return (
        <Mono tone={tierTone(tier)} weight="semibold">
          {tier}
        </Mono>
      );
    },
  }),
  helper.accessor('affinity', {
    header: 'Affinity',
    meta: { shrink: true },
    cell: ({ getValue }) => {
      const affinity = getValue();
      if (!affinity) return <Mono tone="ink-3">—</Mono>;
      return (
        <Mono tone={affinityTone(affinity)} weight="semibold">
          {affinity}
        </Mono>
      );
    },
  }),
  helper.accessor('priority', {
    header: 'Pri',
    meta: { shrink: true },
    cell: ({ getValue }) => {
      const priority = getValue();
      if (priority === 0) return <Mono tone="ink-3">—</Mono>;
      return <Mono weight="semibold">{priority}</Mono>;
    },
  }),
  helper.accessor('author', {
    header: 'Author',
    enableSorting: false,
    meta: { hideNarrow: true },
    cell: ({ getValue }) => <Mono>{getValue() || '—'}</Mono>,
  }),
  helper.accessor('upstream_updated_at', {
    id: 'upstream',
    header: 'Upstream',
    meta: { shrink: true },
    cell: ({ getValue }) => <Stamp iso={getValue()} />,
  }),
]);

export function IssuesPage() {
  useLiveEvents();

  // Filters seed from the URL (`/issues?status=pr-open` from the dashboard funnel,
  // `?repo=owner/name` from the repos page, `?recency=all` for the full backlog) and live in
  // state from there — in-page filter changes don't rewrite the URL.
  const [searchParams] = useSearchParams();
  const [statusFilter, setStatusFilter] = useState(searchParams.get('status') ?? '');
  const [tierFilter, setTierFilter] = useState(searchParams.get('tier') ?? '');
  const [affinityFilter, setAffinityFilter] = useState(searchParams.get('affinity') ?? '');
  const [repoFilter, setRepoFilter] = useState(searchParams.get('repo') ?? '');
  const [labelFilter, setLabelFilter] = useState(searchParams.get('label') ?? '');
  const [kindFilter, setKindFilter] = useState(parseKind(searchParams.get('kind')));
  const [upstreamFilter, setUpstreamFilter] = useState(parseUpstream(searchParams.get('upstream')));
  const [recency, setRecency] = useState(parseRecency(searchParams.get('recency')));
  const [closableOnly, setClosableOnly] = useState(searchParams.get('closable') === '1');
  const [sortKey, setSortKey] = useState<IssueSortKey>(parseSortKey(searchParams.get('sort')));
  const [sortDir, setSortDir] = useState(parseSortDir(searchParams.get('dir')));

  // Pin the cutoff per preset choice — recomputing "now" every render would churn the query key.
  const upstreamSince = useMemo(() => recencyCutoff(recency, new Date()), [recency]);

  const whoami = $api.useQuery('get', '/api/whoami');
  const isAdmin = whoami.data?.role === 'admin';

  const issues = $api.useQuery('get', '/api/issues', {
    params: {
      query: {
        status: statusFilter || undefined,
        tier: tierFilter || undefined,
        affinity: affinityFilter || undefined,
        repo: repoFilter || undefined,
        label: labelFilter || undefined,
        kind: kindFilter || undefined,
        exclude_kind: kindFilter ? undefined : 'playbook',
        upstream: upstreamFilter || undefined,
        upstream_since: upstreamSince,
        sort: sortKey,
        dir: sortDir,
      },
    },
  });

  const facets = $api.useQuery('get', '/api/issues/facets', {
    params: {
      query: {
        status: statusFilter || undefined,
        tier: tierFilter || undefined,
        affinity: affinityFilter || undefined,
        repo: repoFilter || undefined,
        label: labelFilter || undefined,
        kind: kindFilter || undefined,
        exclude_kind: kindFilter ? undefined : 'playbook',
        upstream: upstreamFilter || undefined,
        upstream_since: upstreamSince,
      },
    },
  });

  const facetRows = useMemo<FacetRow[]>(() => {
    const f = facets.data;
    return [
      {
        label: 'Kind',
        options: facetOptions(f?.kind, ISSUE_KINDS, f?.total, (v) => ISSUE_KIND_LABELS[parseKind(v) || 'github']),
        value: kindFilter,
        onChange: (value) => {
          setKindFilter(parseKind(value));
        },
      },
      {
        label: 'Status',
        options: facetOptions(f?.status, ISSUE_STATUSES, f?.total),
        value: statusFilter,
        onChange: setStatusFilter,
      },
      {
        label: 'Tier',
        options: facetOptions(f?.tier, TIER_OPTIONS, f?.total),
        value: tierFilter,
        onChange: setTierFilter,
      },
      {
        label: 'Affinity',
        options: facetOptions(f?.affinity, AFFINITY_OPTIONS, f?.total),
        value: affinityFilter,
        onChange: setAffinityFilter,
      },
      {
        label: 'Repo',
        options: facetOptions(f?.repo, null, f?.total, shortRepo, true),
        value: repoFilter,
        onChange: setRepoFilter,
        maxVisible: 6,
      },
      {
        label: 'Upstream',
        options: [...UPSTREAM_FILTERS, { value: 'closable', label: 'closable' }],
        value: closableOnly ? 'closable' : upstreamFilter,
        onChange: (value) => {
          setClosableOnly(value === 'closable');
          setUpstreamFilter(value === 'closable' ? '' : parseUpstream(value));
        },
      },
      {
        label: 'Seen',
        options: RECENCY_FILTERS,
        value: recency,
        onChange: (value) => {
          setRecency(parseRecency(value));
        },
      },
    ];
  }, [facets.data, kindFilter, statusFilter, tierFilter, affinityFilter, repoFilter, upstreamFilter, recency, closableOnly]);

  // "Closable upstream" narrows on top of whatever the query above already returned — a client-side
  // pass, not a wire filter, so it composes with every other filter instead of replacing them.
  const visibleIssues = useMemo(
    () => filterStaleClosable(issues.data ?? EMPTY_ISSUES, closableOnly),
    [issues.data, closableOnly],
  );

  const clearFilters = () => {
    setStatusFilter('');
    setTierFilter('');
    setAffinityFilter('');
    setRepoFilter('');
    setLabelFilter('');
    setKindFilter('');
    setUpstreamFilter('');
    setRecency(DEFAULT_RECENCY);
    setClosableOnly(false);
  };

  const appliedFilters = useMemo<AppliedFilter[]>(() => {
    const out: AppliedFilter[] = [];
    const add = (label: string, value: string, clear: () => void) => {
      if (value) out.push({ label: `${label}: ${value}`, onClear: clear });
    };
    add('kind', kindFilter, () => {
      setKindFilter('');
    });
    add('status', statusFilter, () => {
      setStatusFilter('');
    });
    add('tier', tierFilter, () => {
      setTierFilter('');
    });
    add('affinity', affinityFilter, () => {
      setAffinityFilter('');
    });
    add('repo', repoFilter, () => {
      setRepoFilter('');
    });
    add('label', labelFilter, () => {
      setLabelFilter('');
    });
    add('upstream', upstreamFilter, () => {
      setUpstreamFilter('');
    });
    if (recency !== DEFAULT_RECENCY) {
      out.push({
        label: `seen: ${RECENCY_LABELS[recency]}`,
        onClear: () => {
          setRecency(DEFAULT_RECENCY);
        },
      });
    }
    if (closableOnly) {
      out.push({
        label: 'closable upstream',
        onClear: () => {
          setClosableOnly(false);
        },
      });
    }
    return out;
  }, [kindFilter, statusFilter, tierFilter, affinityFilter, repoFilter, labelFilter, upstreamFilter, recency, closableOnly]);

  const sorting: SortingState = useMemo(() => {
    const id = SORT_COLUMNS[sortKey];
    return id === null ? [] : [{ id, desc: sortDir === 'desc' }];
  }, [sortKey, sortDir]);

  const onSortingChange = (updater: Updater<SortingState>) => {
    const next = typeof updater === 'function' ? updater(sorting) : updater;
    const first = next[0];
    if (!first) return;
    setSortKey(parseSortKey(first.id));
    setSortDir(first.desc ? 'desc' : 'asc');
  };

  const table = useDataTable({
    columns,
    data: visibleIssues,
    getRowId: (issue) => issue.key,
    manualSorting: true,
    enableSortingRemoval: false,
    state: { sorting },
    onSortingChange,
  });

  const seenWindow = recency === 'all' ? null : RECENCY_LABELS[recency];

  return (
    <>
      <PageHeader
        eyebrow="Queue"
        title="Issues"
        description="Everything the loop can pick up, ranked by tier then priority. Approve a scope to move an issue into execution."
      />

      <Facets rows={facetRows} />
      <Applied
        shown={visibleIssues.length}
        total={facets.data?.total ?? visibleIssues.length}
        noun="issues"
        filters={appliedFilters}
        onClearAll={clearFilters}
      />
      <Toolbar>
        <ToolbarSearch
          value={labelFilter}
          onChange={setLabelFilter}
          placeholder="Filter by label…"
          aria-label="Filter by label"
        />
        {isAdmin && (
          <ToolbarActions>
            <BulkRerank />
            <Button render={<Link to="/jira/new" />}>+ ADOPT JIRA</Button>
            <Button variant="filled" render={<Link to="/scenarios/new" />}>
              + NEW SCENARIO
            </Button>
          </ToolbarActions>
        )}
      </Toolbar>

      <QueryState query={issues} noun="ISSUES">
        <DataTable
          table={table}
          renderSubRow={(row) => <IssueDetailView issueKey={row.original.key} />}
          empty={
            <Empty
              title="NO ISSUES"
              description={
                seenWindow === null
                  ? 'No issues match the current filters.'
                  : `No issues match the current filters within the ${seenWindow} activity window.`
              }
            />
          }
          footer={
            <>
              {visibleIssues.length} issue{visibleIssues.length === 1 ? '' : 's'} · sorted by{' '}
              {sortKey} {sortDir === 'asc' ? '▲' : '▼'}
              {seenWindow !== null && <> · upstream activity in the {seenWindow}</>}
            </>
          }
        />
      </QueryState>
    </>
  );
}

/// A filter whose vocabulary is too long for a row of buttons (the watched repos).
type RerankFilter = components['schemas']['RerankFilter'];

type RerankChoice = { label: string; filter: RerankFilter };

const RERANK_CHOICES: RerankChoice[] = [
  { label: 'All new issues', filter: { scope: 'all' } },
  { label: 'Unranked only', filter: { scope: 'unranked' } },
  ...TIER_OPTIONS.map((tier): RerankChoice => ({ label: `Tier ${tier}`, filter: { scope: 'tier', tier } })),
];

/// Admin-only bulk force re-rank: pick a scope (all / by tier / unranked), confirm, and the
/// controller clears the matching rank caches. Ranking happens on upcoming reconcile sweeps, not
/// inline — the ack only says how many issues are now due.
function BulkRerank() {
  const mutation = $api.useMutation('post', '/api/issues/rerank');
  const queryClient = useQueryClient();
  const [pending, setPending] = useState<RerankChoice | null>(null);
  const [result, setResult] = useState<string | null>(null);

  const handleConfirm = () => {
    if (!pending) return;
    mutation.mutate(
      { body: pending.filter },
      {
        onSuccess: (ack) => {
          setPending(null);
          setResult(`${ack.affected} issue(s) will re-rank on upcoming sweeps`);
          void queryClient.invalidateQueries({ queryKey: ['get', '/api/issues'] });
        },
      },
    );
  };

  return (
    <>
      {result !== null && (
        <Mono size="label" tone="ink-3" className="flex items-center border-r border-rule px-2.5">
          {result}
        </Mono>
      )}
      <select
        aria-label="Bulk force re-rank"
        value=""
        onChange={(event) => {
          const choice = RERANK_CHOICES.find((c) => c.label === event.target.value);
          if (!choice) return;
          setResult(null);
          setPending(choice);
        }}
        className="cursor-pointer border-r border-rule bg-transparent px-2.5 font-mono text-data text-ink-2 hover:bg-hi hover:text-ink"
      >
        <option value="">RE-RANK…</option>
        {RERANK_CHOICES.map((choice) => (
          <option key={choice.label} value={choice.label}>
            {choice.label}
          </option>
        ))}
      </select>
      <AlertDialog.Root
        open={pending !== null}
        onOpenChange={(open) => {
          if (!open) setPending(null);
        }}
      >
        <AlertDialog.Portal>
          <AlertDialog.Backdrop className={DIALOG_BACKDROP} />
          <AlertDialog.Popup className={ALERT_POPUP}>
            <AlertDialog.Title className={DIALOG_TITLE}>
              Re-rank {pending?.label.toLowerCase() ?? ''}?
            </AlertDialog.Title>
            <AlertDialog.Description className="m-0 px-4 py-3.5 text-ink-2">
              Clears the cached ranking verdicts for every <strong className="text-ink">new</strong>{' '}
              issue in scope. The ranker re-runs per issue on upcoming reconcile sweeps, not
              immediately; standing tiers keep gating until fresh verdicts land.
            </AlertDialog.Description>
            <div className="flex justify-end border-t border-rule">
              <Button
                onClick={() => {
                  setPending(null);
                }}
              >
                CANCEL
              </Button>
              <Button variant="filled" onClick={handleConfirm} disabled={mutation.isPending}>
                RE-RANK
              </Button>
            </div>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog.Root>
    </>
  );
}

function DetailBlock({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <Mono size="micro" tone="ink-3" uppercase className="tracking-section">
        {label}
      </Mono>
      <div className="mt-1 text-ink-2">{children}</div>
    </div>
  );
}

function IssueDetailView({ issueKey }: { issueKey: string }) {
  const detail = $api.useQuery('get', '/api/issues/{key}', {
    params: { path: { key: issueKey } },
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
      <div className="px-4.5 py-3.5 text-ink-2">Error loading issue details: {formatError(detail.error)}</div>
    );
  }

  const { data } = detail;

  const rationale = rankingRationale(data.issue.kind, data.events);
  const labels = data.issue.labels;
  const parkedReason = data.issue.parked_reason?.text ?? null;
  const parkedBy = data.issue.parked_by;

  return (
    <div className="grid gap-3.5 px-4.5 py-3.5">
      {rationale && <DetailBlock label="Ranking rationale">{rationale}</DetailBlock>}
      {labels.length > 0 && (
        <DetailBlock label="Labels">
          <span className="flex flex-wrap gap-1">
            {labels.map((label) => (
              <Mono key={label} size="micro" className="border border-rule-hard px-1 py-px">
                {label}
              </Mono>
            ))}
          </span>
        </DetailBlock>
      )}
      {data.issue.evidence_url && (
        <DetailBlock label="Evidence">
          <a
            href={data.issue.evidence_url}
            target="_blank"
            rel="noopener noreferrer"
            className="font-mono text-data underline-offset-2 hover:underline"
          >
            {data.issue.evidence_url}
          </a>
        </DetailBlock>
      )}
      {parkedReason !== null && (
        <DetailBlock label="Parked reason">
          {parkedReason}
          {parkedBy && <Mono tone="ink-3"> (by {parkedBy})</Mono>}
          <span className="ml-2 inline-flex">
            <StaleBadge staleClosable={data.issue.stale_closable} parkedReason={data.issue.parked_reason} />
          </span>
        </DetailBlock>
      )}
      {data.scopes.length > 0 && (
        <DetailBlock label="Provenance">
          <ul className="m-0 grid list-none gap-1 p-0">
            {data.scopes.map((scope) => (
              <li key={scope.scope.id}>
                <Mono uppercase size="micro" tone="ink-3">
                  scope
                </Mono>{' '}
                <Mono>{scope.scope.id}</Mono>
                {scope.scope.approval_pr && (
                  <>
                    {' '}
                    <a
                      href={scope.scope.approval_pr}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="font-mono text-data text-blue underline-offset-2 hover:underline"
                    >
                      approval PR
                    </a>
                  </>
                )}
                {scope.runs.length > 0 && (
                  <ul className="m-0 mt-1 grid list-none gap-1 p-0 pl-4">
                    {scope.runs.map((runDetail) => (
                      <li key={runDetail.run.run_id} className="flex flex-wrap items-center gap-2">
                        <Identifier variant="inline" to={`/runs/${runDetail.run.run_id}`}>
                          {runDetail.run.run_id}
                        </Identifier>
                        {runDetail.run.best_score !== null && runDetail.run.best_score !== undefined && (
                          <Mono>score {runDetail.run.best_score.toFixed(1)}</Mono>
                        )}
                        {runDetail.run.cost_usd !== null && runDetail.run.cost_usd !== undefined && (
                          <Mono>${runDetail.run.cost_usd.toFixed(2)}</Mono>
                        )}
                      </li>
                    ))}
                  </ul>
                )}
              </li>
            ))}
          </ul>
        </DetailBlock>
      )}
    </div>
  );
}
