import { Link, useParams } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { useLiveEvents } from '../api/useLiveEvents';
import type { components } from '../api/schema';
import {
  Breadcrumb,
  Button,
  cn,
  CopyText,
  DetailHeader,
  Empty,
  Identifier,
  LoadingBlock,
  Mono,
  Section,
  SectionBody,
  SectionHeader,
  Spec,
  Status,
  statusTone,
} from '../ui';
import type { SpecItem } from '../ui';
import { issueStatusColor } from './issueStatus';
import { absoluteTime, relativeTime } from './journeyView';
import {
  dispatchFailure,
  dispatchTone,
  graphRunId,
  launchLabel,
  launchRunPath,
  launchVersion,
  originLabel,
  originLinks,
  relaunchPath,
} from './launchView';
import { AgentProviderTag } from './ProviderIcon';
import { runLabel } from './runLabel';
import { runStatusColor } from './runStatus';
import { formatCost } from './runReport';
import { RerunSection } from './RerunSection';
import { LaunchWallTime } from './LaunchWallTime';
import { RunLinks } from './RunLinks';
import { RunTaskGraph } from './RunTaskGraph';

type LaunchDetail = components['schemas']['PlaybookLaunchDetailDto'];

/// One launch: what it was authorized to run, where its dispatch got to, and the graph its run
/// drew. A playbook row never ranks and never scopes, so none of the issue journey applies to it —
/// this page is what a launch key opens instead.
export function PlaybookLaunchDetailPage() {
  useLiveEvents();

  const { key = '' } = useParams<{ key: string }>();
  const detail = $api.useQuery(
    'get',
    '/api/playbook-runs/{key}',
    { params: { path: { key } } },
    { refetchInterval: 15_000 },
  );

  return (
    <>
      <Breadcrumb
        items={[{ label: 'Playbook runs', to: '/playbook-runs' }, { label: launchLabel(key) }]}
      />
      {detail.isError ? (
        <Empty title="Launch unavailable" description={formatError(detail.error)} />
      ) : detail.isPending ? (
        <LoadingBlock label="Loading launch" />
      ) : (
        <LaunchView detail={detail.data} />
      )}
    </>
  );
}

function LaunchView({ detail }: { detail: LaunchDetail }) {
  const { launch, dispatch } = detail;
  const runId = graphRunId(detail);
  const cost = launch.cost_usd ?? null;
  const specs: SpecItem[] =
    cost !== null && cost > 0
      ? [{ label: 'Cost', value: formatCost(cost), note: `/ $${launch.max_cost.toFixed(2)}` }]
      : [];
  const dispatchNews =
    dispatch.state !== 'dispatched' || Boolean(dispatch.failure) || dispatch.failures > 0;

  return (
    <>
      <DetailHeader
        title={launch.playbook}
        badge={
          <Status status={launch.status} tone={statusTone(issueStatusColor(launch.status))} />
        }
        description={launch.description ?? undefined}
        meta={
          <>
            {detail.runs.map((run) => (
              <span key={run.run_id} className="flex items-center gap-1.5">
                <Identifier variant="inline" to={launchRunPath(launch.key, run.run_id)}>
                  {runLabel(run)}
                </Identifier>
                {run.status !== 'finished' && (
                  <Status status={run.status} tone={statusTone(runStatusColor(run.status))} />
                )}
              </span>
            ))}
            {originLinks(detail).map((link) => {
              const text = link.label === 'draft' ? launchVersion(launch) : link.value;
              return link.to === null ? (
                <span key={link.label}>{text}</span>
              ) : (
                <Link key={link.label} className="text-ink-2 hover:text-ink" to={link.to}>
                  {text}
                </Link>
              );
            })}
            {launch.origin !== 'draft' && <span>{originLabel(launch.origin)}</span>}
            {launch.created_by && <span>by {launch.created_by}</span>}
            {launch.agent_provider && (
              <span>
                via{' '}
                <AgentProviderTag provider={launch.agent_provider} model={launch.agent_model} />
              </span>
            )}
            <span title={absoluteTime(launch.created_at) ?? undefined}>
              launched {relativeTime(launch.created_at) ?? launch.created_at}
            </span>
            <LaunchWallTime launchKey={launch.key} />
            {runId !== null && <RunLinks runId={runId} />}
            <CopyText value={launch.key} />
          </>
        }
        aside={
          <div className="flex items-start gap-3">
            {specs.length > 0 && <Spec items={specs} />}
            <Button
              className="uppercase"
              render={<Link to={relaunchPath(launch.playbook, launch.key)} />}
            >
              Relaunch
            </Button>
          </div>
        }
      />

      {dispatchNews && <DispatchSection detail={detail} />}

      <ParamsSection launch={launch} />

      {runId !== null && <RunTaskGraph runId={runId} engineBounds />}

      {runId !== null && detail.runs[0]?.status !== 'running' && (
        <RerunSection detail={detail} runId={runId} />
      )}

      {launch.secrets_refusal && (
        <Section>
          <SectionHeader title="Secrets" note="this launch resolved no credentials" />
          <SectionBody className="grid gap-3">
            <p
              data-testid="secrets-refusal"
              className="m-0 whitespace-pre-wrap font-mono text-data text-red"
            >
              {launch.secrets_refusal}
            </p>
            <div>
              <Button className="uppercase" render={<Link to="/secrets" />}>
                Open secrets
              </Button>
            </div>
          </SectionBody>
        </Section>
      )}

      {launch.parked_reason && !launch.secrets_refusal && dispatch.state !== 'failed' && (
        <Section>
          <SectionHeader title="Parked" />
          <SectionBody className="whitespace-pre-wrap font-mono text-data text-ink-2">
            {launch.parked_reason}
          </SectionBody>
        </Section>
      )}
    </>
  );
}

