import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useLocation, useNavigate, useParams } from 'react-router-dom';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import {
  $api,
  fetchArtifact,
  fetchArtifactOrNull,
  fetchFlowEnriched,
  fetchRunFile,
  fetchRunFileBlob,
} from '../api/client';
import { flowEnrichedMessage, validTraceId } from './flowEnriched';
import { MarkdownView } from './MarkdownView';
import { flowThemeStyle, injectFlowTheme } from './flowTheme';
import { AgentProviderTag } from './ProviderIcon';
import { useLiveEvents } from '../api/useLiveEvents';
import { formatError } from '../api/errors';
import { LiveSession } from '../live/LiveSession';
import { useAppTheme } from '../appTheme';
import {
  Breadcrumb,
  Button,
  cn,
  createDataColumnHelper,
  DataTable,
  Delta,
  DetailHeader,
  Empty,
  Identifier,
  LoadingBlock,
  Mono,
  PrLink,
  Score,
  Section,
  SectionBody,
  SectionHeader,
  Spec,
  Spinner,
  Split,
  SplitHandle,
  SplitPane,
  Status,
  statusTone,
  useDataTable,
} from '../ui';
import { CodeSurface } from '../editor/CodeSurface';
import type { SpecItem, StatusTone } from '../ui';
import {
  decisionTone,
  formatBytes,
  formatCost,
  formatScore,
  isBaseline,
  parseUnifiedDiff,
  runCreatedFromId,
  scoresImproveDownward,
  type Candidate,
  type DecisionTone,
  type DiffFile,
  transportLossLabel,
} from './runReport';
import { runStatusColor } from './runStatus';
import { buildCostBreakdown, type TaskCostRow } from './costBreakdown';
import { type RunGraph } from './RunTaskGraph';
import { latestResults, toneOf } from './taskGraph';
import { RunTaskGrid } from './RunTaskGrid';
import { absoluteTime, relativeTime } from './journeyView';
import { detailPath } from './launchView';
import {
  downloadName,
  imageMimeType,
  parseRunFilePath,
  prettyJson,
  producerOf,
  runFileApiPath,
  runFilePath,
  runFileUrl,
  selectRunFile,
  shouldAnchorFiles,
  textKind,
  type RunFileRoute,
  type TextKind,
} from './runFileView';

const DECISION_TONE: Record<DecisionTone, StatusTone> = {
  keep: 'green',
  discard: 'red',
  baseline: 'grey',
  neutral: 'blue',
};

/// A personal target's name is an opaque secret id, so it is shown as what it is rather than as a
/// cluster name a reader could go look up.
function clusterLabel(cluster: string): string {
  return cluster.startsWith('personal:') ? 'a personal cluster' : cluster;
}

