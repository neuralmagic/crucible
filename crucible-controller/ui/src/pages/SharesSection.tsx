import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  Empty,
  LoadingBlock,
  Mono,
  Section,
  SectionBody,
  SectionHeader,
  useDataTable,
} from '../ui';
import { FormActions, FormError, FormGrid, SelectField, TextField } from './formControls';
import { optionValue } from './pickList';
import {
  emptyShareForm,
  expiryLabel,
  notAfter,
  SHARE_ROLES,
  sortedShares,
  validGrantee,
  type ShareDto,
  type ShareForm,
} from './sharesView';

/// The resources that carry shares, by the path their detail lives under.
export type SharedResourcePath =
  | '/api/playbooks/{id}'
  | '/api/playbook-drafts/{id}'
  | '/api/playbooks/imports/{id}'
  | '/api/providers/{id}';

export interface SharesSectionProps {
  path: SharedResourcePath;
  id: string;
}

const helper = createDataColumnHelper<ShareDto>();

function columns(onRevoke: (grantee: string) => void, pending: boolean) {
  return helper.columns([
    helper.accessor('grantee', {
      header: 'Grantee',
      enableSorting: false,
      meta: { pad: 'tight' },
      cell: ({ getValue }) => <Mono size="data">{getValue()}</Mono>,
    }),
    helper.accessor('role', {
      header: 'Role',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data' },
    }),
    helper.display({
      id: 'until',
      header: 'Until',
      meta: { shrink: true, className: 'font-mono text-data' },
      cell: ({ row }) => (
        <Mono size="data" tone={row.original.expired ? 'ink-3' : 'ink-2'}>
          {expiryLabel(row.original)}
        </Mono>
      ),
    }),
    helper.accessor('created_by', {
      header: 'Shared by',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-3' },
      cell: ({ getValue }) => getValue() ?? '—',
    }),
    helper.display({
      id: 'revoke',
      header: '',
      meta: { align: 'end' },
      cell: ({ row }) => (
        <Button
          disabled={pending}
          onClick={() => {
            onRevoke(row.original.grantee);
          }}
        >
          REVOKE
        </Button>
      ),
    }),
  ]);
}

/// The shares on one resource, with a grant form and a revoke per row. Anyone who may read the
/// resource sees the list; a refused grant or revocation shows the decision verbatim.
export function SharesSection({ path, id }: SharesSectionProps) {
  const qc = useQueryClient();
  const sharesPath = `${path}/shares` as const;
  const sharePath = `${path}/shares/{grantee}` as const;
  const shares = $api.useQuery('get', sharesPath, { params: { path: { id } } });
  const grant = $api.useMutation('put', sharePath);
  const revoke = $api.useMutation('delete', sharePath);
  const [form, setForm] = useState<ShareForm>(emptyShareForm);
  const [error, setError] = useState<string | null>(null);

  const refresh = () => qc.invalidateQueries({ queryKey: ['get', sharesPath] });

  const submit = async () => {
    setError(null);
    try {
      await grant.mutateAsync({
        params: { path: { id, grantee: form.grantee.trim() } },
        body: { role: form.role, not_after: notAfter(form.until) },
      });
      setForm(emptyShareForm());
      await refresh();
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const onRevoke = async (grantee: string) => {
    setError(null);
    try {
      await revoke.mutateAsync({ params: { path: { id, grantee } } });
      await refresh();
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const rows = sortedShares(shares.data ?? []);
  const table = useDataTable({
    columns: columns((grantee) => { void onRevoke(grantee); }, revoke.isPending),
    data: rows,
    getRowId: (row) => row.grantee,
  });

  return (
    <Section>
      <SectionHeader title="Shares" note="who else may use this, at what role, until when" />
      {shares.isError ? (
        <SectionBody>
          <Empty title="SHARES UNAVAILABLE" description={formatError(shares.error)} />
        </SectionBody>
      ) : shares.isPending ? (
        <LoadingBlock label="LOADING SHARES" />
      ) : (
        <div data-testid="shares">
          <DataTable table={table} empty={<Empty title="NOT SHARED" />} footer={<>Showing {rows.length}</>} />
        </div>
      )}
      <SectionBody>
        <FormGrid>
          <TextField
            id="share-grantee"
            label="Grantee"
            mono
            required
            value={form.grantee}
            onChange={(grantee) => {
              setForm({ ...form, grantee });
            }}
            placeholder="user:<login> or team:<slug>"
          />
          <SelectField
            id="share-role"
            label={<Mono size="label">Role</Mono>}
            value={form.role}
            onChange={(role) => {
              setForm({ ...form, role: optionValue(SHARE_ROLES, role, 'viewer') });
            }}
            options={SHARE_ROLES}
          />
          <TextField
            id="share-until"
            label="Until"
            mono
            value={form.until}
            onChange={(until) => {
              setForm({ ...form, until });
            }}
            placeholder="YYYY-MM-DD"
          />
        </FormGrid>
        {error === null ? null : <FormError className="mt-3">{error}</FormError>}
      </SectionBody>
      <FormActions>
        <Button
          variant="filled"
          disabled={!validGrantee(form.grantee) || grant.isPending}
          onClick={() => {
            void submit();
          }}
        >
          {grant.isPending ? 'SHARING…' : 'SHARE'}
        </Button>
      </FormActions>
    </Section>
  );
}
