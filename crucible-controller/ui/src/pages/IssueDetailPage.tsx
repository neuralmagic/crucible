import { useEffect, useState, type ReactNode } from 'react';
import { AlertDialog } from '@base-ui-components/react/alert-dialog';
import { Link, Navigate, useParams } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { MetaRow } from './formControls';
import { AgentProviderTag } from './ProviderIcon';
import { formatError } from '../api/errors';
import { openEventSource } from '../api/session';
import { LiveSession } from '../live/LiveSession';
import type { components } from '../api/schema';
import {
  ALERT_POPUP,
  Breadcrumb,
  Button,
  cn,
  DetailHeader,
  DIALOG_BACKDROP,
  DIALOG_TITLE,
  Empty,
  Identifier,
  LoadingBlock,
  Mono,
  PrLink,
  Section,
  SectionBody,
  SectionHeader,
  Spec,
  Status,
  statusTone,
  Tooltip,
} from '../ui';
import type { SpecItem } from '../ui';
import { issueStatusColor, tierTone } from './issueStatus';
import { runStatusColor } from './runStatus';
import {
  absoluteTime,
  ghostTitle,
  ghostSteps,
  relativeTime,
  stepPresentation,
  type Tone,
} from './journeyView';
import { IssueBodySection } from './IssueBodySection';
import { IssueCommentsSection } from './IssueCommentsSection';
import { KindLabel } from './KindLabel';
import { staleCloseEvidence } from './parkedStale';
import { isLaunchKey, launchPath } from './launchView';
import { lastRankedAt, rankingRationale, verdictSource } from './rankProvenance';
import { ScenarioDetailSection } from './ScenarioDetailSection';

type JourneyStep = components['schemas']['JourneyStep'];
type InputKindDto = components['schemas']['InputKindDto'];

const SCOPEABLE_STATUSES = new Set(['new', 'parked']);

const TONE_SWATCH: Record<Tone, string> = {
  muted: 'bg-ink-3',
  info: 'bg-blue',
  success: 'bg-green',
  warning: 'bg-amber',
  danger: 'bg-red',
};

const INPUT =
  'border border-rule-hard bg-surface px-2 py-1 font-mono text-data text-ink placeholder:text-ink-3';

function disabledReason(status: string): string {
  switch (status) {
    case 'scoped': return 'Already scoped';
    case 'awaiting-approval': return 'Awaiting approval, scope already ran';
    case 'running': return 'A run is in progress';
    case 'done': return 'Issue is resolved';
    case 'pr-open': return 'PR is open for review';
    default: return `Cannot scope in ${status} state`;
  }
}

// Pull just the issue key out of a raw SSE event frame, no `any`/`as` (mirrors ActivityPage's guard).
function eventKeyOf(raw: string): string | null {
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof v === 'object' && v !== null && 'key' in v && typeof v.key === 'string') {
    return v.key;
  }
  return null;
}

/// Live step-lighting: a single SSE subscription filtered to this one issue's key. Any transition
/// for the issue invalidates its journey (+ detail) query, so new nodes appear mid-demo with no
/// reload. Refetch-on-event only — no client-side step synthesis.
function useJourneyLive(issueKey: string) {
  const queryClient = useQueryClient();
  useEffect(() => {
    if (!issueKey) return;
    const source = openEventSource('/api/events/stream');
    source.onmessage = (msg: MessageEvent<string>) => {
      if (eventKeyOf(msg.data) !== issueKey) return;
      void queryClient.invalidateQueries({
        predicate: (query) => {
          const key = query.queryKey;
          return Array.isArray(key) && key[0] === 'get' && typeof key[1] === 'string' && key[1].startsWith('/api/issues/');
        },
      });
    };
    return () => source.close();
  }, [issueKey, queryClient]);
}

