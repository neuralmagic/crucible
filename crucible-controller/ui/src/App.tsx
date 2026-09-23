import { lazy, Suspense, useCallback, useEffect, type ReactNode } from 'react';
import { Route, Routes, Link, NavLink, Navigate } from 'react-router-dom';
import { $api } from './api/client';
import { useAutoresearch } from './api/lanes';
import { cn, Spinner, Status, Tooltip } from './ui';
import { useDeviceFlag } from './useDeviceFlag';
import { relativeTime } from './pages/journeyView';
import { DisplayPrefs } from './DisplayPrefs';
import { IdentityBadge } from './IdentityBadge';
import { OwnerSwitcher } from './OwnerSwitcher';
import { AutopilotBanner } from './AutopilotBanner';
import { DashboardPage } from './pages/DashboardPage';
import { IssuesPage } from './pages/IssuesPage';
import { IssueDetailPage } from './pages/IssueDetailPage';
import { ScopeFormPage } from './pages/ScopeFormPage';
import { ScopeProgressPage } from './pages/ScopeProgressPage';
import { NewScenarioPage } from './pages/NewScenarioPage';
import { NewJiraPage } from './pages/NewJiraPage';
import { InboxPage } from './pages/InboxPage';
import { ApprovalsPage } from './pages/ApprovalsPage';
import { ReposPage } from './pages/ReposPage';
import { RunsPage } from './pages/RunsPage';
import { RunDetailPage } from './pages/RunDetailPage';
import { BuildsPage } from './pages/BuildsPage';
import { PlaybooksPage } from './pages/PlaybooksPage';
import { PlaybookDetailPage } from './pages/PlaybookDetailPage';
import { PlaybookImportPage } from './pages/PlaybookImportPage';
import { PlaybookImportReviewPage } from './pages/PlaybookImportReviewPage';
import { PlaybookDraftsPage } from './pages/PlaybookDraftsPage';
import { DraftStudioPage } from './pages/DraftStudioPage';
import { PlaybookLaunchPage } from './pages/PlaybookLaunchPage';
import { PlaybookRunsPage } from './pages/PlaybookRunsPage';
import { SchedulesPage } from './pages/SchedulesPage';
import { MoltenLogo } from './MoltenLogo';
import { PlaybookLaunchDetailPage } from './pages/PlaybookLaunchDetailPage';
import { TurnsPage } from './pages/TurnsPage';
import { TurnLivePage } from './pages/TurnLivePage';
import { ActivityPage } from './pages/ActivityPage';
import { LivePage } from './pages/LivePage';
import { AdminPage } from './pages/AdminPage';
import { SecretsPage } from './pages/SecretsPage';
import { ProvidersPage } from './pages/ProvidersPage';
import { SettingsPage } from './pages/SettingsPage';
import { TeamsPage } from './pages/TeamsPage';
import { TeamDetailPage } from './pages/TeamDetailPage';

// Explore carries the DuckDB-WASM bundle (multi-MB of wasm), so it loads only when visited.
const ExplorePage = lazy(() => import('./pages/ExplorePage'));

const POLL = { refetchInterval: 30_000 };

interface StatusCount {
  status: string;
  count: number;
}

function countOf(statuses: readonly StatusCount[], status: string): number {
  return statuses.find((s) => s.status === status)?.count ?? 0;
}

function openIssueCount(statuses: readonly StatusCount[]): number {
  return statuses.reduce((total, s) => (s.status === 'done' ? total : total + s.count), 0);
}

