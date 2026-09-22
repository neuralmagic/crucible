import { useEffect, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { $api, fetchScopeTranscript } from '../api/client';
import { useLiveEvents } from '../api/useLiveEvents';
import { formatError } from '../api/errors';
import { TurnLive } from '../live/TurnLive';
import type { components } from '../api/schema';
import { foldSessionText, type FeedRow } from '../live/feed';
import { SessionFeed } from '../live/SessionFeed';
import {
  Breadcrumb,
  Button,
  cn,
  createDataColumnHelper,
  DataTable,
  DetailHeader,
  Empty,
  Identifier,
  LoadingBlock,
  Mono,
  Section,
  SectionBody,
  SectionHeader,
  Spec,
  Spinner,
  Status,
  statusTone,
  useDataTable,
} from '../ui';
import type { BreadcrumbItem, SpecItem, StatusTone } from '../ui';
import { outcomeSummary, roundOutcomeTone, type RoundRecord } from './approvalEvidence';
import { FormActions, FormError, Note } from './formControls';
import { issueStatusColor, type IssueStatus } from './issueStatus';
import {
  firstFailedStage,
  formatUsd,
  hasBreakdown,
  reportHeadline,
  stageTone,
  type ScopeReport,
  type ScopeStage,
} from './scopeReport';

type IssueDetail = components['schemas']['IssueDetail'];
type EventDto = components['schemas']['EventDto'];

type Phase = 'queued' | 'running' | 'success' | 'failure';

const PHASE_LABEL: Record<Phase, string> = {
  queued: 'queued',
  running: 'running',
  success: 'completed',
  failure: 'failed',
};

const PHASE_TONE: Record<Phase, StatusTone> = {
  queued: 'grey',
  running: 'blue',
  success: 'green',
  failure: 'red',
};

function findScopeNowEvent(events: EventDto[]): EventDto | null {
  for (let i = events.length - 1; i >= 0; i--) {
    const event = events[i];
    const reason = event.reason?.text;
    if (reason?.includes('ScopeNow') || reason?.includes('scope_now')) {
      return event;
    }
  }
  return null;
}

const SCOPED_OK: readonly IssueStatus[] = ['scoped', 'awaiting-approval'];

function derivePhase(data: IssueDetail): Phase {
  const status = data.issue.status;

  if (SCOPED_OK.some((s) => s === status)) return 'success';
  if (status === 'parked') return 'failure';
  if (status === 'running') return 'running';
  return 'queued';
}

function latestApprovalPr(data: IssueDetail): string | null {
  for (let i = data.scopes.length - 1; i >= 0; i--) {
    const pr = data.scopes[i].scope.approval_pr;
    if (pr) return pr;
  }
  return null;
}

/// The newest scope recorded for the issue: the one an approval would act on.
function latestScope(data: IssueDetail): IssueDetail['scopes'][number]['scope'] | null {
  const last = data.scopes[data.scopes.length - 1];
  return last ? last.scope : null;
}

/// What approving this pack accepts. The controller renders the lines, so this page, the MCP
/// renderer and the approval PR never drift apart.
function ExposureBlock({ scope }: { scope: IssueDetail['scopes'][number]['scope'] }) {
  const lines = scope.exposure.lines;
  if (lines.length === 0) return null;
  const bound = scope.exposure.approved_digest ?? null;
  const stored = scope.exposure.digest ?? null;
  return (
    <div className="grid gap-1">
      <span className="font-mono text-micro tracking-label text-ink-3 uppercase">
        Exposure {stored ?? '(none stored)'}
      </span>
      <pre className="m-0 max-w-[80ch] overflow-auto border border-rule bg-paper px-2 py-1.5 font-mono text-data whitespace-pre-wrap text-ink-2">
        {lines.join('\n')}
      </pre>
      {bound !== null && (
        <span className={cn('font-mono text-micro', bound === stored ? 'text-ink-3' : 'text-amber')}>
          {bound === stored
            ? `approval bound to exposure ${bound}`
            : `approval bound to exposure ${bound}, which is no longer what is stored`}
        </span>
      )}
    </div>
  );
}

function formatElapsed(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return m > 0 ? `${m}m ${s}s` : `${s}s`;
}

const stageHelper = createDataColumnHelper<ScopeStage>();

const stageColumns = stageHelper.columns([
  stageHelper.display({
    id: 'result',
    header: 'Result',
    meta: { shrink: true },
    cell: ({ row }) => (
      <Status
        status={row.original.passed ? 'pass' : 'fail'}
        tone={statusTone(stageTone(row.original))}
      />
    ),
  }),
  stageHelper.accessor('name', {
    header: 'Stage',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data font-semibold text-ink' },
  }),
  stageHelper.accessor('detail', {
    header: 'Detail',
    enableSorting: false,
    meta: { wrap: true, className: 'text-ink-2' },
  }),
]);

function StagesTable({ stages }: { stages: ScopeStage[] }) {
  const table = useDataTable({
    columns: stageColumns,
    data: stages,
    getRowId: (stage) => stage.name,
  });
  return <DataTable table={table} />;
}

const roundHelper = createDataColumnHelper<RoundRecord>();

const roundColumns = roundHelper.columns([
  roundHelper.accessor('round', {
    header: 'Round',
    enableSorting: false,
    meta: { shrink: true, align: 'end', className: 'font-mono text-data font-semibold text-ink' },
  }),
  roundHelper.accessor('kind', {
    header: 'Kind',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => (
      <Mono size="label" uppercase>
        {getValue()}
      </Mono>
    ),
  }),
  roundHelper.display({
    id: 'outcome',
    header: 'Outcome',
    meta: { shrink: true },
    cell: ({ row }) => {
      const { label, tone } = roundOutcomeTone(row.original.outcome);
      return <Status status={label} tone={statusTone(tone)} />;
    },
  }),
  roundHelper.display({
    id: 'summary',
    header: 'Summary',
    cell: ({ row }) => <span className="text-ink-2">{outcomeSummary(row.original.outcome)}</span>,
    meta: { wrap: true },
  }),
  roundHelper.accessor('cost', {
    header: 'Cost',
    enableSorting: false,
    meta: { shrink: true, align: 'end' },
    cell: ({ getValue }) => <Mono>{formatUsd(getValue()) ?? '—'}</Mono>,
  }),
]);

function RoundsTable({ rounds }: { rounds: RoundRecord[] }) {
  const table = useDataTable({
    columns: roundColumns,
    data: rounds,
    getRowId: (round) => String(round.round),
  });
  return <DataTable table={table} />;
}

function ScopeReportView({ report }: { report: ScopeReport }) {
  const totalCost = formatUsd(report.cost);
  return (
    <Section>
      <SectionHeader title="Scope report" note={reportHeadline(report)} />
      {report.stages.length > 0 && <StagesTable stages={report.stages} />}
      {report.rounds.length > 0 && <RoundsTable rounds={report.rounds} />}
      <SectionBody className="flex flex-wrap items-baseline gap-3.5">
        {totalCost !== null && <Mono>total turn cost {totalCost}</Mono>}
        <Mono tone="ink-3">{new Date(report.created_at).toLocaleString()}</Mono>
        {report.pod_name && (
          <Mono tone="ink-3">
            work pod <Mono className="select-all text-ink-2">{report.pod_name}</Mono>
          </Mono>
        )}
      </SectionBody>
    </Section>
  );
}

/// Row cap for the replayed transcript — matches the spirit of the live feed's buffer cap: enough
/// to read the whole story of a multi-round scope, bounded so a pathological transcript can't hang
/// the page.
const TRANSCRIPT_ROW_CAP = 5000;

type TranscriptState =
  | { phase: 'loading' }
  | { phase: 'absent' }
  | { phase: 'error'; message: string }
  | { phase: 'ready'; rows: FeedRow[] };

/// The post-facto transcript of the scope turn's agent session: fetched once from the controller
/// (which persisted it when the turn ended) and replayed through the same session renderer the
/// live run pane uses. Pre-transcript scopes have nothing stored — say so instead of erroring.
function TranscriptSection({ issueKey }: { issueKey: string }) {
  const [state, setState] = useState<TranscriptState>({ phase: 'loading' });

  useEffect(() => {
    let cancelled = false;
    setState({ phase: 'loading' });
    fetchScopeTranscript(issueKey)
      .then((text) => {
        if (cancelled) return;
        if (text === null) {
          setState({ phase: 'absent' });
        } else {
          setState({ phase: 'ready', rows: foldSessionText(text, TRANSCRIPT_ROW_CAP) });
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setState({ phase: 'error', message: err instanceof Error ? err.message : String(err) });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [issueKey]);

  return (
    <Section>
      <SectionHeader title="Transcript" />
      {state.phase === 'ready' ? (
        <SessionFeed rows={state.rows} autoFollow={false} emptyText="The transcript is empty." />
      ) : (
        <SectionBody>
          {state.phase === 'loading' && <Spinner label="LOADING TRANSCRIPT" />}
          {state.phase === 'absent' && (
            <Note>
              No transcript was recorded for this scope attempt (scoped before transcript capture,
              or the turn never streamed).
            </Note>
          )}
          {state.phase === 'error' && <Note>Failed to load the transcript: {state.message}</Note>}
        </SectionBody>
      )}
    </Section>
  );
}

const eventHelper = createDataColumnHelper<EventDto>();

const eventColumns = eventHelper.columns([
  eventHelper.display({
    id: 'transition',
    header: 'Transition',
    meta: { shrink: true },
    cell: ({ row }) => (
      <span className="flex items-center gap-2">
        <Status status={row.original.from} tone={statusTone(issueStatusColor(row.original.from))} />
        <Mono tone="ink-3">→</Mono>
        <Status status={row.original.to} tone={statusTone(issueStatusColor(row.original.to))} />
      </span>
    ),
  }),
  eventHelper.accessor('actor', {
    header: 'Actor',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => <Mono>{getValue() || '—'}</Mono>,
  }),
  eventHelper.display({
    id: 'reason',
    header: 'Reason',
    meta: { wrap: true, className: 'text-ink-2' },
    cell: ({ row }) => row.original.reason?.text ?? '—',
  }),
  eventHelper.accessor('ts', {
    header: 'When',
    enableSorting: false,
    meta: { shrink: true, align: 'end' },
    cell: ({ getValue }) => <Mono tone="ink-3">{new Date(getValue()).toLocaleString()}</Mono>,
  }),
]);

function EventsTable({ events }: { events: EventDto[] }) {
  const table = useDataTable({
    columns: eventColumns,
    data: events,
    getRowId: (event, index) => `${event.ts}-${index}`,
  });
  return <DataTable table={table} />;
}

export function ScopeProgressPage() {
  useLiveEvents();

  const { key: rawKey } = useParams<{ key: string }>();
  const issueKey = rawKey ? decodeURIComponent(rawKey) : '';

  const detail = $api.useQuery('get', '/api/issues/{key}', {
    params: { path: { key: issueKey } },
    refetchInterval: 5000,
  });
  const scopeReport = $api.useQuery('get', '/api/issues/{key}/scope-report', {
    params: { path: { key: issueKey } },
    refetchInterval: 5000,
    retry: false,
  });
  const whoami = $api.useQuery('get', '/api/whoami');
  // While a scope turn is in flight, its pod name lives on the running work-pod ledger row — the
  // handle the live pane streams by. Polled so the pane appears the moment dispatch creates the row.
  const runningTurns = $api.useQuery('get', '/api/turns', {
    params: { query: { kind: 'scope', state: 'running' } },
    refetchInterval: 5000,
  });
  const retryMutation = $api.useMutation('post', '/api/issues/{key}/scope');

  const [startTime] = useState(() => Date.now());
  const [elapsed, setElapsed] = useState(0);
  const [retryError, setRetryError] = useState<string | null>(null);
  const [retriedAt, setRetriedAt] = useState<number | null>(null);

  useEffect(() => {
    const timer = setInterval(() => {
      setElapsed(Math.floor((Date.now() - startTime) / 1000));
    }, 1000);
    return () => clearInterval(timer);
  }, [startTime]);

  if (!issueKey) {
    return <Empty title="NO ISSUE KEY" description="This route needs an issue key." />;
  }

  if (detail.isError) {
    return <Empty title="PROGRESS UNAVAILABLE" description={formatError(detail.error)} />;
  }

  if (detail.isPending) {
    return <LoadingBlock label="LOADING SCOPE PROGRESS" />;
  }

  const { data } = detail;
  // A submitted retry re-queues the issue via the override sink; keep showing the queued state
  // until the poll observes the transition, so the button press has visible effect immediately.
  // A park that lands AFTER the retry is a fresh failure and wins again.
  const rawPhase = derivePhase(data);
  const latestParkTs = data.events
    .filter((e) => e.to === 'parked')
    .reduce((max, e) => Math.max(max, new Date(e.ts).getTime()), 0);
  const phase =
    rawPhase === 'failure' && retriedAt !== null && latestParkTs < retriedAt ? 'queued' : rawPhase;
  const approvalPr = latestApprovalPr(data);
  const scope = latestScope(data);
  // 404 = no structured report recorded (dispatch failure, timeout, or pre-upgrade row).
  const report = scopeReport.data && hasBreakdown(scopeReport.data) ? scopeReport.data : null;
  const isAdmin = whoami.data?.role === 'admin';

  const handleRetry = async () => {
    setRetryError(null);
    const failed = scopeReport.data ? firstFailedStage(scopeReport.data.stages) : null;
    const why = failed ? `${failed.name}: ${failed.detail}` : (data.issue.parked_reason?.text ?? 'unknown failure');
    try {
      await retryMutation.mutateAsync({
        params: { path: { key: issueKey } },
        body: { justification: `Retry after scope failure — ${why}`.slice(0, 500) },
      });
      setRetriedAt(Date.now());
    } catch (err: unknown) {
      setRetryError(formatError(err));
    }
  };

  const scopeNowEvent = findScopeNowEvent(data.events);
  const recentEvents = data.events.slice(-10).reverse();
  // The live pane needs a pod: the newest running scope turn for this issue. No row (yet) — e.g.
  // dispatch hasn't created the pod, or the stream 404s — degrades to the plain spinner above.
  const liveTurn =
    phase === 'running' ? (runningTurns.data ?? []).find((t) => t.issue_key === issueKey) ?? null : null;

  const crumbs: BreadcrumbItem[] = [
    { label: 'Issues', to: '/issues' },
    { label: issueKey, to: `/issues/${encodeURIComponent(issueKey)}` },
    { label: 'Scope progress' },
  ];

  const specs: SpecItem[] = [
    { label: 'Phase', value: PHASE_LABEL[phase], tone: phase === 'failure' ? 'bad' : phase === 'success' ? 'win' : 'default' },
    { label: 'Elapsed', value: formatElapsed(elapsed) },
  ];

  return (
    <>
      <Breadcrumb items={crumbs} />
      <DetailHeader
        title={issueKey}
        badge={
          <Status
            status={PHASE_LABEL[phase]}
            tone={PHASE_TONE[phase]}
            pulse={phase === 'queued' || phase === 'running'}
          />
        }
        description={data.issue.title || issueKey}
        meta={
          <>
            <Identifier variant="inline" to={`/issues/${encodeURIComponent(issueKey)}`}>
              {issueKey}
            </Identifier>
            <Status
              status={data.issue.status}
              tone={statusTone(issueStatusColor(data.issue.status))}
            />
          </>
        }
        aside={<Spec items={specs} />}
      />

      {liveTurn && (
        <Section>
          <SectionHeader title="Live session" note={liveTurn.pod_name} />
          <SectionBody>
            <TurnLive podName={liveTurn.pod_name} createdAt={liveTurn.created_at} />
          </SectionBody>
        </Section>
      )}

      {(phase === 'queued' || phase === 'running') && !liveTurn && (
        <Section>
          <SectionHeader title="Progress" />
          <SectionBody>
            <Spinner
              label={phase === 'queued' ? 'SCOPE OVERRIDE QUEUED' : 'SCOPE TURN RUNNING'}
            />
          </SectionBody>
        </Section>
      )}

      {phase === 'success' && (
        <Section>
          <SectionHeader title="Outcome" note="scope completed" />
          <SectionBody className="grid gap-3.5">
            {scope !== null && <ExposureBlock scope={scope} />}
            {approvalPr ? (
              <span className="flex flex-wrap items-center gap-3.5 font-mono text-data">
                <a
                  href={approvalPr}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="text-blue underline-offset-2 hover:underline"
                >
                  View approval PR
                </a>
                <Link to="/approvals" className="text-ink-2 underline-offset-2 hover:underline">
                  View on Approvals
                </Link>
              </span>
            ) : (
              <p className="m-0 text-ink-2">
                Approval PR will appear on the <Link to="/approvals" className="underline-offset-2 hover:underline">Approvals</Link>{' '}
                page once generated.
              </p>
            )}
          </SectionBody>
        </Section>
      )}

      {phase === 'failure' && (
        <Section>
          <SectionHeader title="Outcome" note="scope failed" />
          <SectionBody className="grid gap-3.5">
            {!report && data.issue.parked_reason && (
              <p className="m-0 max-w-[80ch] text-ink-2">{data.issue.parked_reason.text}</p>
            )}
            <div>
              <Button
                variant="filled"
                onClick={() => void handleRetry()}
                disabled={!isAdmin || retryMutation.isPending}
              >
                {retryMutation.isPending ? 'RETRYING…' : 'RETRY SCOPE'}
              </Button>
            </div>
            <p className="m-0 max-w-[80ch] text-data-lg text-ink-3">
              {isAdmin
                ? 'Retrying unparks the issue and re-runs the scope turn, bypassing all budget caps (a ScopeNow override, booked to your login).'
                : 'Admin access required to retry a scope turn.'}
            </p>
            {retryError !== null && <FormError>{retryError}</FormError>}
          </SectionBody>
        </Section>
      )}

      {report && <ScopeReportView report={report} />}

      {(phase === 'success' || phase === 'failure') && <TranscriptSection issueKey={issueKey} />}

      {scopeNowEvent && (
        <Section>
          <SectionHeader title="Trigger" />
          <SectionBody className="flex flex-wrap items-baseline gap-2.5">
            {scopeNowEvent.actor && <Mono weight="semibold">{scopeNowEvent.actor}</Mono>}
            {scopeNowEvent.reason && <span className="text-ink-2">{scopeNowEvent.reason.text}</span>}
            <Mono tone="ink-3">{new Date(scopeNowEvent.ts).toLocaleString()}</Mono>
          </SectionBody>
        </Section>
      )}

      {recentEvents.length > 0 && (
        <Section>
          <SectionHeader title="Recent events" note={`last ${recentEvents.length}`} />
          <EventsTable events={recentEvents} />
        </Section>
      )}

      <FormActions>
        <Button render={<Link to={`/issues/${encodeURIComponent(issueKey)}`} />}>
          BACK TO ISSUE
        </Button>
      </FormActions>
    </>
  );
}