export function RunDetailPage() {
  useLiveEvents();

  const { runId: rawRunId, key: rawLaunchKey } = useParams<{ runId: string; key: string }>();
  const runId = rawRunId ?? '';
  const launchKey = rawLaunchKey ?? null;
  const { pathname } = useLocation();
  const deepLinkedFile = parseRunFilePath(pathname)?.fileKey ?? null;
  const queryClient = useQueryClient();
  // A playbook run has no scoring loop: no flow report, no iterations, no baseline. The whole
  // score surface is off for it rather than rendered empty.
  const isPlaybook = runId.startsWith('playbook_');

  const runDetail = $api.useQuery('get', '/api/runs/{run_id}', {
    params: { path: { run_id: runId } },
  });
  const iterations = $api.useQuery(
    'get',
    '/api/runs/{run_id}/iterations',
    { params: { path: { run_id: runId } } },
    { enabled: !isPlaybook },
  );
  // Same key FlowSection uses, so this costs no extra fetch. The score curve and iterations
  // table only render when the run has no flow report — the report subsumes both (all that's
  // lost is inline diff hunks, still served at artifacts/diffs/iter-N.patch).
  const isLiveRun = runDetail.data?.run.status === 'running';
  const flowProbe = useFlowArtifact(runId, runDetail.data !== undefined && !isLiveRun && !isPlaybook);
  const flowAbsent = isLiveRun || flowProbe.isError || flowProbe.data === null;

  // When the live stream ends because the run finished or is no longer running, refetch the run's
  // queries so the freshly-ingested record replaces the live pane (mirrors the old RunsPage behavior).
  const handleLiveEnded = useCallback(
    (reason: string) => {
      if (reason !== 'bridge-closed' && reason !== 'run-not-running') return;
      void queryClient.invalidateQueries({
        predicate: (query) => {
          const key = query.queryKey;
          return (
            Array.isArray(key) &&
            key[0] === 'get' &&
            typeof key[1] === 'string' &&
            (key[1] === '/api/runs' ||
              key[1] === '/api/runs/{run_id}' ||
              key[1] === '/api/runs/{run_id}/iterations')
          );
        },
      });
    },
    [queryClient],
  );

  if (!runId) {
    return <Empty title="NO RUN ID" />;
  }

  if (runDetail.isError) {
    return <Empty title="RUN UNAVAILABLE" description={formatError(runDetail.error)} />;
  }

  if (runDetail.isPending) {
    return <LoadingBlock label="LOADING RUN" />;
  }

  const { run } = runDetail.data;
  const candidates = iterations.data ?? [];
  const prUrls = uniquePrUrls(candidates);
  const created = runCreatedFromId(runId);
  const baseline = baselineScore(candidates);
  const lowerIsBetter = improvesDownward(candidates, run.best_score);
  const kept = candidates.filter((c) => !isBaseline(c) && decisionTone(c.decision) === 'keep').length;
  const measured = candidates.filter((c) => !isBaseline(c)).length;

  const vsBaseline = percentChange(run.best_score, baseline);
  const improved = vsBaseline === null ? null : lowerIsBetter ? vsBaseline < 0 : vsBaseline > 0;

  const specs: SpecItem[] = isPlaybook
    ? [{ label: 'Cost', value: formatCost(run.cost_usd) }]
    : [
        {
          label: 'Best score',
          value: formatScore(run.best_score),
          tone: improved === null ? 'default' : improved ? 'win' : 'bad',
        },
        {
          label: 'vs baseline',
          value:
            vsBaseline === null ? '—' : `${vsBaseline > 0 ? '+' : ''}${vsBaseline.toFixed(1)}%`,
          tone: improved === null ? 'default' : improved ? 'win' : 'bad',
        },
        { label: 'Cost', value: formatCost(run.cost_usd) },
        { label: 'Kept', value: String(kept), note: `/ ${measured}` },
      ];

  return (
    <>
      <Breadcrumb items={[{ label: 'Runs', to: '/runs' }, { label: runId }]} />

      <DetailHeader
        title={runId}
        badge={
          <span className="flex flex-wrap items-center gap-2">
            <Status status={run.status} tone={statusTone(runStatusColor(run.status))} />
            {transportLossLabel(run.transport_losses) && (
              <Status status={transportLossLabel(run.transport_losses) ?? ''} tone="red" pulse={false} />
            )}
          </span>
        }
        meta={
          <>
            {run.issue_key && (
              <Identifier variant="inline" to={detailPath(run.issue_key)}>
                {run.issue_key}
              </Identifier>
            )}
            {run.repo && <span>{run.repo}</span>}
            {run.dispatch === 'local' && <span>ran locally</span>}
            {run.dispatch !== 'local' && run.cluster !== 'hub' && (
              <span title={run.namespace ?? undefined}>on {clusterLabel(run.cluster)}</span>
            )}
            {run.agent_provider && (
              <span>
                via <AgentProviderTag provider={run.agent_provider} model={run.agent_model} />
              </span>
            )}
            {created !== null && (
              <span title={absoluteTime(created) ?? undefined}>started {relativeTime(created)}</span>
            )}
          </>
        }
        aside={<Spec items={specs} />}
      />

      {prUrls.length > 0 && (
        <Section>
          <SectionHeader title="Pull requests" note={`${prUrls.length} opened`} />
          <SectionBody className="flex flex-wrap gap-3.5">
            {prUrls.map((url) => (
              <PrLink key={url} url={url} />
            ))}
          </SectionBody>
        </Section>
      )}

      {isLiveRun && (
        <Section>
          <SectionHeader title="Live session" />
          <SectionBody>
            <LiveSession runId={runId} onEnded={handleLiveEnded} />
          </SectionBody>
        </Section>
      )}

      <RunTaskGrid runId={runId} />

      {!isPlaybook && !isLiveRun && <FlowSection runId={runId} />}

      {!isPlaybook && flowAbsent && (
        <ScoreSection
          candidates={candidates}
          pending={iterations.isPending}
          lowerIsBetter={lowerIsBetter}
        />
      )}

      {!isPlaybook && flowAbsent && (
        <Section>
          <SectionHeader title="Iterations" note={`${measured} measured, ${kept} kept`} />
          <IterationsTable
            runId={runId}
            candidates={candidates}
            pending={iterations.isPending}
            baseline={baseline}
            best={run.best_score ?? null}
            lowerIsBetter={lowerIsBetter}
          />
        </Section>
      )}

      <FilesPanel runId={runId} launchKey={launchKey} fileKey={deepLinkedFile} />

      <div className="grid grid-cols-2 max-wide:grid-cols-1">
        <CostSection runId={runId} runCost={run.cost_usd} />
        <ArtifactsPanel runId={runId} />
      </div>
    </>
  );
}

function uniquePrUrls(candidates: Candidate[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const c of candidates) {
    if (c.pr_url && !seen.has(c.pr_url)) {
      seen.add(c.pr_url);
      out.push(c.pr_url);
    }
  }
  return out;
}

function baselineScore(candidates: Candidate[]): number | null {
  for (const c of candidates) {
    if (isBaseline(c) && c.score !== null && c.score !== undefined) return c.score;
  }
  return null;
}

function percentChange(value: number | null | undefined, base: number | null): number | null {
  if (value === null || value === undefined || base === null || base === 0) return null;
  return ((value - base) / base) * 100;
}

/// Which way the gate's metric points: whichever end of the measured band `best_score` sits at is
/// the improving direction. Defaults to lower-is-better.
function improvesDownward(candidates: Candidate[], best: number | null | undefined): boolean {
  const scores: number[] = [];
  for (const c of candidates) {
    if (c.score !== null && c.score !== undefined) scores.push(c.score);
  }
  return scoresImproveDownward(scores, best);
}

// --- flow report -------------------------------------------------------------

// No retries: each request shells `crucible fetch` server-side, and for runs published before
// the flow report existed the answer never changes.
function useFlowArtifact(runId: string, enabled: boolean) {
  return useQuery({
    queryKey: ['artifact', runId, 'flow.html'],
    queryFn: () => fetchArtifactOrNull(runId, 'flow.html'),
    staleTime: Infinity,
    retry: false,
    enabled,
  });
}