export function IssueDetailPage() {
  const { key: rawKey } = useParams<{ key: string }>();
  const issueKey = rawKey ? decodeURIComponent(rawKey) : '';
  // Scoping/approval/PR is an issue's ladder; a launch runs a pinned pack once and climbs none of
  // it. An old link to one lands on the launch view instead of a page of blanks.
  const isLaunch = isLaunchKey(issueKey);

  useJourneyLive(isLaunch ? '' : issueKey);

  const whoami = $api.useQuery('get', '/api/whoami');
  const detail = $api.useQuery(
    'get',
    '/api/issues/{key}',
    { params: { path: { key: issueKey } } },
    { enabled: !isLaunch },
  );
  const journey = $api.useQuery(
    'get',
    '/api/issues/{key}/journey',
    { params: { path: { key: issueKey } } },
    { enabled: !isLaunch },
  );

  if (isLaunch) {
    return <Navigate to={launchPath(issueKey)} replace />;
  }

  if (!issueKey) {
    return <Empty title="NO ISSUE KEY" />;
  }

  if (detail.isError) {
    return <Empty title="ISSUE UNAVAILABLE" description={formatError(detail.error)} />;
  }

  if (detail.isPending) {
    return <LoadingBlock label="LOADING ISSUE" />;
  }

  const { issue } = detail.data;
  const role = whoami.data?.role;
  const title = detail.data.scenario?.title || issue.title || issue.key;

  const specs: SpecItem[] = [
    {
      label: 'Tier',
      value: issue.tier ?? '—',
      tone: issue.tier === 'T0' ? 'bad' : 'default',
    },
    { label: 'Priority', value: issue.priority === 0 ? '—' : String(issue.priority) },
  ];

  return (
    <>
      <Breadcrumb items={[{ label: 'Issues', to: '/issues' }, { label: issue.key }]} />

      <DetailHeader
        title={issue.key}
        badge={<Status status={issue.status} tone={statusTone(issueStatusColor(issue.status))} />}
        description={title}
        meta={
          <>
            <KindLabel kind={issue.kind} />
            <span>{issue.repo}</span>
            {issue.author && <span>{issue.author}</span>}
            <span title={absoluteTime(issue.updated_at) ?? undefined}>
              updated {relativeTime(issue.updated_at) ?? issue.updated_at}
            </span>
            {issue.pr_url && <PrLink url={issue.pr_url} />}
          </>
        }
        aside={<Spec items={specs} />}
      />

      <StaleCloseCallout issue={issue} events={detail.data.events} />

      <div className="grid grid-cols-[minmax(0,1fr)_20rem] max-wide:grid-cols-1">
        <div className="min-w-0">
          {detail.data.scenario ? (
            <ScenarioDetailSection scenario={detail.data.scenario} />
          ) : (
            <IssueBodySection body={detail.data.body} labels={issue.labels} />
          )}
          <IssueCommentsSection comments={detail.data.comments} />
          <Section>
            <SectionHeader title="Journey" />
            {journey.isError ? (
              <SectionBody>
                <Mono tone="ink-3">Could not load the journey: {formatError(journey.error)}</Mono>
              </SectionBody>
            ) : journey.isPending ? (
              <LoadingBlock label="LOADING JOURNEY" className="border-b-0" />
            ) : (
              <SectionBody>
                <JourneyTimeline steps={journey.data.steps} />
              </SectionBody>
            )}
          </Section>
        </div>

        <SideRail
          issue={issue}
          role={role}
          events={detail.data.events}
          journeySteps={journey.data?.steps}
        />
      </div>
    </>
  );
}

// --- the stale-closable call-out ------------------------------------------------------------

/// When the grounded ranker parked this issue as already implemented, the issue is actionable —
/// a human just has to go close it upstream. This surfaces the rationale, the evidence event the
/// park recorded it from, and a prominent link out; it does not offer to close the issue itself
/// (that stays a human, on GitHub).
function StaleCloseCallout({ issue, events }: { issue: IssueDto; events: EventDto[] }) {
  const evidence = staleCloseEvidence(issue.kind, issue.status, issue.stale_closable, events);
  if (!evidence) return null;
  const { evidenceEvent } = evidence;

  return (
    <Section>
      <SectionHeader
        title="Already implemented, closable upstream"
        className="text-amber"
        actions={
          issue.evidence_url && (
            <Button
              variant="filled"
              render={<a href={issue.evidence_url} target="_blank" rel="noopener noreferrer" />}
            >
              REVIEW &amp; CLOSE ↗
            </Button>
          )
        }
      />
      <SectionBody className="border-l-2 border-amber">
        <div className="max-w-[74ch] whitespace-pre-wrap text-ink-2">
          {evidenceEvent?.reason?.text ?? issue.parked_reason?.text}
        </div>
        {evidenceEvent && (
          <Mono size="label" tone="ink-3" className="mt-1.5 block">
            recorded {relativeTime(evidenceEvent.ts) ?? evidenceEvent.ts}
          </Mono>
        )}
      </SectionBody>
    </Section>
  );
}

