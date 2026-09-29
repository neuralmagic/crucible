import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { $api } from '../api/client';
import type { components } from '../api/schema';
import { useLiveEvents } from '../api/useLiveEvents';
import { narrow } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import {
  Button,
  cn,
  Empty,
  Identifier,
  LoadingBlock,
  Mono,
  PageHeader,
  QueryState,
  Section,
  SectionBody,
  SectionHeader,
  Status,
  statusTone,
} from '../ui';
import { issueStatusColor } from './issueStatus';
import { relativeTime } from './journeyView';
import { launchPath } from './launchView';
import { isDraftRun } from './playbookRunsView';
import { formatCost } from './runReport';
import {
  approvalsWaiting,
  attentionItems,
  budgetAttention,
  playbooksByUse,
  untilTime,
  upcoming,
  type AttentionItem,
} from './home';

type PlaybookDto = components['schemas']['PlaybookDto'];
type PlaybookRunDto = components['schemas']['PlaybookRunDto'];
type ScheduleDto = components['schemas']['ScheduleDto'];

const RECENT_RUNS = 6;
const PLAYBOOKS_SHOWN = 6;
const UPCOMING_SHOWN = 3;

const EMPTY_PLAYBOOKS: PlaybookDto[] = [];
const EMPTY_RUNS: PlaybookRunDto[] = [];
const EMPTY_SCHEDULES: ScheduleDto[] = [];

function SeeAll({ to, children }: { to: string; children: ReactNode }) {
  return (
    <Link to={to} className="font-mono text-label font-normal text-ink-3 hover:text-ink">
      {children} →
    </Link>
  );
}

function Rows({ children }: { children: ReactNode }) {
  return <ul className="m-0 list-none p-0">{children}</ul>;
}

const ROW = 'flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-rule py-2 last:border-b-0';

function NeedsYou({ items }: { items: AttentionItem[] }) {
  return (
    <Section>
      <SectionHeader title="Needs you" />
      <SectionBody className="py-0">
        <Rows>
          {items.map((item) => (
            <li key={item.key} className={ROW}>
              <Link
                to={item.to}
                className={cn(
                  'border-l-2 pl-2.5 text-ink hover:underline',
                  item.tone === 'red' ? 'border-l-red' : 'border-l-amber',
                )}
              >
                {item.text}
              </Link>
            </li>
          ))}
        </Rows>
      </SectionBody>
    </Section>
  );
}

function RecentRuns({ runs }: { runs: PlaybookRunDto[] }) {
  if (runs.length === 0) {
    return <p className="m-0 py-3 text-ink-3">No runs yet.</p>;
  }
  return (
    <Rows>
      {runs.map((run) => (
        <li key={run.key} className={ROW}>
          <Status status={run.status} tone={statusTone(issueStatusColor(run.status))} className="w-24" />
          <Link to={launchPath(run.key)} className="font-mono text-data font-semibold text-ink hover:underline">
            {run.playbook}
          </Link>
          <span className="flex-1" />
          <Mono tone="ink-3">{formatCost(run.cost_usd)}</Mono>
          <Mono tone="ink-3" className="w-16 text-right">
            {relativeTime(run.created_at) ?? '—'}
          </Mono>
        </li>
      ))}
    </Rows>
  );
}

function Playbooks({ playbooks }: { playbooks: PlaybookDto[] }) {
  if (playbooks.length === 0) {
    return <p className="m-0 py-3 text-ink-3">No playbooks.</p>;
  }
  return (
    <Rows>
      {playbooks.map((book) => {
        const path = `/playbooks/${encodeURIComponent(book.id)}`;
        return (
          <li key={book.id} className={cn(ROW, 'flex-nowrap')}>
            <div className="min-w-0 flex-1">
              <Identifier variant="inline" to={path}>
                {book.id}
              </Identifier>
              <p className="m-0 mt-1 truncate text-ink-2">{book.description}</p>
            </div>
            {book.actions.includes('launch') ? (
              <Button render={<Link to={`${path}/launch`} />}>LAUNCH</Button>
            ) : null}
          </li>
        );
      })}
    </Rows>
  );
}

