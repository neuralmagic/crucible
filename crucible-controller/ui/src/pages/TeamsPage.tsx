import { useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import {
  Button,
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
  useDataTable,
} from '../ui';
import { FormActions, FormError, FormGrid, TextField } from './formControls';
import { sortedTeams, validSlug, type TeamDto } from './teamsView';

const NO_TEAMS: TeamDto[] = [];
const helper = createDataColumnHelper<TeamDto>();

const columns = helper.columns([
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
      <Button render={<Link to={`/teams/${encodeURIComponent(row.original.slug)}`} />}>OPEN</Button>
    ),
  }),
]);

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
          variant="filled"
          disabled={!validSlug(slug.trim()) || displayName.trim().length === 0 || create.isPending}
          onClick={() => {
            void submit();
          }}
        >
          {create.isPending ? 'CREATING…' : 'CREATE TEAM'}
        </Button>
      </FormActions>
    </Section>
  );
}

/// Every team the caller may read, the ones they are in first.
export function TeamsPage() {
  const teams = $api.useQuery('get', '/api/teams');
  const rows = sortedTeams(teams.data ?? NO_TEAMS);
  const table = useDataTable({ columns, data: rows, getRowId: (row) => row.slug });

  return (
    <>
      <PageHeader eyebrow="System" title="Teams" />
      <QueryState query={teams} noun="TEAMS">
        <DataTable table={table} empty={<Empty title="NO TEAMS" />} footer={<>Showing {rows.length}</>} />
      </QueryState>
      <CreateTeam />
    </>
  );
}