// --- the timeline (the page's spine) ---------------------------------------------------------

function JourneyTimeline({ steps }: { steps: JourneyStep[] }) {
  const ghosts = ghostSteps(steps);

  return (
    <ol className="m-0 list-none p-0">
      {steps.map((step, i) => (
        <RealNode key={`${step.kind}-${i}`} step={step} />
      ))}
      {ghosts.map((kind) => (
        <TimelineNode key={`ghost-${kind}`} tone="muted" ghost title={ghostTitle(kind)} time="upcoming" />
      ))}
    </ol>
  );
}

function TimelineNode({
  tone,
  ghost = false,
  title,
  time,
  timeTitle,
  children,
}: {
  tone: Tone;
  ghost?: boolean;
  title: string;
  time: string | null;
  timeTitle?: string;
  children?: ReactNode;
}) {
  return (
    <li className="group grid grid-cols-[8px_minmax(0,1fr)] gap-3">
      <div className="relative flex justify-center">
        <span
          className={cn(
            'mt-1.5 size-2 shrink-0',
            ghost
              ? 'border border-dashed border-rule-hard'
              : cn('border border-black/30 dark:border-white/30', TONE_SWATCH[tone]),
          )}
        />
        <span className="absolute top-5 bottom-0 left-1/2 w-px -translate-x-1/2 bg-rule group-last:hidden" />
      </div>
      <div className={cn('min-w-0 pb-5', ghost && 'text-ink-3')}>
        <div className="flex flex-wrap items-baseline gap-3">
          <span className="font-semibold">{title}</span>
          {time !== null && (
            <Mono size="label" tone="ink-3" title={timeTitle}>
              {time}
            </Mono>
          )}
        </div>
        {children}
      </div>
    </li>
  );
}

function RealNode({ step }: { step: JourneyStep }) {
  const view = stepPresentation(step);

  return (
    <TimelineNode
      tone={view.tone}
      title={view.title}
      time={relativeTime(step.at)}
      timeTitle={absoluteTime(step.at) ?? undefined}
    >
      <StepDetail step={step} detail={view.detail} />
    </TimelineNode>
  );
}

/// Abbreviate a pinned `repo@sha256:<64hex>` to `repo@sha256:1234abcd…` for the chip; the full ref
/// is on the tooltip. A ref without a `@sha256:` digest (shouldn't happen on a succeeded build) is
/// returned as-is.
function shortDigest(ref: string): string {
  const at = ref.indexOf('@sha256:');
  if (at === -1) return ref;
  const repo = ref.slice(0, at);
  const hex = ref.slice(at + '@sha256:'.length);
  return `${repo}@sha256:${hex.slice(0, 8)}…`;
}

function DetailRow({ children }: { children: ReactNode }) {
  return <div className="mt-1 flex flex-wrap items-center gap-2 text-ink-2">{children}</div>;
}

function DetailLink({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      className="font-mono text-data text-blue underline-offset-2 hover:underline"
    >
      {children}
    </a>
  );
}

function Snippet({ children }: { children: ReactNode }) {
  return (
    <div className="mt-1 w-full border-l-2 border-amber bg-sunk px-2.5 py-1.5 font-mono text-data whitespace-pre-wrap text-ink-2">
      {children}
    </div>
  );
}