// The engine publishes a self-contained flow.html (time-proportional swimlane of the run) next to
// summary.json on keep. It renders in a sandboxed iframe: srcDoc + sandbox keep it opaque-origin,
// so its inline scripts run without access to controller cookies or the API. Runs published before
// the flow report existed have no flow.html; that is a normal state, not an error.
function FlowSection({ runId }: { runId: string }) {
  const flow = useFlowArtifact(runId, true);

  // Span enrichment: the trace id is user-supplied (it isn't recorded at ingest yet), and both
  // inputs are immutable, so a fetched render never goes stale.
  const [traceDraft, setTraceDraft] = useState('');
  const [traceId, setTraceId] = useState<string | null>(null);
  const enriched = useQuery({
    queryKey: ['flow-enriched', runId, traceId],
    queryFn: () => fetchFlowEnriched(runId, traceId ?? ''),
    staleTime: Infinity,
    retry: false,
    enabled: traceId !== null,
  });
  const theme = useAppTheme();

  if (flow.isPending) {
    return (
      <Section>
        <SectionHeader title="Flow" />
        <SectionBody>
          <Spinner label="LOADING FLOW" />
        </SectionBody>
      </Section>
    );
  }
  // A missing S3 object surfaces as 502, not 404: the proxy can't tell NoSuchKey from a transport
  // error without parsing `crucible fetch` stderr (artifacts.rs declines to). Old runs are the
  // common case, so any failure degrades to the same quiet message; the controller logs the
  // fetch failure server-side.
  if (flow.isError || flow.data === null) {
    return (
      <Section>
        <SectionHeader title="Flow" />
        <SectionBody>
          <Mono tone="ink-3">No visual report for this run.</Mono>
        </SectionBody>
      </Section>
    );
  }
  const enrichedData = traceId !== null ? enriched.data : undefined;
  const enrichedHtml = enrichedData?.ok ? enrichedData.html : null;
  const enrichedError =
    enrichedData !== undefined && !enrichedData.ok
      ? flowEnrichedMessage(enrichedData.status, enrichedData.error)
      : enriched.isError
        ? 'span fetch failed'
        : null;
  const openHref =
    enrichedHtml !== null && traceId !== null
      ? `/api/runs/${encodeURIComponent(runId)}/flow-enriched?trace_id=${encodeURIComponent(traceId)}`
      : `/api/runs/${encodeURIComponent(runId)}/artifacts/flow.html`;
  const submitTrace = () => {
    const t = traceDraft.trim();
    if (validTraceId(t)) setTraceId(t);
  };
  // Read the SPA's tokens now, after the toggle has repainted <html>.
  const tokens = getComputedStyle(document.documentElement);
  const themedHtml = injectFlowTheme(
    enrichedHtml ?? flow.data,
    flowThemeStyle(theme, (token) => tokens.getPropertyValue(token)),
  );

  return (
    <Section>
      <SectionHeader
        title="Flow"
        actions={
          <a
            className="font-mono text-label font-normal tracking-normal normal-case text-ink-3 hover:text-ink"
            href={openHref}
            target="_blank"
            rel="noopener noreferrer"
          >
            open in new tab
          </a>
        }
      />
      <SectionBody>
        {/* Requires an engine with the flow subcommand and Datadog keys in the controller env. */}
        <div className="mb-2 flex items-center gap-2">
          <input
            className="w-80 border border-rule-hard bg-paper px-2 py-1 font-mono text-data text-ink placeholder:text-ink-3"
            aria-label="Datadog trace id"
            placeholder="Datadog trace id"
            value={traceDraft}
            onChange={(ev) => {
              setTraceDraft(ev.target.value);
            }}
            onKeyDown={(ev) => {
              if (ev.key === 'Enter') submitTrace();
            }}
          />
          <Button variant="filled" disabled={!validTraceId(traceDraft.trim())} onClick={submitTrace}>
            ADD TIMINGS
          </Button>
          {traceId !== null && (
            <Button
              onClick={() => {
                setTraceId(null);
                setTraceDraft('');
              }}
            >
              CLEAR
            </Button>
          )}
          {enrichedError !== null && <Mono tone="red">{enrichedError}</Mono>}
        </div>
        {/* First enrichment can take several seconds: session fetch + a paginated Datadog search. */}
        {enriched.isFetching ? (
          <Spinner label="FETCHING SPANS" />
        ) : (
          <iframe
            key={theme}
            className="h-[70vh] w-full border border-rule bg-surface"
            title="run flow report"
            sandbox="allow-scripts"
            srcDoc={themedHtml}
          />
        )}
      </SectionBody>
    </Section>
  );
}

// --- score chart -------------------------------------------------------------

const CHART_W = 1000;
const CHART_H = 260;
const AXIS_L = 54;
const AXIS_R = 18;
const AXIS_T = 16;
const AXIS_B = 30;
const PLOT_W = CHART_W - AXIS_L - AXIS_R;
const PLOT_H = CHART_H - AXIS_T - AXIS_B;
const TICKS = 5;

interface ChartMark {
  x: number;
  y: number;
  iter: number;
  score: number;
  tone: DecisionTone;
}

interface RunChart {
  marks: ChartMark[];
  grid: { y: number; label: string }[];
  trail: string;
  baseline: { y: number; label: string } | null;
  best: { x: number; y: number; label: string };
  label: string;
}

/// Every measured candidate plotted with a bigger metric value higher, the baseline measurement
/// seeding the band, and the trail walking best-so-far in the gate's improving direction.
function buildRunChart(candidates: Candidate[], lowerIsBetter: boolean): RunChart | null {
  const measured: { iter: number; score: number; tone: DecisionTone }[] = [];
  for (const c of candidates) {
    const score = c.score;
    if (score === null || score === undefined) continue;
    measured.push({
      iter: c.iter ?? 0,
      score,
      tone: isBaseline(c) ? 'baseline' : decisionTone(c.decision),
    });
  }
  if (measured.length === 0) return null;

  const scores = measured.map((m) => m.score);
  const rawLo = Math.min(...scores);
  const rawHi = Math.max(...scores);
  const pad = (rawHi - rawLo) * 0.12 || 1;
  const lo = rawLo - pad;
  const hi = rawHi + pad;
  const span = hi - lo;
  const digits = rawHi - rawLo < 10 ? 1 : 0;

  const yOf = (v: number) => AXIS_T + ((hi - v) / span) * PLOT_H;
  const xOf = (i: number) =>
    measured.length === 1 ? AXIS_L + PLOT_W / 2 : AXIS_L + (i / (measured.length - 1)) * PLOT_W;

  const marks: ChartMark[] = measured.map((m, i) => ({
    x: xOf(i),
    y: yOf(m.score),
    iter: m.iter,
    score: m.score,
    tone: m.tone,
  }));

  let running = measured[0].score;
  const trail = measured
    .map((m, i) => {
      running = lowerIsBetter ? Math.min(running, m.score) : Math.max(running, m.score);
      return `${xOf(i).toFixed(2)},${yOf(running).toFixed(2)}`;
    })
    .join(' ');

  const grid = Array.from({ length: TICKS }, (_, k) => {
    const v = lo + ((hi - lo) * k) / (TICKS - 1);
    return { y: yOf(v), label: v.toFixed(digits) };
  });

  const base = baselineScore(candidates);
  const bestValue = lowerIsBetter ? rawLo : rawHi;

  return {
    marks,
    grid,
    trail,
    baseline: base === null ? null : { y: yOf(base), label: `BASELINE ${base.toFixed(digits)}` },
    best: {
      x: xOf(measured.length - 1),
      y: yOf(bestValue),
      label: `BEST ${bestValue.toFixed(digits)}`,
    },
    label: `Score by iteration, ${measured[0].score.toFixed(digits)} to ${bestValue.toFixed(digits)} over ${measured.length} measurements`,
  };
}

