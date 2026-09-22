import { useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { narrow, withOwner } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import { OwnerField } from './OwnerField';
import type { components } from '../api/schema';
import {
  Breadcrumb,
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
import { CoDraftHint } from './CoDraftHint';
import { DeleteDraftDialog } from './DeleteDraftDialog';
import { FormActions, FormError, FormGrid, SelectField, TextField } from './formControls';
import { originLabel } from './draftStudio';

type DraftDto = components['schemas']['PlaybookDraftDto'];
type PlaybookDto = components['schemas']['PlaybookDto'];

/// Where a new draft's first version comes from. `git` fetches a pack and opens it as a draft in
/// one motion, which is the import path with its own front door.
type DraftSource = 'skeleton' | 'template' | 'git';

function asSource(value: string): DraftSource {
  return value === 'template' || value === 'git' ? value : 'skeleton';
}

const NO_DRAFTS: DraftDto[] = [];
const NO_PLAYBOOKS: PlaybookDto[] = [];
const CRUMBS = [{ label: 'Playbooks', to: '/playbooks' }, { label: 'Drafts' }];

const helper = createDataColumnHelper<DraftDto>();

function makeColumns(onDelete: (id: string) => void) {
  return helper.columns([
    helper.accessor('id', {
      header: 'Draft',
      enableSorting: false,
      meta: { pad: 'tight', shrink: true },
      cell: ({ getValue }) => (
        <Identifier to={`/playbooks/drafts/${encodeURIComponent(getValue())}`}>{getValue()}</Identifier>
      ),
    }),
    helper.accessor('description', { header: 'Description', enableSorting: false }),
    helper.accessor('owner', {
      header: 'Owner',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    }),
    helper.display({
      id: 'state',
      header: 'State',
      meta: { className: 'font-mono text-data text-ink-2' },
      cell: ({ row }) =>
        row.original.retired_at !== null
          ? 'retired'
          : row.original.compiles
            ? `v${row.original.latest_version}`
            : `v${row.original.latest_version} · ${row.original.diagnostics} diagnostic${
                row.original.diagnostics === 1 ? '' : 's'
              }`,
    }),
    helper.display({
      id: 'origin',
      header: 'From',
      meta: { className: 'font-mono text-data text-ink-3' },
      cell: ({ row }) => originLabel(row.original.origin),
    }),
    helper.display({
      id: 'graduation',
      header: 'Graduation',
      meta: { className: 'font-mono text-data text-ink-3' },
      cell: ({ row }) =>
        row.original.graduation_pr_url === null ? (
          '—'
        ) : (
          <a href={row.original.graduation_pr_url} target="_blank" rel="noreferrer">
            {row.original.graduation_repo}
          </a>
        ),
    }),
    helper.display({
      id: 'open',
      header: '',
      meta: { align: 'end' },
      cell: ({ row }) => (
        <span className="flex justify-end gap-1">
          <Button render={<Link to={`/playbooks/drafts/${encodeURIComponent(row.original.id)}`} />}>
            OPEN
          </Button>
          <Button
            disabled={!row.original.actions.includes('delete')}
            onClick={() => {
              onDelete(row.original.id);
            }}
          >
            DELETE
          </Button>
        </span>
      ),
    }),
  ]);
}

/// Drafts under authoring: a rail of its own, never mixed into the registry. A draft is not pinned
/// to anything, so nothing here can be launched from a commit — it is launched from a save.
export function PlaybookDraftsPage() {
  const navigate = useNavigate();
  const whoami = $api.useQuery('get', '/api/whoami');
  const drafts = $api.useQuery('get', '/api/playbook-drafts');
  const playbooks = $api.useQuery('get', '/api/playbooks');
  const create = $api.useMutation('post', '/api/playbook-drafts');
  const fromGit = $api.useMutation('post', '/api/playbook-drafts/from-git');

  const drop = $api.useMutation('delete', '/api/playbook-drafts/{id}');

  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [id, setId] = useState('');
  const [description, setDescription] = useState('');
  const [source, setSource] = useState<DraftSource>('skeleton');
  const [template, setTemplate] = useState('');
  const [repo, setRepo] = useState('');
  const [gitRef, setGitRef] = useState('');
  const [path, setPath] = useState('');
  const [ownerChoice, setOwnerChoice] = useState('');
  const [error, setError] = useState<string | null>(null);

  const ownerContext = useOwnerContext();
  const owner = withOwner(ownerChoice, ownerContext.context, ownerContext.owners);
  const rows = narrow(drafts.data ?? NO_DRAFTS, ownerContext.context, (row) => row.owner);
  const signedIn = typeof whoami.data?.user === 'string';
  const columns = useMemo(
    () =>
      makeColumns((draftId) => {
        setDeleteError(null);
        setPendingDelete(draftId);
      }),
    []
  );
  const table = useDataTable({ columns, data: rows, getRowId: (row) => row.id });
  const pending = create.isPending || fromGit.isPending;

  const handleDelete = async () => {
    if (pendingDelete === null) return;
    setDeleteError(null);
    try {
      await drop.mutateAsync({ params: { path: { id: pendingDelete } } });
      setPendingDelete(null);
      void drafts.refetch();
    } catch (err: unknown) {
      setDeleteError(formatError(err));
    }
  };

  const handleCreate = async () => {
    setError(null);
    try {
      if (source === 'git') {
        await fromGit.mutateAsync({
          body: {
            id: id.trim(),
            description: description.trim(),
            repo: repo.trim(),
            git_ref: gitRef.trim().length === 0 ? undefined : gitRef.trim(),
            path: path.trim().length === 0 ? undefined : path.trim(),
            owner,
          },
        });
      } else {
        await create.mutateAsync({
          body: {
            id: id.trim(),
            description: description.trim(),
            template: source === 'template' && template.length > 0 ? template : undefined,
            owner,
          },
        });
      }
      void navigate(`/playbooks/drafts/${encodeURIComponent(id.trim())}`);
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  return (
    <>
      <Breadcrumb items={CRUMBS} />
      <PageHeader
        eyebrow="Queue"
        title="Draft packs"
        description="Packs authored here instead of imported. Every save compiles against the pinned engine; graduation exports one as a pull request."
      />

      <DeleteDraftDialog
        id={pendingDelete ?? ''}
        open={pendingDelete !== null}
        pending={drop.isPending}
        error={deleteError}
        onOpenChange={(open) => {
          if (!open) setPendingDelete(null);
        }}
        onConfirm={() => {
          void handleDelete();
        }}
      />

      <QueryState query={drafts} noun="DRAFTS">
        <DataTable
          table={table}
          empty={<Empty title="NO DRAFTS" description="Start one below." />}
          footer={<>Showing {rows.length}</>}
        />
      </QueryState>

      <CoDraftHint />

      {signedIn ? (
        <Section>
          <SectionHeader title="New draft" />
          <SectionBody>
            <FormGrid>
              <TextField
                id="draft-id"
                label="Draft id"
                mono
                required
                value={id}
                onChange={setId}
                hint="Lowercase slug. It shares the launch-key namespace with the registry, so it may not be a registered id."
              />
              <TextField
                id="draft-description"
                label="Description"
                required
                value={description}
                onChange={setDescription}
                hint="What this pack is for."
              />
              <OwnerField
                id="draft-owner"
                value={owner}
                onChange={setOwnerChoice}
                options={ownerContext.owners}
              />
              <SelectField
                id="draft-source"
                label={<Mono size="label">Source</Mono>}
                value={source}
                onChange={(next) => {
                  setSource(asSource(next));
                }}
                options={[
                  { value: 'skeleton', label: 'Skeleton (a workflow that compiles)' },
                  { value: 'template', label: 'A registered pack' },
                  { value: 'git', label: 'A pack in a git repo' },
                ]}
                hint="Where version 1 comes from."
              />
              {source === 'template' && (
                <SelectField
                  id="draft-template"
                  label={<Mono size="label">Template</Mono>}
                  value={template}
                  onChange={setTemplate}
                  options={[
                    { value: '', label: 'Pick a pack' },
                    ...(playbooks.data ?? NO_PLAYBOOKS).map((playbook) => ({
                      value: playbook.id,
                      label: `${playbook.id} — ${playbook.description}`,
                    })),
                  ]}
                  hint="Seed version 1 from a registered pack's files."
                />
              )}
              {source === 'git' && (
                <>
                  <TextField
                    id="draft-repo"
                    label="Repo"
                    mono
                    required
                    value={repo}
                    onChange={setRepo}
                    hint="owner/repo or a clone URL. The fetch lands a pending import that records where the bytes came from."
                  />
                  <TextField
                    id="draft-git-ref"
                    label="Ref"
                    mono
                    value={gitRef}
                    onChange={setGitRef}
                    hint="Branch or tag. Blank is the repo's default branch."
                  />
                  <TextField
                    id="draft-path"
                    label="Path"
                    mono
                    value={path}
                    onChange={setPath}
                    hint="The pack directory inside that repo. Blank is the repo root."
                  />
                </>
              )}
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
              disabled={
                id.trim().length === 0 ||
                description.trim().length === 0 ||
                (source === 'template' && template.length === 0) ||
                (source === 'git' && repo.trim().length === 0) ||
                pending
              }
              onClick={() => {
                void handleCreate();
              }}
            >
              {pending ? 'CREATING…' : 'CREATE DRAFT'}
            </Button>
          </FormActions>
        </Section>
      ) : null}
    </>
  );
}
