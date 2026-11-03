import { useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  Empty,
  Identifier,
  Mono,
  PageHeader,
  QueryState,
  useDataTable,
} from '../ui';
import { EMPTY_USE, needsVaultToken, secretsSummary, blueprintUseBody } from './blueprints';
import { UseBlueprintDialog } from './UseBlueprintDialog';

type BlueprintDto = components['schemas']['BlueprintDto'];
type Placeholder = components['schemas']['PlaceholderDto'];

const EMPTY: BlueprintDto[] = [];

const helper = createDataColumnHelper<BlueprintDto>();

/// The catalog: every blueprint the caller may read, and what using one would bind for them.
export function BlueprintsPage() {
  const navigate = useNavigate();
  const blueprints = $api.useQuery('get', '/api/blueprints');
  const using = $api.useMutation('post', '/api/blueprints/{id}/use');
  const [picked, setPicked] = useState<BlueprintDto | null>(null);
  const [form, setForm] = useState(EMPTY_USE);
  const [error, setError] = useState<string | null>(null);
  const [left, setLeft] = useState<readonly Placeholder[] | null>(null);

  const openDraft = (id: string) => {
    void navigate(`/playbooks/drafts/${encodeURIComponent(id)}`);
  };
  const close = () => {
    setPicked(null);
    setForm(EMPTY_USE);
    setError(null);
    setLeft(null);
  };
  const confirm = async () => {
    if (picked === null) return;
    setError(null);
    try {
      const used = await using.mutateAsync({
        params: { path: { id: picked.id } },
        body: blueprintUseBody(form, picked.rev),
      });
      if (used.placeholders.length === 0) {
        openDraft(used.id);
      } else {
        setLeft(used.placeholders);
      }
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const columns = useMemo(
    () =>
      helper.columns([
        helper.accessor('id', {
          header: 'Blueprint',
          enableSorting: false,
          meta: { pad: 'tight', shrink: true },
          cell: ({ getValue }) => (
            <Identifier to={`/playbooks/${encodeURIComponent(getValue())}`}>
              {getValue()}
            </Identifier>
          ),
        }),
        helper.accessor('description', {
          header: 'Description',
          enableSorting: false,
        }),
        helper.accessor('owner', {
          header: 'Owner',
          enableSorting: false,
          meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
        }),
        helper.accessor('public', {
          header: '',
          enableSorting: false,
          meta: { shrink: true },
          cell: ({ getValue }) =>
            getValue() ? (
              <Mono size="data" tone="ink-3">
                public
              </Mono>
            ) : null,
        }),
        helper.display({
          id: 'secrets',
          header: 'Secrets',
          meta: { className: 'font-mono text-data text-ink-2' },
          cell: ({ row }) => secretsSummary(row.original.secrets),
        }),
        helper.display({
          id: 'use',
          header: '',
          meta: { align: 'end' },
          cell: ({ row }) => (
            <Button
              className="uppercase"
              onClick={() => {
                setPicked(row.original);
              }}
            >
              Use
            </Button>
          ),
        }),
      ]),
    [],
  );
  const rows = blueprints.data ?? EMPTY;
  const table = useDataTable({
    columns,
    data: rows,
    getRowId: (row) => row.id,
  });

  return (
    <>
      <PageHeader eyebrow="Queue" title="Blueprints" />
      <QueryState query={blueprints} noun="blueprints">
        <DataTable
          table={table}
          empty={<Empty title="No blueprints" />}
          footer={<>Showing {rows.length}</>}
        />
      </QueryState>
      {picked === null ? null : (
        <UseBlueprintDialog
          id={picked.id}
          open
          form={form}
          askToken={needsVaultToken(picked.secrets)}
          pending={using.isPending}
          error={error}
          placeholders={left}
          onFormChange={setForm}
          onOpenChange={(open) => {
            if (!open) close();
          }}
          onConfirm={() => {
            void confirm();
          }}
          onOpenDraft={() => {
            openDraft(form.to.trim());
          }}
        />
      )}
    </>
  );
}