function ComingUp({ schedules }: { schedules: ScheduleDto[] }) {
  const now = Date.now();
  return (
    <Section>
      <SectionHeader title="Coming up" actions={<SeeAll to="/schedules">All schedules</SeeAll>} />
      <SectionBody className="py-0">
        <Rows>
          {schedules.map((row) => (
            <li key={row.id} className={ROW}>
              <Mono weight="semibold">{row.playbook}</Mono>
              <Mono tone="ink-3">{row.cron_expr}</Mono>
              <span className="flex-1" />
              <Mono tone="ink-2" title={row.next_due_at ?? undefined}>
                {untilTime(row.next_due_at, now)}
              </Mono>
            </li>
          ))}
        </Rows>
      </SectionBody>
    </Section>
  );
}

function FirstRun() {
  return (
    <Empty
      title="NOTHING HERE YET"
      action={
        <>
          <Button variant="filled" render={<Link to="/playbooks/import" />}>
            IMPORT FROM GIT
          </Button>
          <Button render={<Link to="/playbooks/drafts" />}>START A DRAFT</Button>
        </>
      }
    />
  );
}

/// Where a returning user lands: what waits on them, what ran lately, and what to launch next.
/// Sections with nothing to say stay hidden.
export function HomePage() {
  useLiveEvents();

  const playbooks = $api.useQuery('get', '/api/playbooks');
  const runs = $api.useQuery('get', '/api/playbook-runs');
  const schedules = $api.useQuery('get', '/api/schedules');
  const approvals = $api.useQuery('get', '/api/approvals');
  const overview = $api.useQuery('get', '/api/overview');
  const owner = useOwnerContext();

  const allRuns = runs.data ?? EMPTY_RUNS;
  const realRuns = allRuns.filter((run) => !isDraftRun(run));
  const books = narrow(playbooks.data ?? EMPTY_PLAYBOOKS, owner.context, (row) => row.owner);
  const scheduleRows = narrow(schedules.data ?? EMPTY_SCHEDULES, owner.context, (row) => row.owner_principal);
  const budget = overview.data
    ? budgetAttention({ spent: overview.data.cost_today.current, ceiling: overview.data.cost_today.ceiling }, new Date())
    : null;
  const attention = [
    ...(budget ? [budget] : []),
    ...attentionItems(realRuns, scheduleRows, approvals.data ? approvalsWaiting(approvals.data) : 0),
  ];
  const next = upcoming(scheduleRows, UPCOMING_SHOWN);

  const header = (
    <PageHeader
      title="Home"
    />
  );

  if (playbooks.isPending || runs.isPending) {
    return (
      <>
        {header}
        <LoadingBlock label="LOADING" />
      </>
    );
  }

  if (playbooks.isSuccess && runs.isSuccess && playbooks.data.length === 0 && allRuns.length === 0) {
    return (
      <>
        {header}
        <FirstRun />
      </>
    );
  }

  return (
    <>
      {header}
      {attention.length > 0 ? <NeedsYou items={attention} /> : null}
      <div className="grid grid-cols-1 wide:grid-cols-2">
        <Section>
          <SectionHeader title="Recent runs" actions={<SeeAll to="/playbook-runs">All runs</SeeAll>} />
          <SectionBody className="py-0">
            <QueryState query={runs} noun="RUNS">
              <RecentRuns runs={realRuns.slice(0, RECENT_RUNS)} />
            </QueryState>
          </SectionBody>
        </Section>
        <Section className="wide:border-l wide:border-l-rule-hard">
          <SectionHeader title="Playbooks" actions={<SeeAll to="/playbooks">All playbooks</SeeAll>} />
          <SectionBody className="py-0">
            <QueryState query={playbooks} noun="PLAYBOOKS">
              <Playbooks playbooks={playbooksByUse(books, allRuns, PLAYBOOKS_SHOWN)} />
            </QueryState>
          </SectionBody>
        </Section>
      </div>
      {next.length > 0 ? <ComingUp schedules={next} /> : null}
    </>
  );
}