function ScoreSection({
  candidates,
  pending,
  lowerIsBetter,
}: {
  candidates: Candidate[];
  pending: boolean;
  lowerIsBetter: boolean;
}) {
  const chart = pending ? null : buildRunChart(candidates, lowerIsBetter);
  return (
    <Section>
      <SectionHeader
        title="Score by iteration"
        note={lowerIsBetter ? 'lower is better' : 'higher is better'}
      />
      {pending ? (
        <SectionBody>
          <Spinner label="LOADING ITERATIONS" />
        </SectionBody>
      ) : chart === null ? (
        <SectionBody>
          <Mono tone="ink-3">No measured iterations to chart.</Mono>
        </SectionBody>
      ) : (
        <>
          <SectionBody padded={false} className="px-4.5 pt-3.5 pb-1">
            <ScoreChart chart={chart} />
          </SectionBody>
          <div className="flex gap-4.5 bg-surface px-4.5 pb-3 font-mono text-label uppercase tracking-label text-ink-3">
            <span className="flex items-center gap-1.5">
              <span className="size-2 bg-green" />
              kept
            </span>
            <span className="flex items-center gap-1.5">
              <span className="size-2 border-[1.25px] border-red bg-surface" />
              dropped
            </span>
            <span className="flex items-center gap-1.5">
              <span className="size-2 bg-ink-3" />
              baseline
            </span>
            <span className="flex items-center gap-1.5">
              <span className="h-0.5 w-3.5 bg-green" />
              best so far
            </span>
          </div>
        </>
      )}
    </Section>
  );
}

const MARK_CLASS: Record<DecisionTone, string> = {
  keep: 'fill-green',
  discard: 'fill-surface stroke-red',
  baseline: 'fill-ink-3',
  neutral: 'fill-ink-2',
};

function ScoreChart({ chart }: { chart: RunChart }) {
  return (
    <svg
      viewBox={`0 0 ${CHART_W} ${CHART_H}`}
      preserveAspectRatio="none"
      className="block h-[260px] w-full"
      role="img"
      aria-label={chart.label}
    >
      <rect
        x={AXIS_L}
        y={AXIS_T}
        width={PLOT_W}
        height={PLOT_H}
        className="fill-none stroke-rule-hard"
        strokeWidth={1}
      />
      {chart.grid.map((tick) => (
        <line
          key={tick.label}
          x1={AXIS_L}
          y1={tick.y}
          x2={AXIS_L + PLOT_W}
          y2={tick.y}
          className="stroke-rule"
          strokeWidth={1}
        />
      ))}
      {chart.baseline !== null && (
        <>
          <line
            x1={AXIS_L}
            y1={chart.baseline.y}
            x2={AXIS_L + PLOT_W}
            y2={chart.baseline.y}
            className="stroke-ink-3"
            strokeWidth={1}
            strokeDasharray="4 3"
          />
          <text
            x={AXIS_L + PLOT_W - 6}
            y={chart.baseline.y - 6}
            textAnchor="end"
            className="fill-ink-3 font-mono text-micro tracking-group"
          >
            {chart.baseline.label}
          </text>
        </>
      )}
      <polyline points={chart.trail} className="fill-none stroke-green" strokeWidth={1.75} />
      {chart.marks.map((mark) => (
        <rect
          key={`${mark.iter}-${mark.x}`}
          x={mark.x - (mark.tone === 'keep' ? 3.5 : 3)}
          y={mark.y - (mark.tone === 'keep' ? 3.5 : 3)}
          width={mark.tone === 'keep' ? 7 : 6}
          height={mark.tone === 'keep' ? 7 : 6}
          strokeWidth={mark.tone === 'discard' ? 1.25 : 0}
          className={MARK_CLASS[mark.tone]}
        >
          <title>{`#${mark.iter}: ${formatScore(mark.score)} (${mark.tone})`}</title>
        </rect>
      ))}
      <line
        x1={AXIS_L}
        y1={chart.best.y}
        x2={chart.best.x}
        y2={chart.best.y}
        className="stroke-green"
        strokeWidth={1}
        strokeDasharray="4 3"
      />
      <text
        x={AXIS_L + 6}
        y={chart.best.y - 6}
        className="fill-green font-mono text-micro tracking-group"
      >
        {chart.best.label}
      </text>
      {chart.grid.map((tick) => (
        <text
          key={tick.label}
          x={AXIS_L - 8}
          y={tick.y + 3.5}
          textAnchor="end"
          className="fill-ink-3 font-mono text-label"
        >
          {tick.label}
        </text>
      ))}
      {chart.marks.map((mark) => (
        <text
          key={`x-${mark.iter}-${mark.x}`}
          x={mark.x}
          y={AXIS_T + PLOT_H + 18}
          textAnchor="middle"
          className="fill-ink-3 font-mono text-label"
        >
          {mark.iter}
        </text>
      ))}
    </svg>
  );
}

// --- iterations --------------------------------------------------------------

const iterHelper = createDataColumnHelper<Candidate>();

