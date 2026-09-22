import { Link } from 'react-router-dom';
import type { components } from '../api/schema.d';
import { $api } from '../api/client';
import { useLiveEvents } from '../api/useLiveEvents';
import { CostByDayChart } from '../charts/CostByDayChart';
import { IssuesByStatusChart } from '../charts/IssuesByStatusChart';
import { IssuesByTierChart } from '../charts/IssuesByTierChart';
import { CapGauge } from '../charts/CapGauge';
import { SpendByTagChart } from '../charts/SpendByTagChart';
import { AutopilotStatusChip } from '../AutopilotStatusChip';
import { ClustersStrip } from './ClustersStrip';
import { formatError } from '../api/errors';
import {
  isZeroStage,
  splitFunnelStages,
  stageTone,
  type FunnelStage,
  type StageTone,
} from './funnel';
import { truncateReason } from './turns';
import { relativeTime } from './journeyView';
import {
  cn,
  Empty,
  Identifier,
  LoadingBlock,
  Mono,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
  Spec,
  Spinner,
  type SpecItem,
} from '../ui';

const TONE_CLASS: Record<StageTone, string> = {
  discovery: 'border-l-blue',
  gate: 'border-l-ink-3',
  progress: 'border-l-amber',
  success: 'border-l-green',
  attention: 'border-l-red',
};

function StageBox({ stage }: { stage: FunnelStage }) {
  return (
    <Link
      to={stage.href_hint}
      className={cn(
        'flex min-w-[7rem] flex-col justify-center border-r border-l-2 border-rule px-3.5 py-1.5 hover:bg-hi',
        TONE_CLASS[stageTone(stage.key)],
        isZeroStage(stage) && 'opacity-45',
      )}
    >
      <span className="font-mono text-figure font-semibold text-ink">{stage.count}</span>
      <span className="font-mono text-label uppercase tracking-group whitespace-nowrap text-ink-3">
        {stage.label}
      </span>
    </Link>
  );
}

function FunnelStrip({ stages }: { stages: FunnelStage[] }) {
  const { inBand, outOfBand } = splitFunnelStages(stages);
  return (
    <div className="flex flex-wrap items-stretch border-b border-rule-hard bg-sunk">
      {inBand.map((stage) => (
        <StageBox key={stage.key} stage={stage} />
      ))}
      {outOfBand.length > 0 ? <span className="flex-1" /> : null}
      {outOfBand.map((stage) => (
        <StageBox key={stage.key} stage={stage} />
      ))}
    </div>
  );
}

type RunRow = components['schemas']['RunRowDto'];
type ApprovalsDto = components['schemas']['ApprovalsDto'];

function HappeningNowStrip({
  runningRuns,
  approvals,
  autopilotEnabled,
}: {
  runningRuns: RunRow[];
  approvals: ApprovalsDto | undefined;
  autopilotEnabled: boolean | undefined;
}) {
  const pendingApprovals = approvals?.awaiting_approval.length ?? 0;
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-rule bg-surface px-4.5 py-2">
      <Mono size="label" tone="ink-3" uppercase className="tracking-section">
        Happening now
      </Mono>
      {runningRuns.length === 0 ? (
        <Mono tone="ink-3">no runs in flight</Mono>
      ) : (
        runningRuns.map((run) => (
          <span key={run.run_id} className="flex items-center gap-2">
            <Identifier variant="inline" to={`/runs/${run.run_id}`}>
              {run.run_id}
            </Identifier>
            <Mono>{run.issue_key ?? '—'}</Mono>
          </span>
        ))
      )}
      <Link to="/approvals" className="font-mono text-data text-ink-2 hover:text-ink">
        {pendingApprovals} pending approval{pendingApprovals === 1 ? '' : 's'}
      </Link>
      {autopilotEnabled === false ? (
        <Mono tone="amber" weight="semibold" uppercase>
          autopilot is off
        </Mono>
      ) : null}
    </div>
  );
}

type WorkPod = components['schemas']['WorkPodDto'];

/** The last few failed turns, each a jump into the /turns ledger — the card that would have made
 * last night's 50-pod failure storm visible on the first glance instead of after 11 hours. */
function RecentTurnFailures({ failures }: { failures: WorkPod[] }) {
  if (failures.length === 0) {
    return <Mono tone="ink-3">No recent failures.</Mono>;
  }
  return (
    <ul className="m-0 list-none p-0">
      {failures.map((turn) => (
        <li
          key={turn.pod_name}
          className="flex flex-wrap items-baseline gap-x-3 border-b border-rule py-1.5 last:border-b-0"
        >
          <Link to="/turns?state=failed" className="font-mono text-data font-semibold text-ink hover:underline">
            {turn.kind}
            {turn.issue_key ? ` · ${turn.issue_key}` : ''}
          </Link>
          <Mono tone="ink-3">{relativeTime(turn.terminal_at ?? turn.created_at)}</Mono>
          {truncateReason(turn.error, 90) ? (
            <Mono tone="red">{truncateReason(turn.error, 90)}</Mono>
          ) : null}
        </li>
      ))}
    </ul>
  );
}