/// The per-kind detail line: the pure `detail` string plus the interactive chrome (tier chip,
/// evidence link, live pane, PR chip, terminal snippet) that only makes sense in the DOM.
function StepDetail({ step, detail }: { step: JourneyStep; detail: string }) {
  switch (step.kind) {
    case 'ranked':
      return (
        <DetailRow>
          {step.tier && (
            <Mono weight="semibold" tone={tierTone(step.tier)}>
              {step.tier}
            </Mono>
          )}
          <span>{detail}</span>
        </DetailRow>
      );
    case 'scoped':
      return (
        <DetailRow>
          <span>{detail}</span>
          <Link
            to="/approvals"
            className="font-mono text-data text-blue underline-offset-2 hover:underline"
          >
            evidence
          </Link>
        </DetailRow>
      );
    case 'build':
      return (
        <DetailRow>
          <span>{detail}</span>
          {step.digest && (
            <Tooltip content={step.digest}>
              <Mono className="border border-rule-hard px-1 py-px">{shortDigest(step.digest)}</Mono>
            </Tooltip>
          )}
          {step.evidence && <DetailLink href={step.evidence}>build log ↗</DetailLink>}
        </DetailRow>
      );
    case 'run':
      return <RunDetail step={step} />;
    case 'pr':
      return (
        <DetailRow>
          <Mono tone="ink-3">{step.repo}</Mono>
          <PrLink url={step.url} />
        </DetailRow>
      );
    case 'parked':
      return (
        <DetailRow>
          <span>{detail}</span>
          {step.reason && <Snippet>{step.reason}</Snippet>}
        </DetailRow>
      );
    case 'stale':
      return (
        <DetailRow>
          <span>{detail}</span>
          {step.evidence && <Snippet>{step.evidence}</Snippet>}
        </DetailRow>
      );
    default:
      return (
        <DetailRow>
          <span>{detail}</span>
        </DetailRow>
      );
  }
}

/// A run node. When live, offers a prominent "watch live" link to the run page plus a collapsed
/// inline live pane (the compact `LiveSession`) so the page stays calm until the operator expands it.
function RunDetail({ step }: { step: Extract<JourneyStep, { kind: 'run' }> }) {
  const [open, setOpen] = useState(false);
  const isRunning = step.status === 'running';
  const hasScore = step.best_score !== null && step.best_score !== undefined;

  return (
    <>
      <DetailRow>
        <Status
          status={step.status}
          tone={statusTone(step.live ? 'blue' : runStatusColor(step.status))}
          pulse={step.live && isRunning}
        />
        {hasScore && <Mono>best {step.best_score?.toFixed(1)}</Mono>}
        <Identifier variant="inline" to={`/runs/${encodeURIComponent(step.run_id)}`}>
          {step.run_id}
        </Identifier>
        {step.live && isRunning && (
          <Button
            onClick={() => {
              setOpen((v) => !v);
            }}
          >
            {open ? 'HIDE LIVE' : 'LIVE STATUS'}
          </Button>
        )}
      </DetailRow>
      {step.live && isRunning && open && (
        <div className="mt-3 border border-rule-hard">
          <LiveSession runId={step.run_id} compact />
        </div>
      )}
    </>
  );
}

// --- the operational side rail ---------------------------------------------------------------

type IssueDto = components['schemas']['IssueDto'];
type Role = components['schemas']['Role'];
type EventDto = components['schemas']['EventDto'];

