import { useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import {
  Button,
  cn,
  createDataColumnHelper,
  DataTable,
  Empty,
  Identifier,
  Mono,
  PageHeader,
  QueryState,
  Section,
  SectionBody,
  SectionHeader,
  Toolbar,
  ToolbarGroup,
  ToolbarSearch,
  useDataTable,
} from '../ui';
import { matchesTeam } from '../teamPicks';
import { useActiveTeams, useStarredTeams } from '../useTeamPicks';
import { FormActions, FormError, FormGrid, TextField } from './formControls';
import { sortedTeams, validSlug, type TeamDto } from './teamsView';

const NO_TEAMS: TeamDto[] = [];

type Show = 'mine' | 'all';
const SHOW: { value: Show; label: string }[] = [
  { value: 'mine', label: 'Mine' },
  { value: 'all', label: 'All' },
];
const helper = createDataColumnHelper<TeamDto>();

function makeColumns(starred: ReadonlySet<string>, active: ReadonlySet<string>, toggle: (slug: string) => void) {
  return helper.columns([
  helper.display({
    id: 'star',
    header: '',
    meta: { pad: 'tight', shrink: true },
    cell: ({ row }) => {
      const on = starred.has(row.original.slug);
      return (
        <button
          type="button"
          aria-pressed={on}
          aria-label={`${on ? 'Unstar' : 'Star'} ${row.original.slug}`}
          className={cn(
            'cursor-pointer border-0 bg-transparent px-2 py-1 font-mono text-data hover:text-ink',
            on ? 'text-amber' : 'text-ink-3',
          )}
          onClick={() => {
            toggle(row.original.slug);
          }}
        >
          {on ? '★' : '☆'}
        </button>
      );
    },
  }),
  helper.accessor('slug', {
    header: 'Team',
    enableSorting: false,
    meta: { pad: 'tight', shrink: true },
    cell: ({ getValue }) => <Identifier to={`/teams/${encodeURIComponent(getValue())}`}>{getValue()}</Identifier>,
  }),
  helper.accessor('display_name', { header: 'Name', enableSorting: false }),
  helper.accessor('my_role', {
    header: 'Your role',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data' },
    cell: ({ getValue }) => {
      const role = getValue();
      return (
        <Mono size="data" tone={role === null || role === undefined ? 'ink-3' : 'ink'}>
          {role ?? '—'}
        </Mono>
      );
    },
  }),
  helper.display({
    id: 'members',
    header: 'Members',
    meta: { shrink: true, align: 'end', className: 'font-mono text-data text-ink-2' },
    cell: ({ row }) => row.original.members.length,
  }),
  helper.display({
    id: 'active',
    header: 'Owns',
    meta: { shrink: true },
    cell: ({ row }) => (
      <Mono size="data" tone={active.has(row.original.slug) ? 'ink' : 'ink-3'}>
        {active.has(row.original.slug) ? 'yes' : 'no'}
      </Mono>
    ),
  }),
  helper.accessor('reachable', {
    header: 'Reachable',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => (
      <Mono size="data" tone={getValue() ? 'ink' : 'ink-3'}>
        {getValue() ? 'yes' : 'no'}
      </Mono>
    ),
  }),
  helper.display({
    id: 'open',
    header: '',
    meta: { align: 'end' },
    cell: ({ row }) => (
      <Button className="uppercase" render={<Link to={`/teams/${encodeURIComponent(row.original.slug)}`} />}>
        Open
      </Button>
    ),
  }),
]);
}

function CreateTeam() {
  const navigate = useNavigate();
  const create = $api.useMutation('post', '/api/teams');
  const [slug, setSlug] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    setError(null);
    try {
      const created = await create.mutateAsync({ body: { slug: slug.trim(), display_name: displayName.trim() } });
      void navigate(`/teams/${encodeURIComponent(created.slug)}`);
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  return (
    <Section>
      <SectionHeader title="New team" />
      <SectionBody>
        <FormGrid>
          <TextField id="team-slug" label="Slug" mono required value={slug} onChange={setSlug} />
          <TextField id="team-name" label="Name" required value={displayName} onChange={setDisplayName} />
        </FormGrid>
      </SectionBody>
      {error === null ? null : (
        <SectionBody>
          <FormError>{error}</FormError>
        </SectionBody>
      )}
      <FormActions>
        <Button
          className="uppercase"
          variant="filled"
          disabled={!validSlug(slug.trim()) || displayName.trim().length === 0 || create.isPending}
          onClick={() => {
            void submit();
          }}
        >
          {create.isPending ? 'Creating…' : 'Create team'}
        </Button>
      </FormActions>
    </Section>
  );
}

/// Every team the caller may read, the ones they are in first.
export function TeamsPage() {
  const teams = $api.useQuery('get', '/api/teams');
  const [starredList, toggle] = useStarredTeams();
  const active = useActiveTeams();
  const [query, setQuery] = useState('');
  const [show, setShow] = useState<Show>('mine');
  const starred = useMemo(() => new Set(starredList), [starredList]);
  const columns = useMemo(() => makeColumns(starred, active, toggle), [starred, active, toggle]);
  const rows = sortedTeams(teams.data ?? NO_TEAMS, starred, active).filter(
    (team) =>
      matchesTeam(query, team.slug, team.display_name) &&
      (show === 'all' || starred.has(team.slug) || (team.my_role !== null && team.my_role !== undefined)),
  );
  const table = useDataTable({ columns, data: rows, getRowId: (row) => row.slug });

  return (
    <>
      <PageHeader eyebrow="System" title="Teams" />
      <Toolbar>
        <ToolbarSearch value={query} onChange={setQuery} placeholder="Filter teams…" aria-label="Filter teams" />
        <ToolbarGroup label="Show" options={SHOW} value={show} onChange={setShow} />
      </Toolbar>
      <QueryState query={teams} noun="teams">
        <DataTable table={table} empty={<Empty title="No teams" />} footer={<>Showing {rows.length}</>} />
      </QueryState>
      <CreateTeam />
    </>
  );
}