function IterationsTable({
  runId,
  candidates,
  pending,
  baseline,
  best,
  lowerIsBetter,
}: {
  runId: string;
  candidates: Candidate[];
  pending: boolean;
  baseline: number | null;
  best: number | null;
  lowerIsBetter: boolean;
}) {
  const iters = useMemo(() => candidates.filter((c) => !isBaseline(c)), [candidates]);

  const columns = useMemo(
    () =>
      iterHelper.columns([
        iterHelper.accessor('iter', {
          header: 'Iter',
          meta: { align: 'end', className: 'font-mono text-data' },
          cell: ({ getValue }) => getValue() ?? '—',
        }),
        iterHelper.accessor('lane', {
          header: 'Lane',
          meta: { className: 'font-mono text-data font-semibold text-ink-2' },
          cell: ({ getValue }) => getValue() ?? '—',
        }),
        iterHelper.accessor('kind', {
          header: 'Kind',
          cell: ({ getValue }) => (
            <Mono size="label" tone="ink-3" uppercase>
              {getValue() ?? '—'}
            </Mono>
          ),
        }),
        iterHelper.accessor('score', {
          header: 'Score',
          meta: { align: 'end' },
          cell: ({ getValue }) => {
            const score = getValue();
            return (
              <Score
                value={formatScore(score)}
                best={best !== null && score !== null && score !== undefined && score === best}
              />
            );
          },
        }),
        iterHelper.display({
          id: 'vs_base',
          header: 'vs base',
          meta: { align: 'end' },
          cell: ({ row }) => (
            <Delta
              percent={percentChange(row.original.score, baseline)}
              betterWhen={lowerIsBetter ? 'lower' : 'higher'}
            />
          ),
        }),
        iterHelper.accessor('decision', {
          header: 'Decision',
          cell: ({ getValue }) => {
            const decision = getValue();
            return decision ? (
              <Status status={decision} tone={DECISION_TONE[decisionTone(decision)]} />
            ) : (
              <Mono tone="ink-3">—</Mono>
            );
          },
        }),
        iterHelper.accessor('pr_url', {
          header: 'PR',
          cell: ({ getValue }) => {
            const url = getValue();
            return url ? <PrLink url={url} compact /> : <Mono tone="ink-3">—</Mono>;
          },
        }),
      ]),
    [baseline, best, lowerIsBetter],
  );

  const table = useDataTable({ columns, data: iters, enableSorting: false });

  if (pending) {
    return (
      <SectionBody>
        <Spinner label="LOADING ITERATIONS" />
      </SectionBody>
    );
  }

  return (
    <DataTable
      table={table}
      empty={<Empty title="NO ITERATIONS" />}
      renderSubRow={(row) =>
        row.original.iter === null || row.original.iter === undefined ? (
          <div className="px-4.5 py-2">
            <Mono tone="ink-3">No diff captured for this iteration.</Mono>
          </div>
        ) : (
          <DiffView runId={runId} iter={row.original.iter} />
        )
      }
    />
  );
}

function DiffView({ runId, iter }: { runId: string; iter: number }) {
  const diff = useQuery({
    queryKey: ['artifact', runId, `diffs/iter-${iter}.patch`],
    queryFn: () => fetchArtifact(runId, `diffs/iter-${iter}.patch`),
  });

  if (diff.isPending) {
    return (
      <div className="px-4.5 py-2">
        <Spinner label="LOADING DIFF" />
      </div>
    );
  }
  if (diff.isError) {
    return (
      <div className="px-4.5 py-2">
        <Mono tone="ink-3">No diff captured for this iteration.</Mono>
      </div>
    );
  }
  const files = parseUnifiedDiff(diff.data);
  if (files.length === 0) {
    return (
      <div className="px-4.5 py-2">
        <Mono tone="ink-3">No file changes.</Mono>
      </div>
    );
  }
  return (
    <>
      {files.map((file) => (
        <DiffFileBlock key={file.path} file={file} />
      ))}
    </>
  );
}