function SideRail({
  issue,
  role,
  events,
  journeySteps,
}: {
  issue: IssueDto;
  role: Role | undefined;
  events: EventDto[];
  journeySteps: JourneyStep[] | undefined;
}) {
  const rationale = rankingRationale(issue.kind, events);
  const isAdmin = role === 'admin';
  const isOperator = role === 'admin' || role === 'operator';
  const canScope = SCOPEABLE_STATUSES.has(issue.status);
  // ScopeNow and re-rank are GitHub-ranker affordances: a scenario already bypasses the ranker
  // and its rank-horizon/tier/cap gates at adoption, so neither control applies to it.
  const isGitHub = issue.kind.type === 'github';

  return (
    <aside className="border-l border-rule-hard max-wide:border-t max-wide:border-l-0">
      {isOperator && (
        <Section>
          <SectionHeader title="Controls" />
          <SectionBody className="grid gap-3">
            <PriorityControl issue={issue} />
            <ParkControls issue={issue} />
            {isAdmin && <RedispatchControl issue={issue} />}
            {isAdmin && issue.kind.type === 'scenario' && <ApproveScenarioControl issue={issue} />}
            {isAdmin && isGitHub && (
              <>
                {canScope ? (
                  <Button
                    variant="filled"
                    className="w-full"
                    render={<Link to={`/issues/${encodeURIComponent(issue.key)}/scope`} />}
                  >
                    SCOPE THIS ISSUE
                  </Button>
                ) : (
                  <Tooltip content={disabledReason(issue.status)}>
                    <Button
                      variant="filled"
                      className="w-full cursor-default bg-rule-hard"
                      aria-disabled
                    >
                      SCOPE THIS ISSUE
                    </Button>
                  </Tooltip>
                )}
                <RerankControl issue={issue} />
              </>
            )}
          </SectionBody>
        </Section>
      )}

      <Section>
        <SectionHeader title="Details" />
        <SectionBody>
          <dl className="m-0">
            <MetaRow label="Status">
              <Status status={issue.status} tone={statusTone(issueStatusColor(issue.status))} />
            </MetaRow>
            {issue.tier && (
              <MetaRow label="Tier">
                <Mono weight="semibold" tone={tierTone(issue.tier)}>
                  {issue.tier}
                </Mono>
              </MetaRow>
            )}
            <MetaRow label="Priority">
              <Mono tone={issue.priority === 0 ? 'ink-3' : 'ink'}>
                {issue.priority === 0 ? '—' : issue.priority}
              </Mono>
            </MetaRow>
            <MetaRow label="Repo">
              <Mono>{issue.repo}</Mono>
            </MetaRow>
            {issue.agent_provider && (
              <MetaRow label="Provider">
                <Mono>
                  <AgentProviderTag provider={issue.agent_provider} model={issue.agent_model} />
                </Mono>
              </MetaRow>
            )}
            {issue.author && (
              <MetaRow label="Author">
                <Mono>{issue.author}</Mono>
              </MetaRow>
            )}
            <MetaRow label="Updated">
              <Mono title={absoluteTime(issue.updated_at) ?? undefined}>
                {relativeTime(issue.updated_at) ?? issue.updated_at}
              </Mono>
            </MetaRow>
            {issue.evidence_url && (
              <MetaRow label="Upstream">
                <DetailLink href={issue.evidence_url}>view ↗</DetailLink>
              </MetaRow>
            )}
          </dl>
          {issue.labels.length > 0 && (
            <div className="mt-3 flex flex-wrap gap-1">
              {issue.labels.map((label) => (
                <Mono key={label} size="micro" className="border border-rule-hard px-1 py-px">
                  {label}
                </Mono>
              ))}
            </div>
          )}
        </SectionBody>
      </Section>

      <RankingSection kind={issue.kind} events={events} journeySteps={journeySteps} />

      {rationale && (
        <Section>
          <SectionHeader title="Ranking rationale" />
          <SectionBody className="whitespace-pre-wrap text-ink-2">{rationale}</SectionBody>
        </Section>
      )}
    </aside>
  );
}

/// Rank provenance: when the standing verdict landed (the newest rank event) and whether it is
/// text-tier or code-grounded (the journey's `grounded` step). Sits right above the rationale box,
/// giving the rail's Re-rank button its context. Per-issue rank cost is not carried here — ledger
/// `rank` rows are day-bucketed with no issue key, so there is nothing honest to show.
function RankingSection({
  kind,
  events,
  journeySteps,
}: {
  kind: InputKindDto;
  events: EventDto[];
  journeySteps: JourneyStep[] | undefined;
}) {
  const rankedAt = lastRankedAt(kind, events);
  const source = journeySteps ? verdictSource(journeySteps) : null;
  if (!rankedAt && !source) return null;

  return (
    <Section>
      <SectionHeader title="Ranking" />
      <SectionBody>
        <dl className="m-0">
          <MetaRow label="Last ranked">
            {rankedAt ? (
              <Mono title={absoluteTime(rankedAt) ?? undefined}>
                {relativeTime(rankedAt) ?? rankedAt}
              </Mono>
            ) : (
              <Mono tone="ink-3">unknown</Mono>
            )}
          </MetaRow>
          {source && (
            <MetaRow label="Verdict source">
              <Mono uppercase size="label" weight="semibold" tone={source === 'grounded' ? 'blue' : 'ink-2'}>
                {source === 'grounded' ? 'code-grounded' : 'text tier'}
              </Mono>
            </MetaRow>
          )}
        </dl>
      </SectionBody>
    </Section>
  );
}


function RailField({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <Mono size="label" tone="ink-3" uppercase className="mb-1 block">
        {label}
      </Mono>
      {children}
    </div>
  );
}