/// Dispatch, first-class: whether the engine ever started and the error that stopped an attempt
/// that did not. Nothing about this is a ranking verdict, which is where a launch's failure used to
/// land.
function DispatchSection({ detail }: { detail: LaunchDetail }) {
  const { dispatch } = detail;
  const failure = dispatchFailure(dispatch);
  return (
    <Section>
      <SectionHeader
        title="Dispatch"
        note={dispatch.failures > 0 ? `${dispatch.failures} failed` : undefined}
      />
      <SectionBody className="grid gap-3">
        <div data-testid="dispatch-state">
          <Status status={dispatch.state} tone={dispatchTone(dispatch.state)} />
        </div>
        {failure && (
          <div>
            <Mono size="micro" tone="ink-3" uppercase className="tracking-section">
              {failure.label}
            </Mono>
            <p
              data-testid="dispatch-failure"
              className={cn(
                'mt-1 mb-0 whitespace-pre-wrap font-mono text-data',
                failure.blocking ? 'text-red' : 'text-ink-2',
              )}
            >
              {failure.error}
            </p>
            {failure.at && (
              <Mono size="micro" tone="ink-3" title={absoluteTime(failure.at) ?? undefined}>
                {relativeTime(failure.at) ?? failure.at}
              </Mono>
            )}
          </div>
        )}
      </SectionBody>
    </Section>
  );
}

/// The values and ceilings this launch froze: the argv the engine was handed, and what a relaunch
/// prefills from.
function ParamsSection({ launch }: { launch: LaunchDetail['launch'] }) {
  const entries = Object.entries(launch.params);
  return (
    <Section>
      <SectionHeader
        title="Parameters"
        note={`max $${launch.max_cost.toFixed(2)} / ${launch.max_time}${launch.advance_dedupe ? ' · dedupe advances' : ''}`}
      />
      <SectionBody>
        {entries.length === 0 ? (
          <Mono tone="ink-3">no params</Mono>
        ) : (
          <dl
            data-testid="launch-params"
            className="m-0 grid grid-cols-[repeat(auto-fill,minmax(13rem,1fr))] gap-x-6 gap-y-1 font-mono text-data"
          >
            {entries.map(([name, value]) => (
              <div key={name} className="flex min-w-0 items-baseline gap-2">
                <dt className="flex-none text-ink-3">{name}</dt>
                <dd className="m-0 min-w-0 truncate text-ink-2" title={String(value)}>
                  {String(value)}
                </dd>
              </div>
            ))}
          </dl>
        )}
        <div className="mt-2 flex flex-wrap items-center gap-3 text-micro">
          <CopyText value={launch.schema_digest} />
          {launch.schema_drifted && (
            <Status status="form moved since" tone="amber" pulse={false} />
          )}
        </div>
      </SectionBody>
    </Section>
  );
}
