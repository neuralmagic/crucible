import { Link } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { narrow } from '../ownerContext';
import { Empty, Identifier, LoadingBlock, Mono, Section, SectionBody, SectionHeader } from '../ui';

interface Row {
  id: string;
  label: string;
  to: string;
  note: string;
}

function Rows({ title, rows, empty }: { title: string; rows: readonly Row[]; empty: string }) {
  return (
    <Section>
      <SectionHeader title={title} note={`${rows.length}`} />
      <SectionBody padded={rows.length === 0}>
        {rows.length === 0 ? (
          <Empty title={empty} />
        ) : (
          <ul className="m-0 list-none p-0">
            {rows.map((row) => (
              <li key={row.id} className="flex items-center gap-4 border-b border-rule px-3.5 py-1.5 last:border-b-0">
                <Identifier to={row.to}>{row.label}</Identifier>
                <Mono size="data" tone="ink-3">
                  {row.note}
                </Mono>
              </li>
            ))}
          </ul>
        )}
      </SectionBody>
    </Section>
  );
}

/// What one principal owns, across the lists the caller may read: the team as a project.
export function TeamResources({ owner }: { owner: string }) {
  const playbooks = $api.useQuery('get', '/api/playbooks');
  const drafts = $api.useQuery('get', '/api/playbook-drafts');
  const schedules = $api.useQuery('get', '/api/schedules');
  const secrets = $api.useQuery('get', '/api/secrets');
  const providers = $api.useQuery('get', '/api/providers');
  const lists = [playbooks, drafts, schedules, secrets];
  const failed = lists.find((q) => q.isError);
  if (failed !== undefined) {
    return <Empty title="RESOURCES UNAVAILABLE" description={formatError(failed.error)} />;
  }
  if (lists.some((q) => q.isPending)) {
    return <LoadingBlock label="LOADING RESOURCES" />;
  }
  return (
    <div data-testid="team-resources">
      <Rows
        title="Playbooks"
        empty="NO PLAYBOOKS"
        rows={narrow(playbooks.data ?? [], owner, (p) => p.owner).map((p) => ({
          id: p.id,
          label: p.id,
          to: `/playbooks/${encodeURIComponent(p.id)}`,
          note: p.description,
        }))}
      />
      <Rows
        title="Drafts"
        empty="NO DRAFTS"
        rows={narrow(drafts.data ?? [], owner, (d) => d.owner).map((d) => ({
          id: d.id,
          label: d.id,
          to: `/playbooks/drafts/${encodeURIComponent(d.id)}`,
          note: d.description,
        }))}
      />
      <Rows
        title="Schedules"
        empty="NO SCHEDULES"
        rows={narrow(schedules.data ?? [], owner, (s) => s.owner_principal).map((s) => ({
          id: s.id,
          label: s.playbook,
          to: '/schedules',
          note: s.enabled ? 'enabled' : 'disabled',
        }))}
      />
      <Rows
        title="Secrets"
        empty="NO SECRETS"
        rows={narrow(secrets.data ?? [], owner, (s) => s.owner).map((s) => ({
          id: s.id,
          label: s.name,
          to: '/secrets',
          note: s.kind,
        }))}
      />
      {providers.isSuccess ? (
        <Rows
          title="Providers"
          empty="NO PROVIDERS"
          rows={narrow(providers.data, owner, (p) => p.owner).map((p) => ({
            id: p.id,
            label: p.id,
            to: '/providers',
            note: p.display_name,
          }))}
        />
      ) : null}
      <div className="px-3.5 py-2 font-mono text-data text-ink-3">
        <Link to="/playbooks" className="hover:text-ink">
          every list follows the owner context
        </Link>
      </div>
    </div>
  );
}