/// Operator-gated priority editor. Preserves the original bump flow: edit locally, Save commits and
/// invalidates the issue list + detail views.
function PriorityControl({ issue }: { issue: IssueDto }) {
  const queryClient = useQueryClient();
  const bumpMutation = $api.useMutation('post', '/api/issues/{key}/bump');
  const [editing, setEditing] = useState<number | null>(null);
  const value = editing ?? issue.priority;
  const clamp = (v: number) => Math.max(0, Math.min(99, v));

  return (
    <RailField label="Priority">
      <div className="flex items-center gap-2">
        <div className="flex items-stretch border border-rule-hard">
          <Button
            aria-label="Decrease priority"
            disabled={bumpMutation.isPending}
            onClick={() => {
              setEditing(clamp(value - 1));
            }}
          >
            −
          </Button>
          <input
            type="number"
            aria-label="Priority"
            min={0}
            max={99}
            value={value}
            disabled={bumpMutation.isPending}
            onChange={(event) => {
              const v = Number(event.target.value);
              if (!Number.isNaN(v)) setEditing(clamp(v));
            }}
            className="w-12 border-x border-rule bg-transparent px-1 text-center font-mono text-data text-ink"
          />
          <Button
            aria-label="Increase priority"
            disabled={bumpMutation.isPending}
            onClick={() => {
              setEditing(clamp(value + 1));
            }}
          >
            +
          </Button>
        </div>
        {editing !== null && editing !== issue.priority && (
          <Button
            variant="filled"
            disabled={bumpMutation.isPending}
            onClick={() => {
              bumpMutation.mutate(
                { params: { path: { key: issue.key } }, body: { priority: editing } },
                {
                  onSuccess: () => {
                    setEditing(null);
                    void queryClient.invalidateQueries({ queryKey: ['get', '/api/issues/{key}'] });
                    void queryClient.invalidateQueries({ queryKey: ['get', '/api/issues'] });
                  },
                },
              );
            }}
          >
            SAVE
          </Button>
        )}
      </div>
    </RailField>
  );
}

/// Admin-gated force re-rank: clears the cached ranking verdict so the next reconcile sweep
/// re-ranks this issue (nothing happens inline). The standing tier keeps gating until the fresh
/// verdict lands, so a confirm modal spells that out before the POST.
function RerankControl({ issue }: { issue: IssueDto }) {
  const refresh = useIssueRefresh();
  const rerankMutation = $api.useMutation('post', '/api/issues/{key}/rerank');
  const [modalOpen, setModalOpen] = useState(false);

  const handleConfirm = () => {
    rerankMutation.mutate(
      { params: { path: { key: issue.key } } },
      {
        onSuccess: () => {
          setModalOpen(false);
          refresh();
        },
      },
    );
  };

  return (
    <>
      <Button
        className="w-full border border-rule-hard"
        onClick={() => {
          setModalOpen(true);
        }}
      >
        RE-RANK
      </Button>
      <AlertDialog.Root
        open={modalOpen}
        onOpenChange={(open) => {
          if (!open) setModalOpen(false);
        }}
      >
        <AlertDialog.Portal>
          <AlertDialog.Backdrop className={DIALOG_BACKDROP} />
          <AlertDialog.Popup className={ALERT_POPUP}>
            <AlertDialog.Title className={DIALOG_TITLE}>
              Force re-rank?
            </AlertDialog.Title>
            <AlertDialog.Description className="m-0 px-4 py-3.5 text-ink-2">
              Clears the cached ranking verdict for{' '}
              <strong className="font-mono text-ink">{issue.key}</strong>. The ranker re-runs on the
              next reconcile sweep, not immediately; the current tier
              {issue.tier ? ` (${issue.tier})` : ''} keeps gating until the fresh verdict lands.
            </AlertDialog.Description>
            <div className="flex justify-end border-t border-rule">
              <Button
                onClick={() => {
                  setModalOpen(false);
                }}
              >
                CANCEL
              </Button>
              <Button variant="filled" disabled={rerankMutation.isPending} onClick={handleConfirm}>
                RE-RANK
              </Button>
            </div>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog.Root>
    </>
  );
}