function DiffFileBlock({ file }: { file: DiffFile }) {
  let added = 0;
  let deleted = 0;
  for (const hunk of file.hunks) {
    for (const line of hunk.lines) {
      if (line.kind === 'add') added += 1;
      if (line.kind === 'del') deleted += 1;
    }
  }
  return (
    <div>
      <div className="flex items-center gap-2.5 border-b border-rule px-4.5 py-1.5">
        <span className="border border-rule-hard px-1.5 py-px font-mono text-micro font-bold uppercase tracking-group text-ink-2">
          {file.status}
        </span>
        {file.renamedFrom && (
          <>
            <span className="font-mono text-data text-ink-3">{file.renamedFrom}</span>
            <span className="text-ink-3">→</span>
          </>
        )}
        <span className="font-mono text-data font-semibold break-all">{file.path}</span>
        <span className="ml-auto font-mono text-data">
          <b className="text-green">+{added}</b> <b className="text-red">-{deleted}</b>
        </span>
      </div>
      {file.hunks.length === 0 ? (
        <div className="bg-surface px-4.5 py-2">
          <Mono tone="ink-3">No textual changes.</Mono>
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse bg-surface">
            <tbody>
              {file.hunks.map((hunk, hi) => (
                <HunkRows key={hi} heading={hunk.heading} lines={hunk.lines} />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

const GUTTER = 'w-px border-r border-rule bg-sunk px-2 text-right text-ink-3 select-none';
const CODE = 'px-2.5 font-mono text-data leading-[1.6] whitespace-pre text-ink';

function HunkRows({ heading, lines }: { heading: string; lines: DiffFile['hunks'][number]['lines'] }) {
  return (
    <>
      <tr>
        <td className={GUTTER} />
        <td className={GUTTER} />
        <td className={cn(CODE, 'bg-sunk text-ink-3')}>{heading}</td>
      </tr>
      {lines.map((line, li) => (
        <tr key={li}>
          <td className={cn(GUTTER, 'font-mono text-data leading-[1.6]')}>{line.oldNo ?? ''}</td>
          <td className={cn(GUTTER, 'font-mono text-data leading-[1.6]')}>{line.newNo ?? ''}</td>
          <td
            className={cn(
              CODE,
              line.kind === 'add' && 'bg-green/14',
              line.kind === 'del' && 'bg-red/14',
            )}
          >
            {line.kind === 'add' ? '+' : line.kind === 'del' ? '-' : ' '}
            {line.text}
          </td>
        </tr>
      ))}
    </>
  );
}

// --- cost breakdown ----------------------------------------------------------

// Per-iteration columns stay only while the table still reads at a glance.
const MAX_ITER_COLUMNS = 6;

const costHelper = createDataColumnHelper<TaskCostRow>();

/// Per-task cost from the run's graph results. Rendered only when the run admitted a plan AND some
/// task recorded a nonzero cost — otherwise there is nothing to break down and the section is
/// simply absent (same quiet treatment as the task graph itself).
function CostSection({ runId, runCost }: { runId: string; runCost: number | null | undefined }) {
  const query = $api.useQuery(
    'get',
    '/api/runs/{run_id}/graph',
    { params: { path: { run_id: runId } } },
    { retry: 2 },
  );
  const graph: RunGraph | undefined = query.data;

  const breakdown = useMemo(
    () => (graph ? buildCostBreakdown(graph.results, runCost ?? null) : null),
    [graph, runCost],
  );
  const rows = useMemo(() => breakdown?.rows ?? [], [breakdown]);
  const iters = useMemo(() => breakdown?.iters ?? [], [breakdown]);
  const showIters = iters.length > 1 && iters.length <= MAX_ITER_COLUMNS;
  const tasksTotal = breakdown?.tasksTotal ?? 0;

  const columns = useMemo(
    () => [
      costHelper.display({
        id: 'task',
        header: 'Task',
        meta: { className: 'font-mono text-data text-ink-2' },
        cell: ({ row }) => row.original.task,
      }),
      ...(showIters
        ? iters.map((i) =>
            costHelper.display({
              id: `iter-${i}`,
              header: `iter ${i}`,
              meta: { align: 'end', shrink: true, className: 'font-mono text-data' },
              cell: ({ row }) => formatCost(row.original.byIter.get(i)),
            }),
          )
        : []),
      costHelper.display({
        id: 'total',
        header: 'Total',
        meta: { align: 'end', shrink: true, className: 'font-mono text-data' },
        cell: ({ row }) => formatCost(row.original.total),
      }),
      costHelper.display({
        id: 'share',
        header: 'Share',
        meta: { width: '30%' },
        cell: ({ row }) => (
          <span
            className="block h-2 bg-green"
            style={{ width: tasksTotal === 0 ? '0%' : `${(row.original.total / tasksTotal) * 100}%` }}
          />
        ),
      }),
    ],
    [showIters, iters, tasksTotal],
  );

  const table = useDataTable({ columns, data: rows, enableSorting: false });

  if (!breakdown) return null;

  return (
    <Section className="border-r border-rule-hard max-wide:border-r-0">
      <SectionHeader title="Cost" note={`${formatCost(breakdown.tasksTotal)} across graded tasks`} />
      <DataTable
        table={table}
        footer={
          <>
            {breakdown.runTotal !== null && <>run total {formatCost(breakdown.runTotal)}</>}
            {breakdown.outsideTasks !== null && (
              <> · outside graded tasks: {formatCost(breakdown.outsideTasks)}</>
            )}
          </>
        }
      />
    </Section>
  );
}

// --- artifacts ---------------------------------------------------------------

interface ArtifactEntry {
  path: string;
  size_bytes: number | null;
}

function artifactEntries(data: unknown): ArtifactEntry[] {
  if (typeof data !== 'object' || data === null || !('entries' in data)) return [];
  const raw: unknown = data.entries;
  const list: readonly unknown[] = Array.isArray(raw) ? raw : [];
  const out: ArtifactEntry[] = [];
  for (const item of list) {
    if (typeof item !== 'object' || item === null || !('path' in item)) continue;
    const path: unknown = item.path;
    if (typeof path !== 'string') continue;
    const size: unknown = 'size_bytes' in item ? item.size_bytes : null;
    out.push({ path, size_bytes: typeof size === 'number' ? size : null });
  }
  return out;
}

function artifactHref(runId: string, path: string): string {
  // Manifest paths are already-safe segments; only the run id can hold odd characters (the same
  // rule fetchArtifact follows — the path's slashes must survive).
  return `/api/runs/${encodeURIComponent(runId)}/artifacts/${path}`;
}

const artifactHelper = createDataColumnHelper<ArtifactEntry>();

/// The run's artifact directory, from the typed manifest endpoint. html opens in a tab (served
/// under a sandbox CSP); everything else downloads. RESULTS.md / summary.json additionally expand
/// to an inline view. Old runs with nothing published get the same quiet text as the flow section.
function ArtifactsPanel({ runId }: { runId: string }) {
  const manifest = $api.useQuery(
    'get',
    '/api/runs/{run_id}/artifacts',
    { params: { path: { run_id: runId } } },
    { retry: 2 },
  );
  const entries = artifactEntries(manifest.data);

  const columns = useMemo(
    () =>
      artifactHelper.columns([
        artifactHelper.accessor('path', {
          header: 'File',
          meta: { pad: 'tight', shrink: true },
          cell: ({ getValue }) => (
            <Identifier href={artifactHref(runId, getValue())}>{getValue()}</Identifier>
          ),
        }),
        artifactHelper.accessor('size_bytes', {
          header: 'Size',
          meta: { align: 'end', className: 'font-mono text-data text-ink-3' },
          cell: ({ getValue }) => formatBytes(getValue()),
        }),
        artifactHelper.display({
          id: 'actions',
          header: 'Actions',
          cell: ({ row }) =>
            row.original.path.toLowerCase().endsWith('.html') ? (
              <a
                className="font-mono text-data text-ink-2 hover:text-ink"
                href={artifactHref(runId, row.original.path)}
                target="_blank"
                rel="noopener noreferrer"
              >
                open
              </a>
            ) : (
              <a
                className="font-mono text-data text-ink-2 hover:text-ink"
                href={artifactHref(runId, row.original.path)}
                download={`${runId}-${row.original.path.replaceAll('/', '-')}`}
              >
                download
              </a>
            ),
        }),
      ]),
    [runId],
  );

  const table = useDataTable({
    columns,
    data: entries,
    enableSorting: false,
    getRowCanExpand: (row) => textKind(row.original.path) !== null,
  });

  if (manifest.isPending) {
    return (
      <Section>
        <SectionHeader title="Artifacts" />
        <SectionBody>
          <Spinner label="LOADING ARTIFACTS" />
        </SectionBody>
      </Section>
    );
  }
  if (manifest.isError || entries.length === 0) {
    return (
      <Section>
        <SectionHeader title="Artifacts" />
        <SectionBody>
          <Mono tone="ink-3">No artifacts recorded for this run.</Mono>
        </SectionBody>
      </Section>
    );
  }

  return (
    <Section>
      <SectionHeader title="Artifacts" note={`${entries.length} files`} />
      <DataTable
        table={table}
        renderSubRow={(row) => {
          const kind = textKind(row.original.path);
          if (kind === null) return null;
          return <ArtifactInline runId={runId} path={row.original.path} kind={kind} />;
        }}
      />
    </Section>
  );
}

const FILE_PANES = ['list', 'content'];

/// The files the run's tasks declared and captured — a playbook's reports and findings land here,
/// not in the artifact store. Master-detail: pick a file on the left, read it on the right —
/// markdown renders, json and text show as code, an image renders inline, anything else downloads.
/// The selected file is the URL, so the address bar is always a link someone else can open.

function CopyLinkButton({ route, className }: { route: RunFileRoute; className?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      className={cn('flex-none px-1.5 py-0 text-micro tracking-label uppercase', className)}
      title="Copy a link to this file"
      onClick={() => {
        void navigator.clipboard.writeText(runFileUrl(window.location.origin, route)).then(() => {
          setCopied(true);
          window.setTimeout(() => {
            setCopied(false);
          }, 1500);
        });
      }}
    >
      {copied ? 'copied' : 'copy link'}
    </Button>
  );
}

function FilesPanel({
  runId,
  launchKey,
  fileKey,
}: {
  runId: string;
  launchKey: string | null;
  fileKey: string | null;
}) {
  const navigate = useNavigate();
  const listing = $api.useQuery(
    'get',
    '/api/runs/{run_id}/files',
    { params: { path: { run_id: runId } } },
    { retry: 2 },
  );
  const graph = $api.useQuery(
    'get',
    '/api/runs/{run_id}/graph',
    { params: { path: { run_id: runId } } },
    { retry: 2 },
  );
  // A file captured from a task that did not pass is evidence of the failure, and must not read
  // as the output of a task that succeeded.
  const failed = useMemo(() => {
    const out = new Set<string>();
    for (const [task, result] of latestResults(graph.data?.results ?? [])) {
      if (toneOf(result.status) === 'fail') out.add(task);
    }
    return out;
  }, [graph.data]);
  const entries = listing.data?.files ?? [];
  const selection = selectRunFile(entries, fileKey);
  const anchor = useRef<HTMLDivElement>(null);
  const anchored = useRef(false);
  const openedOnDeepLink = useRef(fileKey !== null);
  const loaded = entries.length;

  useEffect(() => {
    if (
      !shouldAnchorFiles({
        openedOnDeepLink: openedOnDeepLink.current,
        anchored: anchored.current,
        loaded,
      })
    )
      return;
    const el = anchor.current;
    if (el === null) return;
    anchored.current = true;
    el.scrollIntoView({ block: 'start' });
  }, [loaded]);

  if (listing.isPending) {
    return (
      <Section>
        <SectionHeader title="Files" />
        <SectionBody>
          <Spinner label="LOADING FILES" />
        </SectionBody>
      </Section>
    );
  }
  // An error is not an empty capture: say which one happened.
  if (listing.isError) {
    return (
      <Section>
        <SectionHeader title="Files" />
        <SectionBody>
          <Mono tone="ink-3">Files could not be loaded: {formatError(listing.error)}</Mono>
        </SectionBody>
      </Section>
    );
  }
  if (selection.state === 'empty') {
    return (
      <Section>
        <SectionHeader title="Files" />
        <SectionBody>
          <Mono tone="ink-3">No files captured for this run.</Mono>
        </SectionBody>
      </Section>
    );
  }

  const selectedKey = selection.state === 'found' ? selection.file.key : null;

  return (
    <div ref={anchor}>
      <Section>
        <SectionHeader
          title="Files"
          note={`${entries.length} files`}
          actions={
            selection.state === 'found' && (
              <CopyLinkButton route={{ launchKey, runId, fileKey: selection.file.key }} />
            )
          }
        />
        <Split
          id="crucible.run-files"
          panelIds={FILE_PANES}
          orientation="horizontal"
          className="h-[32rem] border border-rule-hard"
        >
          <SplitPane
            id="list"
            defaultSize="24%"
            minSize="3rem"
            className="overflow-y-auto bg-surface"
          >
            {entries.map((e) => (
              <div
                key={e.key}
                className={cn(
                  'group flex w-full items-baseline',
                  e.key === selectedKey ? 'bg-hi' : 'bg-transparent',
                )}
              >
                <button
                  type="button"
                  aria-pressed={e.key === selectedKey}
                  onClick={() => {
                    void navigate(runFilePath({ launchKey, runId, fileKey: e.key }), {
                      replace: true,
                    });
                  }}
                  title={e.key}
                  className={cn(
                    'flex min-w-0 flex-1 cursor-pointer items-baseline justify-between gap-2 border-0 bg-transparent px-2 py-0.5 text-left font-mono text-micro tracking-label',
                    e.key === selectedKey ? 'font-semibold text-ink' : 'text-ink-2 hover:text-ink',
                  )}
                >
                  <span className="min-w-0 truncate">{e.key}</span>
                  {failed.has(producerOf(e)) && (
                    <span className="flex-none text-red uppercase" data-testid="failure-evidence">
                      failure evidence
                    </span>
                  )}
                  <span className="flex-none text-ink-3">{formatBytes(e.size_bytes)}</span>
                </button>
                <CopyLinkButton
                  route={{ launchKey, runId, fileKey: e.key }}
                  className="opacity-0 group-hover:opacity-100 focus-visible:opacity-100"
                />
              </div>
            ))}
          </SplitPane>
          <SplitHandle orientation="horizontal" label="Resize the file content" />
          <SplitPane id="content" minSize="3rem">
            <div className="flex h-full min-h-0 flex-col">
              <p className="m-0 flex items-baseline gap-3 border-b border-rule bg-sunk px-2 py-0.5 font-mono text-micro tracking-label">
                <span className="min-w-0 truncate text-ink-2">
                  {selection.state === 'found' ? selection.file.key : selection.key}
                </span>
                {selection.state === 'found' && failed.has(producerOf(selection.file)) && (
                  <span className="flex-none text-red uppercase">
                    captured from a task that failed
                  </span>
                )}
                {selection.state === 'found' && (
                  <a
                    className="ml-auto flex-none text-ink-2 uppercase hover:text-ink"
                    href={runFileApiPath(runId, selection.file.key)}
                    download={downloadName(runId, selection.file.key)}
                  >
                    download
                  </a>
                )}
              </p>
              {selection.state === 'found' ? (
                <RunFileContent
                  runId={runId}
                  fileKey={selection.file.key}
                  path={selection.file.path}
                />
              ) : (
                <p className="m-0 px-2 py-1.5 font-mono text-data text-ink-3">
                  This run captured no file {selection.key}. Pick one from the list.
                </p>
              )}
            </div>
          </SplitPane>
        </Split>
      </Section>
    </div>
  );
}

/// A captured image, fetched as bytes and tagged with the type its name implies: the endpoint
/// serves an octet stream, and an object URL renders without asking the browser to guess.
function RunFileImage({
  runId,
  fileKey,
  mediaType,
}: {
  runId: string;
  fileKey: string;
  mediaType: string;
}) {
  const query = useQuery({
    queryKey: ['run-file-blob', runId, fileKey],
    queryFn: () => fetchRunFileBlob(runId, fileKey, mediaType),
  });
  const blob = query.data;
  const [src, setSrc] = useState<string | null>(null);

  useEffect(() => {
    if (blob === undefined) return;
    const url = URL.createObjectURL(blob);
    setSrc(url);
    return () => {
      URL.revokeObjectURL(url);
      setSrc(null);
    };
  }, [blob]);

  if (query.isError) {
    return (
      <div className="px-2 py-1.5">
        <Mono tone="ink-3">{formatError(query.error)}</Mono>
      </div>
    );
  }
  if (src === null) {
    return (
      <div className="px-2 py-1.5">
        <Spinner label="LOADING FILE" />
      </div>
    );
  }
  return (
    <div className="min-h-0 flex-1 overflow-auto bg-paper px-4 py-3">
      <img src={src} alt={fileKey} className="max-w-full" />
    </div>
  );
}

function RunFileContent({
  runId,
  fileKey,
  path,
}: {
  runId: string;
  fileKey: string;
  path: string;
}) {
  const mediaType = imageMimeType(path);
  if (mediaType !== null) {
    return <RunFileImage runId={runId} fileKey={fileKey} mediaType={mediaType} />;
  }
  return <RunFileText runId={runId} fileKey={fileKey} kind={textKind(path)} />;
}

function RunFileText({
  runId,
  fileKey,
  kind,
}: {
  runId: string;
  fileKey: string;
  kind: TextKind | null;
}) {
  const query = useQuery({
    queryKey: ['run-file', runId, fileKey],
    queryFn: () => fetchRunFile(runId, fileKey),
    enabled: kind !== null,
  });
  if (kind === null) {
    return (
      <p className="m-0 px-2 py-1.5 font-mono text-data text-ink-3">
        Not viewable inline; download it instead.
      </p>
    );
  }
  if (query.isPending) {
    return (
      <div className="px-2 py-1.5">
        <Spinner label="LOADING FILE" />
      </div>
    );
  }
  if (query.isError) {
    return (
      <div className="px-2 py-1.5">
        <Mono tone="ink-3">{formatError(query.error)}</Mono>
      </div>
    );
  }
  if (kind === 'markdown') {
    return (
      <div className="min-h-0 flex-1 overflow-auto bg-paper px-4 py-3">
        <MarkdownView markdown={query.data ?? ''} />
      </div>
    );
  }
  return (
    <div className="min-h-0 flex-1">
      <CodeSurface
        path={fileKey}
        value={kind === 'json' ? prettyJson(query.data ?? '') : (query.data ?? '')}
        readOnly
        label={`${fileKey} contents`}
      />
    </div>
  );
}

function ArtifactInline({ runId, path, kind }: { runId: string; path: string; kind: TextKind }) {
  const query = useQuery({
    queryKey: ['artifact', runId, path],
    queryFn: () => fetchArtifact(runId, path),
  });
  if (query.isPending) {
    return (
      <div className="px-4.5 py-2">
        <Spinner label="LOADING ARTIFACT" />
      </div>
    );
  }
  if (query.isError) {
    return (
      <div className="px-4.5 py-2">
        <Mono tone="ink-3">{formatError(query.error)}</Mono>
      </div>
    );
  }
  if (kind === 'markdown') {
    return (
      <div className="max-h-120 overflow-auto bg-surface px-4.5 py-3">
        <MarkdownView markdown={query.data ?? ''} />
      </div>
    );
  }
  return (
    <pre className="m-0 max-h-120 overflow-auto bg-surface px-4.5 py-3 font-mono text-data whitespace-pre-wrap text-ink">
      {query.data ? (kind === 'json' ? prettyJson(query.data) : query.data) : ''}
    </pre>
  );
}
