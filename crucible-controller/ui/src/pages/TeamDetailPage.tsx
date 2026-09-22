import { useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import {
  Breadcrumb,
  Button,
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
  useDataTable,
} from '../ui';
import { FormActions, FormError, FormGrid, SelectField, TextField } from './formControls';
import { optionValue } from './pickList';
import {
  asBodies,
  keepsAnOwner,
  MEMBER_KINDS,
  TEAM_ROLES,
  withMember,
  withoutMember,
  withRole,
  type MemberBody,
} from './membersView';
import { HELD, sortedMembers, type MemberDto } from './teamsView';
import { TeamResources } from './TeamResources';
import { useOwnerContext } from '../useOwnerContext';

const helper = createDataColumnHelper<MemberDto>();

const columns = helper.columns([
  helper.accessor('member', {
    header: 'Member',
    enableSorting: false,
    meta: { pad: 'tight' },
    cell: ({ row, getValue }) =>
      row.original.kind === 'team' ? (
        <Identifier to={`/teams/${encodeURIComponent(getValue())}`}>{getValue()}</Identifier>
      ) : (
        <Mono size="data">{getValue()}</Mono>
      ),
  }),
  helper.accessor('kind', {
    header: 'Held',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    cell: ({ getValue }) => HELD[getValue()],
  }),
  helper.accessor('role', {
    header: 'Role',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data' },
  }),
  helper.accessor('since', {
    header: 'Since',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-3' },
  }),
  helper.accessor('added_by', {
    header: 'Added by',
    enableSorting: false,
    meta: { shrink: true, className: 'font-mono text-data text-ink-3' },
    cell: ({ getValue }) => getValue() ?? '—',
  }),
]);

interface MembersProps {
  slug: string;
  members: readonly MemberDto[];
  /// Whether the caller owns the team: the API takes membership changes from owners alone.
  editable: boolean;
}

/// The member list, with a role select and a remove per row and an add form for an owner. Every
/// change sends the whole list, which is how the API takes it.
function Members({ slug, members, editable }: MembersProps) {
  const qc = useQueryClient();
  const put = $api.useMutation('put', '/api/teams/{slug}/members');
  const [added, setAdded] = useState<MemberBody>({ kind: 'user', member: '', role: 'member' });
  const [error, setError] = useState<string | null>(null);
  const current = asBodies(members);

  const send = async (next: MemberBody[]) => {
    setError(null);
    if (!keepsAnOwner(next)) {
      setError('a team keeps at least one user at owner');
      return;
    }
    try {
      await put.mutateAsync({ params: { path: { slug } }, body: { members: next } });
      await qc.invalidateQueries({ queryKey: ['get', '/api/teams/{slug}'] });
      await qc.invalidateQueries({ queryKey: ['get', '/api/whoami'] });
      setAdded({ kind: 'user', member: '', role: 'member' });
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const rows = sortedMembers(members);
  const table = useDataTable({
    columns: editable
      ? [
          ...columns,
          helper.display({
            id: 'edit',
            header: '',
            meta: { align: 'end' },
            cell: ({ row }) => (
              <span className="inline-flex items-center gap-2">
                <select
                  aria-label={`Role of ${row.original.member}`}
                  value={row.original.role}
                  disabled={put.isPending}
                  onChange={(event) => {
                    void send(withRole(current, row.original.kind, row.original.member, optionValue(TEAM_ROLES, event.target.value, 'member')));
                  }}
                  className="cursor-pointer border border-rule-hard bg-surface px-1.5 py-0.5 font-mono text-data"
                >
                  {TEAM_ROLES.map((role) => (
                    <option key={role.value} value={role.value}>
                      {role.label}
                    </option>
                  ))}
                </select>
                <Button
                  disabled={put.isPending}
                  onClick={() => {
                    void send(withoutMember(current, row.original.kind, row.original.member));
                  }}
                >
                  REMOVE
                </Button>
              </span>
            ),
          }),
        ]
      : columns,
    data: rows,
    getRowId: (row) => `${row.kind}:${row.member}`,
  });

  return (
    <>
      <div data-testid="members">
        <DataTable table={table} empty={<Empty title="NO MEMBERS" />} footer={<>Showing {rows.length}</>} />
      </div>
      {editable ? (
        <>
          <SectionBody>
            <FormGrid>
              <SelectField
                id="member-kind"
                label={<Mono size="label">Kind</Mono>}
                value={added.kind}
                onChange={(kind) => {
                  setAdded({ ...added, kind: optionValue(MEMBER_KINDS, kind, 'user') });
                }}
                options={MEMBER_KINDS}
              />
              <TextField
                id="member-name"
                label="Member"
                mono
                required
                value={added.member}
                onChange={(member) => {
                  setAdded({ ...added, member });
                }}
              />
              <SelectField
                id="member-role"
                label={<Mono size="label">Role</Mono>}
                value={added.role}
                onChange={(role) => {
                  setAdded({ ...added, role: optionValue(TEAM_ROLES, role, 'member') });
                }}
                options={TEAM_ROLES}
              />
            </FormGrid>
            {error === null ? null : <FormError className="mt-3">{error}</FormError>}
          </SectionBody>
          <FormActions>
            <Button
              variant="filled"
              disabled={added.member.trim().length === 0 || put.isPending}
              onClick={() => {
                void send(withMember(current, added));
              }}
            >
              {put.isPending ? 'SAVING…' : 'ADD MEMBER'}
            </Button>
          </FormActions>
        </>
      ) : error === null ? null : (
        <SectionBody>
          <FormError>{error}</FormError>
        </SectionBody>
      )}
    </>
  );
}

/// One team: who is in it, at what role, and how each membership is held.
export function TeamDetailPage() {
  const { slug = '' } = useParams();
  const team = $api.useQuery('get', '/api/teams/{slug}', { params: { path: { slug } } });
  const owner = useOwnerContext();
  const principal = `team:${slug}`;
  const acting = owner.context === principal;
  const crumbs = [{ label: 'Teams', to: '/teams' }, { label: slug }];

  if (team.isError) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty title="TEAM UNAVAILABLE" description={formatError(team.error)} />
      </>
    );
  }
  if (team.isPending) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <LoadingBlock label="LOADING TEAM" />
      </>
    );
  }
  const row = team.data;
  return (
    <>
      <Breadcrumb items={crumbs} />
      <DetailHeader
        title={row.slug}
        description={row.display_name}
        meta={
          <>
            <span>team:{row.slug}</span>
            <span>created {row.created_at}</span>
            {row.created_by ? <span>by {row.created_by}</span> : null}
          </>
        }
        badge={
          owner.switchable.some((p) => p.value === principal) ? (
            <Button
              variant={acting ? 'filled' : undefined}
              aria-pressed={acting}
              data-testid="act-as-team"
              onClick={() => {
                owner.setContext(acting ? 'all' : principal);
              }}
            >
              {acting ? 'ACTING AS' : 'ACT AS'}
            </Button>
          ) : null
        }
        aside={
          <Spec
            items={[
              { label: 'Your role', value: row.my_role ?? '—' },
              { label: 'Members', value: row.members.length },
              { label: 'Reachable', value: row.reachable ? 'yes' : 'no' },
            ]}
          />
        }
      />
      <Section>
        <SectionHeader
          title="Members"
          actions={
            <Link to="/teams" className="font-mono text-data text-ink-3 hover:text-ink">
              all teams
            </Link>
          }
        />
        <Members slug={row.slug} members={row.members} editable={row.my_role === 'owner'} />
      </Section>
      <TeamResources owner={principal} />
    </>
  );
}