/// The UI-native replacement for the GitHub draft-PR approval gate: a scenario has no upstream
/// item to open one against, so this posts straight to the approve endpoint. Only shown while a
/// scope is actually awaiting approval; refreshes the same queries the approval's PR-merge poll would.
function ApproveScenarioControl({ issue }: { issue: IssueDto }) {
  const refresh = useIssueRefresh();
  const approveMutation = $api.useMutation('post', '/api/scenarios/{key}/approve');

  if (issue.status !== 'awaiting-approval') return null;

  return (
    <div>
      <Button
        variant="filled"
        className="w-full"
        disabled={approveMutation.isPending}
        onClick={() => {
          approveMutation.mutate(
            { params: { path: { key: issue.key } } },
            { onSuccess: refresh },
          );
        }}
      >
        APPROVE PACK
      </Button>
      {approveMutation.isError && (
        <Mono size="label" tone="red" className="mt-1 block">
          {formatError(approveMutation.error)}
        </Mono>
      )}
    </div>
  );
}

/** A callback that invalidates every `/api/issues*` query so the issue list + detail + journey
 * refetch after a mutation lands (park, unpark, re-run). */
function useIssueRefresh() {
  const queryClient = useQueryClient();
  return () => {
    void queryClient.invalidateQueries({
      predicate: (query) => {
        const key = query.queryKey;
        return Array.isArray(key) && key[0] === 'get' && typeof key[1] === 'string' && key[1].startsWith('/api/issues');
      },
    });
  };
}

/// Operator-gated park / unpark. Unpark for a parked issue; park (with a required reason) for an
/// otherwise-active one. Both refresh the issue detail + journey so the terminal node lights up.
function ParkControls({ issue }: { issue: IssueDto }) {
  const parkMutation = $api.useMutation('post', '/api/issues/{key}/park');
  const unparkMutation = $api.useMutation('post', '/api/issues/{key}/unpark');
  const [reason, setReason] = useState('');
  const refresh = useIssueRefresh();

  if (issue.status === 'parked') {
    return (
      <Button
        className="w-full border border-rule-hard"
        disabled={unparkMutation.isPending}
        onClick={() => {
          unparkMutation.mutate(
            { params: { path: { key: issue.key } }, body: { reason: null } },
            { onSuccess: refresh },
          );
        }}
      >
        UNPARK
      </Button>
    );
  }

  if (issue.status === 'done') return null;

  return (
    <RailField label="Park">
      <div className="flex gap-2">
        <input
          aria-label="Park reason"
          placeholder="reason"
          value={reason}
          disabled={parkMutation.isPending}
          onChange={(event) => {
            setReason(event.target.value);
          }}
          className={cn(INPUT, 'min-w-0 flex-1')}
        />
        <Button
          className="border border-rule-hard"
          disabled={reason.trim() === '' || parkMutation.isPending}
          onClick={() => {
            parkMutation.mutate(
              { params: { path: { key: issue.key } }, body: { reason: reason.trim() } },
              {
                onSuccess: () => {
                  setReason('');
                  refresh();
                },
              },
            );
          }}
        >
          PARK
        </Button>
      </div>
    </RailField>
  );
}

/// Admin-gated re-run: re-admits a finished issue's approved pack to the loop (a fresh run id),
/// the natural way to reproduce a scenario and inspect its new trace. Only offered for `done` /
/// `pr-open` — the exact states the backend's redispatch guard accepts, so the button never no-ops
/// on a wrong-state issue. Kind-agnostic (works for scenarios and GitHub issues alike).
function RedispatchControl({ issue }: { issue: IssueDto }) {
  const redispatchMutation = $api.useMutation('post', '/api/issues/{key}/redispatch');
  const [justification, setJustification] = useState('');
  const refresh = useIssueRefresh();

  if (issue.status !== 'done' && issue.status !== 'pr-open') return null;

  return (
    <RailField label="Re-run">
      <div className="flex gap-2">
        <input
          aria-label="Re-run justification"
          placeholder="justification"
          value={justification}
          disabled={redispatchMutation.isPending}
          onChange={(event) => {
            setJustification(event.target.value);
          }}
          className={cn(INPUT, 'min-w-0 flex-1')}
        />
        <Button
          className="border border-rule-hard"
          disabled={justification.trim() === '' || redispatchMutation.isPending}
          onClick={() => {
            redispatchMutation.mutate(
              {
                params: { path: { key: issue.key } },
                body: { justification: justification.trim() },
              },
              {
                onSuccess: () => {
                  setJustification('');
                  refresh();
                },
              },
            );
          }}
        >
          RE-RUN
        </Button>
      </div>
    </RailField>
  );
}