function usd(amount: number): string {
  return `$${amount.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

const MAST_CELL = 'flex items-center gap-[7px] border-l border-rule px-3 font-mono text-data text-ink-2';

function AutopilotIndicator() {
  const autopilot = $api.useQuery('get', '/api/autopilot', {}, POLL);
  if (!autopilot.isSuccess) return null;

  return (
    <div className={MAST_CELL}>
      <span className="text-ink-3">AUTOPILOT</span>
      <Status status={autopilot.data.enabled ? 'on' : 'off'} tone={autopilot.data.enabled ? 'green' : 'amber'} />
    </div>
  );
}

interface StripItem {
  label: string;
  value: string;
  note?: { text: string; tone: 'green' | 'red' };
}

function DatasheetStrip() {
  const autoresearch = useAutoresearch() === true;
  const overview = $api.useQuery('get', '/api/overview', {}, POLL);
  const approvals = $api.useQuery('get', '/api/approvals', {}, POLL);
  const events = $api.useQuery('get', '/api/events', { params: { query: { limit: 1 } } }, POLL);

  const items: StripItem[] = [];

  if (overview.isSuccess) {
    const { statuses, running } = overview.data;
    if (autoresearch) items.push({ label: 'Open issues', value: String(openIssueCount(statuses)) });
    items.push({
      label: 'Runs active',
      value: running.cap === null || running.cap === undefined ? String(running.current) : `${running.current} / ${running.cap}`,
    });
  }

  if (autoresearch && approvals.isSuccess) {
    items.push({ label: 'Awaiting approval', value: String(approvals.data.awaiting_approval.length) });
  }

  if (overview.isSuccess) {
    const cost = overview.data.cost_today;
    items.push({ label: 'Spend today', value: usd(cost.current) });
    if (cost.ceiling !== null && cost.ceiling !== undefined) {
      const pct = cost.ceiling === 0 ? 100 : Math.round((cost.current / cost.ceiling) * 100);
      items.push({
        label: 'Cap',
        value: usd(cost.ceiling),
        note: { text: `${pct}%`, tone: pct < 100 ? 'green' : 'red' },
      });
    }
  }

  const lastEvent = events.isSuccess ? relativeTime(events.data[0]?.ts) : null;
  if (lastEvent) items.push({ label: 'Last event', value: lastEvent });

  if (items.length === 0) return null;

  return (
    <dl className="m-0 flex flex-none flex-wrap border-b border-rule-hard bg-sunk font-mono text-label">
      {items.map((item) => (
        <div key={item.label} className="flex items-baseline gap-2 border-r border-rule px-3.5 py-[5px]">
          <dt className="uppercase tracking-[0.08em] text-ink-3">{item.label}</dt>
          <dd className="m-0 text-data font-semibold text-ink">
            {item.value}
            {item.note ? (
              <span className={cn('ml-1.5', item.note.tone === 'green' ? 'text-green' : 'text-red')}>{item.note.text}</span>
            ) : null}
          </dd>
        </div>
      ))}
    </dl>
  );
}

interface RailItem {
  to: string;
  label: string;
  /// The two-letter mark the collapsed rail shows in place of the label.
  icon: string;
  count?: number;
  /// Shown only where the autoresearch lane runs.
  autoresearch?: boolean;
}

interface RailSection {
  heading: string;
  items: RailItem[];
}

function RailLink({ to, label, count }: Omit<RailItem, 'icon' | 'autoresearch'>) {
  return (
    <NavLink
      to={to}
      className={({ isActive }) =>
        cn(
          'flex items-center justify-between border-l-2 py-1 pr-3.5 pl-3 text-body hover:bg-hi hover:text-ink',
          isActive ? 'border-green bg-hi font-semibold text-ink' : 'border-transparent text-ink-2'
        )
      }
    >
      {({ isActive }) => (
        <>
          <span>{label}</span>
          {count === undefined ? null : (
            <span className={cn('font-mono text-data', isActive ? 'text-ink-2' : 'text-ink-3')}>{count}</span>
          )}
        </>
      )}
    </NavLink>
  );
}

function RailIcon({ to, label, icon, count }: Omit<RailItem, 'autoresearch'>) {
  return (
    <Tooltip side="right" delay={120} content={count === undefined ? label : `${label} · ${count}`}>
      <NavLink
        to={to}
        aria-label={label}
        className={({ isActive }) =>
          cn(
            'flex h-8 w-full items-center justify-center border-l-2 font-mono text-data hover:bg-hi hover:text-ink',
            isActive ? 'border-green bg-hi font-semibold text-ink' : 'border-transparent text-ink-2'
          )
        }
      >
        {icon}
      </NavLink>
    </Tooltip>
  );
}

const RAIL_KEY = 'crucible.rail.collapsed';

/// The rail's width is per device, and Ctrl/Cmd+B is the toggle wherever the focus is.
function useRailCollapsed(): [boolean, () => void] {
  const [collapsed, setCollapsed] = useDeviceFlag(RAIL_KEY, false);

  const toggle = useCallback(() => {
    setCollapsed(!collapsed);
  }, [collapsed, setCollapsed]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.altKey || event.shiftKey || !(event.ctrlKey || event.metaKey)) return;
      if (event.key !== 'b' && event.key !== 'B') return;
      event.preventDefault();
      toggle();
    };
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('keydown', onKey);
    };
  }, [toggle]);

  return [collapsed, toggle];
}

interface CategoryRailProps {
  collapsed: boolean;
  onToggle: () => void;
}

function CategoryRail({ collapsed, onToggle }: CategoryRailProps) {
  const autoresearch = useAutoresearch() === true;
  const overview = $api.useQuery('get', '/api/overview', {}, POLL);
  const approvals = $api.useQuery('get', '/api/approvals', {}, POLL);
  const repos = $api.useQuery('get', '/api/repos', {}, { enabled: autoresearch });
  const whoami = $api.useQuery('get', '/api/whoami');

  const statuses = overview.isSuccess ? overview.data.statuses : [];
  const teams = [...(whoami.data?.teams ?? [])].sort((a, b) => a.team.localeCompare(b.team));
  const allSections: RailSection[] = [
    {
      heading: 'Queue',
      items: [
        { to: '/issues', label: 'Issues', icon: 'IS', autoresearch: true, count: overview.isSuccess ? openIssueCount(statuses) : undefined },
        { to: '/inbox', label: 'Inbox', icon: 'IN', autoresearch: true, count: overview.isSuccess ? countOf(statuses, 'parked') : undefined },
        {
          to: '/approvals',
          label: 'Approvals',
          icon: 'AP',
          count: approvals.isSuccess ? approvals.data.awaiting_approval.length : undefined,
        },
        { to: '/playbooks', label: 'Playbooks', icon: 'PB' },
        { to: '/playbooks/drafts', label: 'Drafts', icon: 'DR' },
        { to: '/schedules', label: 'Schedules', icon: 'SC' },
      ],
    },
    {
      heading: 'Runs',
      items: [
        { to: '/runs', label: 'Autoresearch', icon: 'AR', autoresearch: true, count: overview.isSuccess ? overview.data.running.current : undefined },
        { to: '/playbook-runs', label: 'Playbooks', icon: 'PR' },
      ],
    },
    {
      heading: 'Execution',
      items: [
        { to: '/builds', label: 'Builds', icon: 'BD', autoresearch: true },
        { to: '/turns', label: 'Turns', icon: 'TN', autoresearch: true },
      ],
    },
    {
      heading: 'Records',
      items: [
        { to: '/activity', label: 'Activity', icon: 'AC' },
        { to: '/repos', label: 'Repos', icon: 'RP', autoresearch: true, count: repos.isSuccess ? repos.data.length : undefined },
        { to: '/explore', label: 'Explore', icon: 'EX', autoresearch: true },
      ],
    },
    {
      heading: 'Teams',
      items: [
        ...teams.map((membership) => ({
          to: `/teams/${encodeURIComponent(membership.team)}`,
          label: membership.team,
          icon: membership.team.slice(0, 2).toUpperCase(),
        })),
        { to: '/teams', label: 'All teams', icon: 'TM' },
      ],
    },
    {
      heading: 'System',
      items: [
        { to: '/secrets', label: 'Secrets', icon: 'SE' },
        { to: '/providers', label: 'Providers', icon: 'PV' },
        { to: '/settings', label: 'Settings', icon: 'ST' },
        { to: '/admin', label: 'Admin', icon: 'AD' },
      ],
    },
  ];
  const sections = allSections
    .map((section) => ({ ...section, items: section.items.filter((item) => autoresearch || !item.autoresearch) }))
    .filter((section) => section.items.length > 0);

  return (
    <nav
      aria-label="Sections"
      data-testid="category-rail"
      data-collapsed={collapsed ? 'true' : 'false'}
      className={cn(
        'flex-none overflow-y-auto border-r border-rule-hard bg-surface pb-6 max-[1100px]:hidden',
        collapsed ? 'w-12' : 'w-52'
      )}
    >
      <div className={cn('flex border-b border-rule py-1', collapsed ? 'justify-center' : 'justify-end pr-2')}>
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={!collapsed}
          aria-keyshortcuts="Control+B Meta+B"
          aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
          title={`${collapsed ? 'Expand' : 'Collapse'} sidebar (Ctrl+B)`}
          className="cursor-pointer border-0 bg-transparent px-1 font-mono text-data text-ink-3 hover:text-ink"
        >
          {collapsed ? '»' : '«'}
        </button>
      </div>
      {sections.map((section) => (
        <div key={section.heading} className={cn(collapsed && 'flex flex-col border-b border-rule py-1')}>
          {collapsed ? null : (
            <h3 className="m-0 px-3.5 pt-3.5 pb-[5px] font-mono text-label font-semibold uppercase tracking-[0.1em] text-ink-3">
              {section.heading}
            </h3>
          )}
          {section.items.map((item) =>
            collapsed ? <RailIcon key={item.to} {...item} /> : <RailLink key={item.to} {...item} />
          )}
        </div>
      ))}
    </nav>
  );
}

/// An autoresearch page where that lane runs; the playbooks otherwise.
function Lane({ page }: { page: ReactNode }) {
  const autoresearch = useAutoresearch();
  if (autoresearch === undefined) {
    return (
      <div className="p-4">
        <Spinner label="LOADING" />
      </div>
    );
  }
  return autoresearch ? page : <Navigate to="/playbooks" replace />;
}

export function App() {
  const [collapsed, toggleRail] = useRailCollapsed();
  const autoresearch = useAutoresearch() === true;

  return (
    <div className="flex h-screen min-h-0 flex-col">
      <header className="z-20 flex h-[42px] flex-none items-stretch border-b border-rule-hard bg-surface">
        <Link
          to="/"
          className={cn(
            'flex items-center gap-2.5 border-r border-rule px-3.5 hover:bg-hi',
            collapsed ? 'w-12 justify-center px-0' : 'min-w-52'
          )}
        >
          <MoltenLogo fallback={<img src="/favicon.svg" width={18} height={18} alt="" />} />
          {collapsed ? null : (
            <b className="font-mono text-[14px] font-bold tracking-[0.14em]">CRUCIBLE</b>
          )}
        </Link>
        <div className="flex-1" />
        <div className="flex items-stretch">
          {autoresearch ? <AutopilotIndicator /> : null}
          <OwnerSwitcher />
          <IdentityBadge />
          <DisplayPrefs />
        </div>
      </header>

      <DatasheetStrip />
      {autoresearch ? <AutopilotBanner /> : null}

      <div className="flex min-h-0 flex-1">
        <CategoryRail collapsed={collapsed} onToggle={toggleRail} />
        <main className="min-h-0 min-w-0 flex-1 overflow-y-auto">
          <Routes>
            <Route path="/" element={<Lane page={<DashboardPage />} />} />
            <Route path="/issues" element={<Lane page={<IssuesPage />} />} />
            <Route path="/scenarios/new" element={<Lane page={<NewScenarioPage />} />} />
            <Route path="/jira/new" element={<Lane page={<NewJiraPage />} />} />
            <Route path="/issues/:key" element={<Lane page={<IssueDetailPage />} />} />
            <Route path="/issues/:key/scope" element={<Lane page={<ScopeFormPage />} />} />
            <Route path="/issues/:key/scope/progress" element={<Lane page={<ScopeProgressPage />} />} />
            <Route path="/inbox" element={<Lane page={<InboxPage />} />} />
            <Route path="/approvals" element={<ApprovalsPage />} />
            <Route path="/repos" element={<Lane page={<ReposPage />} />} />
            <Route path="/runs" element={<Lane page={<RunsPage />} />} />
            <Route path="/runs/:runId" element={<RunDetailPage />} />
            <Route path="/runs/:runId/files/*" element={<RunDetailPage />} />
            <Route path="/builds" element={<Lane page={<BuildsPage />} />} />
            <Route path="/playbooks" element={<PlaybooksPage />} />
            <Route path="/playbooks/:id" element={<PlaybookDetailPage />} />
            <Route path="/playbooks/import" element={<PlaybookImportPage />} />
            <Route path="/playbooks/import/:id" element={<PlaybookImportReviewPage />} />
            <Route path="/playbooks/drafts" element={<PlaybookDraftsPage />} />
            <Route path="/playbooks/drafts/:id" element={<DraftStudioPage />} />
            <Route path="/playbooks/:id/launch" element={<PlaybookLaunchPage />} />
            <Route path="/playbook-runs" element={<PlaybookRunsPage />} />
            <Route path="/schedules" element={<SchedulesPage />} />
            <Route path="/playbook-runs/:key" element={<PlaybookLaunchDetailPage />} />
            <Route path="/playbook-runs/:key/runs/:runId" element={<RunDetailPage />} />
            <Route path="/playbook-runs/:key/runs/:runId/files/*" element={<RunDetailPage />} />
            <Route path="/turns" element={<Lane page={<TurnsPage />} />} />
            <Route path="/turns/:pod/live" element={<Lane page={<TurnLivePage />} />} />
            <Route path="/live" element={<Lane page={<LivePage />} />} />
            <Route path="/activity" element={<ActivityPage />} />
            <Route path="/secrets" element={<SecretsPage />} />
            <Route path="/providers" element={<ProvidersPage />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="/teams" element={<TeamsPage />} />
            <Route path="/teams/:slug" element={<TeamDetailPage />} />
            <Route path="/admin" element={<AdminPage />} />
            <Route
              path="/explore"
              element={
                <Lane
                  page={
                    <Suspense
                      fallback={
                        <div className="p-4">
                          <Spinner label="LOADING EXPLORE" />
                        </div>
                      }
                    >
                      <ExplorePage />
                    </Suspense>
                  }
                />
              }
            />
          </Routes>
        </main>
      </div>
    </div>
  );
}