function capNote(cap: number | null | undefined, format: (value: number) => string): string {
  return cap === null || cap === undefined ? 'uncapped' : `/ ${format(cap)}`;
}

export function DashboardPage() {
  useLiveEvents();

  const overview = $api.useQuery('get', '/api/overview');
  const ledger = $api.useQuery('get', '/api/ledger/summary');
  const ledgerByTag = $api.useQuery('get', '/api/ledger/by-tag');
  const failedTurns = $api.useQuery('get', '/api/turns', {
    params: { query: { state: 'failed', limit: 5 } },
  });
  const funnel = $api.useQuery('get', '/api/funnel');
  const runningRuns = $api.useQuery('get', '/api/runs', {
    params: { query: { status: 'running', sort: 'created', dir: 'desc', limit: 5, offset: 0 } },
  });
  const approvals = $api.useQuery('get', '/api/approvals');
  const autopilot = $api.useQuery('get', '/api/autopilot');

  if (overview.isError || ledger.isError) {
    return (
      <Empty
        title="DASHBOARD UNAVAILABLE"
        description={formatError(overview.error ?? ledger.error)}
      />
    );
  }

  if (overview.isPending || ledger.isPending) {
    return <LoadingBlock label="LOADING DASHBOARD" />;
  }

  const { data: overviewData } = overview;
  const { data: ledgerData } = ledger;

  const specs: SpecItem[] = [
    {
      label: 'Running',
      value: overviewData.running.current,
      note: capNote(overviewData.running.cap, (v) => String(v)),
    },
    {
      label: 'Scopes today',
      value: overviewData.scopes_today.current,
      note: capNote(overviewData.scopes_today.cap, (v) => String(v)),
    },
    {
      label: 'Cost today',
      value: `$${overviewData.cost_today.current.toFixed(2)}`,
      note: capNote(overviewData.cost_today.ceiling, (v) => `$${v.toFixed(2)}`),
    },
  ];

  return (
    <div>
      <PageHeader
        eyebrow="Control plane"
        title="Dashboard"
        description="Pipeline volume, capacity against the admission caps, and where today's spend went."
      />

      {funnel.isSuccess ? <FunnelStrip stages={funnel.data.stages} /> : null}

      <HappeningNowStrip
        runningRuns={runningRuns.data ?? []}
        approvals={approvals.data}
        autopilotEnabled={autopilot.data?.enabled}
      />

      <ClustersStrip />

      <Section>
        <SectionHeader title="Capacity" note="current against cap" />
        <SectionBody className="pb-0">
          <Spec items={specs} />
        </SectionBody>
        <div className="grid grid-cols-1 bg-surface wide:grid-cols-3 wide:divide-x wide:divide-rule">
          <div className="px-4.5 py-3.5">
            <CapGauge
              title="Running"
              current={overviewData.running.current}
              cap={overviewData.running.cap}
            />
          </div>
          <div className="px-4.5 py-3.5">
            <CapGauge
              title="Scopes"
              current={overviewData.scopes_today.current}
              cap={overviewData.scopes_today.cap}
            />
          </div>
          <div className="px-4.5 py-3.5">
            <CapGauge
              title="Cost ($)"
              current={overviewData.cost_today.current}
              cap={overviewData.cost_today.ceiling}
            />
          </div>
        </div>
      </Section>

      <div className="grid grid-cols-1 wide:grid-cols-3">
        <AutopilotStatusChip />
        <Section className="wide:col-span-2 wide:border-l wide:border-l-rule-hard">
          <SectionHeader title="Recent turn failures" />
          <SectionBody>
            {failedTurns.isError ? (
              <Mono tone="red">{formatError(failedTurns.error)}</Mono>
            ) : failedTurns.isPending ? (
              <Spinner />
            ) : (
              <RecentTurnFailures failures={failedTurns.data} />
            )}
          </SectionBody>
        </Section>
      </div>

      <div className="grid grid-cols-1 wide:grid-cols-2">
        <Section>
          <SectionHeader title="Spend by kind" note="today" />
          <SectionBody>
            {ledgerByTag.isError ? (
              <Mono tone="red">{formatError(ledgerByTag.error)}</Mono>
            ) : ledgerByTag.isPending ? (
              <Spinner />
            ) : (
              <SpendByTagChart tags={ledgerByTag.data.tags} ceiling={ledgerByTag.data.ceiling} />
            )}
          </SectionBody>
        </Section>
        <Section className="wide:border-l wide:border-l-rule-hard">
          <SectionHeader title="Issues by status" />
          <SectionBody>
            <IssuesByStatusChart statuses={overviewData.statuses} />
          </SectionBody>
        </Section>
      </div>

      <Section>
        <SectionHeader title="Issues by tier" />
        <SectionBody>
          <IssuesByTierChart tiers={overviewData.tiers} />
        </SectionBody>
      </Section>

      <Section>
        <SectionHeader title="Cost by day" note="last 30 days" />
        <SectionBody>
          <CostByDayChart days={ledgerData.days} />
        </SectionBody>
      </Section>
    </div>
  );
}
