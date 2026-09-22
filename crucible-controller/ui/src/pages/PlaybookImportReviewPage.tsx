import { useMemo, useState } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { SharesSection } from './SharesSection';
import type { components } from '../api/schema';
import {
  Breadcrumb,
  Button,
  Empty,
  LoadingBlock,
  Mono,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
} from '../ui';
import { FormActions, FormError, FormGrid, Note, Notice, TextField } from './formControls';
import { PlaybookPreviewGate } from './PlaybookPreviewGate';
import { initialValues, parseParamsSchema } from './playbookLaunchForm';
import {
  canRegister,
  compileBody,
  graphAwaitsValues,
  matchExistingPlaybook,
  previewOf,
} from './playbookImportWizard';
import { diffIsEmpty, diffParamsSchemas, type SchemaDiff } from './schemaDiff';

type PlaybookDto = components['schemas']['PlaybookDto'];

const NO_PLAYBOOKS: PlaybookDto[] = [];

const CRUMBS = [{ label: 'Playbooks', to: '/playbooks' }, { label: 'Import' }];

function SchemaDiffView({ diff }: { diff: SchemaDiff }) {
  if (!diff.comparable) {
    return (
      <Note>
        One of the two schemas is outside the subset this form renders, so the change cannot be
        itemized. Read the form above against the launch form the registry serves today.
      </Note>
    );
  }
  if (diffIsEmpty(diff)) return <Note>The launch form is unchanged by this bump.</Note>;
  return (
    <div className="grid gap-2 font-mono text-data">
      {diff.added.map((spec) => (
        <div key={`added-${spec.name}`} className="text-green">
          + {spec.name}
          {spec.required ? ' (required)' : ''}
        </div>
      ))}
      {diff.removed.map((spec) => (
        <div key={`removed-${spec.name}`} className="text-red">
          - {spec.name}
        </div>
      ))}
      {diff.changed.map((change) => (
        <div key={`${change.name}-${change.field}`} className="text-ink-2">
          ~ {change.name}.{change.field}: {change.from ?? 'none'} → {change.to ?? 'none'}
        </div>
      ))}
    </div>
  );
}

/// The preview gate for one proposed import. Everything it draws comes off the stored row, so a
/// shared link and a refresh land on the same pack, pinned at the same commit, however far the
/// branch has moved since. Registering is the admin's; proposing, discarding and forking the
/// bytes into a draft are the operator's.
export function PlaybookImportReviewPage() {
  const navigate = useNavigate();
  const { id = '' } = useParams();
  const [search] = useSearchParams();
  const whoami = $api.useQuery('get', '/api/whoami');
  const playbooks = $api.useQuery('get', '/api/playbooks');
  const importRow = $api.useQuery('get', '/api/playbooks/imports/{id}', {
    params: { path: { id } },
  });

  const compile = $api.useMutation('post', '/api/playbooks/imports/{id}/compile');
  const register = $api.useMutation('post', '/api/playbooks/imports/{id}/register');
  const discard = $api.useMutation('post', '/api/playbooks/imports/{id}/discard');
  const openDraft = $api.useMutation('post', '/api/playbooks/imports/{id}/draft');

  const [values, setValues] = useState<Record<string, string>>({});
  const [registryId, setRegistryId] = useState('');
  const [description, setDescription] = useState('');
  const [draftId, setDraftId] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [seeded, setSeeded] = useState(false);

  const row = importRow.data ?? null;
  const preview = row === null ? null : previewOf(row, compile.data);

  const existing =
    row === null ? null : matchExistingPlaybook(playbooks.data ?? NO_PLAYBOOKS, row.repo, row.path);
  const oldSchema = $api.useQuery(
    'get',
    '/api/playbooks/{id}/schema',
    { params: { path: { id: existing?.id ?? '' } } },
    { enabled: existing !== null }
  );
  const diff = useMemo(
    () => diffParamsSchemas(oldSchema.data, preview?.schema),
    [oldSchema.data, preview?.schema]
  );

  // The form starts on whatever the pack declared, and the registry identity on whatever the
  // proposer named: the link's query when one carries it (an agent proposing over MCP), otherwise
  // the registered row a re-import bumps, otherwise nothing.
  if (row !== null && !seeded) {
    setSeeded(true);
    const parsed = parseParamsSchema(row.params_schema ?? null);
    if (parsed.kind === 'form') setValues(initialValues(parsed.specs));
    const proposedId = search.get('id')?.trim();
    const proposedDescription = search.get('description')?.trim();
    setRegistryId(proposedId !== undefined && proposedId.length > 0 ? proposedId : (existing?.id ?? ''));
    setDescription(
      proposedDescription !== undefined && proposedDescription.length > 0
        ? proposedDescription
        : (existing?.description ?? '')
    );
  }

  const recompile = async () => {
    setError(null);
    try {
      await compile.mutateAsync({ params: { path: { id } }, body: compileBody(values) });
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const handleRegister = async () => {
    setError(null);
    try {
      await register.mutateAsync({
        params: { path: { id } },
        body: { id: registryId.trim(), description: description.trim() },
      });
      void navigate('/playbooks');
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const handleDiscard = async () => {
    setError(null);
    try {
      await discard.mutateAsync({ params: { path: { id } } });
      await importRow.refetch();
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const handleOpenDraft = async () => {
    setError(null);
    const wanted = draftId.trim();
    try {
      await openDraft.mutateAsync({
        params: { path: { id } },
        body: { id: wanted, description: row?.repo ?? wanted },
      });
      void navigate(`/playbooks/drafts/${wanted}`);
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  if (whoami.isPending || importRow.isPending) return <LoadingBlock label="LOADING THE IMPORT" />;
  if (importRow.isError || row === null || preview === null) {
    return (
      <>
        <Breadcrumb items={CRUMBS} />
        <Empty
          title="NO SUCH IMPORT"
          description={importRow.isError ? formatError(importRow.error) : `No import ${id}.`}
        />
      </>
    );
  }

  const admin = whoami.data?.role === 'admin';
  const pending = row.status === 'pending';
  const registrable = canRegister(row, preview);
  const idError = registryId.trim().length === 0 ? 'id is required' : null;

  return (
    <>
      <Breadcrumb items={CRUMBS} />
      <PageHeader
        eyebrow="Queue"
        title="Review an import"
        description="What was proposed, compiled by the pinned engine and frozen at the commit it resolved to."
      />

      <Section>
        <SectionHeader
          title="Proposal"
          actions={
            <span data-testid="import-status">
              <Mono size="data" tone="ink-3">
                {row.status}
              </Mono>
            </span>
          }
        />
        <SectionBody>
          <div className="grid gap-1 font-mono text-data text-ink-2">
            <div>
              {row.repo}
              {row.path === '' ? '' : ` · ${row.path}`}
              {row.git_ref === null || row.git_ref === undefined ? '' : ` · ${row.git_ref}`}
            </div>
            <div data-testid="import-rev">pinned at {row.rev.slice(0, 12)}</div>
            <div>
              proposed by {row.proposed_by ?? 'an anonymous caller'} at {row.created_at}
            </div>
            {row.playbook === null || row.playbook === undefined ? null : (
              <div>registered as {row.playbook}</div>
            )}
            {row.draft_id === null || row.draft_id === undefined ? null : (
              <div>opened as draft {row.draft_id}</div>
            )}
          </div>
        </SectionBody>
      </Section>

      <SharesSection path="/api/playbooks/imports/{id}" id={id} />

      {error === null ? null : (
        <Section>
          <SectionBody>
            <FormError>{error}</FormError>
          </SectionBody>
        </Section>
      )}

      {pending ? null : (
        <Section>
          <Notice label="Resolved">
            {`This import is ${row.status}. It is kept as the record of what was proposed; nothing else can act on it.`}
          </Notice>
        </Section>
      )}

      {graphAwaitsValues(preview) && (
        <Section>
          <Notice label="Graph pending">
            This pack declares required parameters. Supply values below and the graph compiles
            against the frozen pack; the pack is registrable either way.
          </Notice>
        </Section>
      )}

      {preview.exposureLines.length === 0 ? null : (
        <Section>
          <SectionHeader
            title="Exposure"
            actions={
              <Mono size="data" tone="ink-3">
                {preview.exposureDigest ?? 'none stored'}
              </Mono>
            }
          />
          <SectionBody>
            <pre className="m-0 max-w-[80ch] overflow-auto border border-rule bg-paper px-2 py-1.5 font-mono text-data whitespace-pre-wrap text-ink-2">
              {preview.exposureLines.join('\n')}
            </pre>
          </SectionBody>
        </Section>
      )}

      <PlaybookPreviewGate
        schema={preview.schema}
        graph={preview.graph}
        diagnostics={preview.diagnostics}
        dispatch={preview.dispatch}
        secrets={preview.secrets}
        values={values}
        onValueChange={(name, value) => {
          setValues((previous) => ({ ...previous, [name]: value }));
        }}
        onValuesSettled={() => {
          void recompile();
        }}
      />

      {existing === null ? null : (
        <Section>
          <SectionHeader
            title="Pin bump"
            actions={
              <Mono size="data" tone="ink-3">
                {existing.id}
              </Mono>
            }
          />
          <Notice label="Already registered">
            {`${existing.id} is pinned at ${existing.rev.slice(0, 12)}. Registering this import re-pins it and replaces the launch form below.`}
          </Notice>
          {(existing.exposure_digest ?? null) === preview.exposureDigest ? null : (
            <Notice label="Exposure changes">
              {`This bump changes what a run may write and reach: ${existing.exposure_digest ?? 'nothing stored'} becomes ${preview.exposureDigest ?? 'nothing stored'}. The declared exposure is below.`}
            </Notice>
          )}
          <SectionBody>
            {oldSchema.isPending ? (
              <LoadingBlock label="LOADING THE REGISTERED FORM" />
            ) : (
              <SchemaDiffView diff={diff} />
            )}
          </SectionBody>
        </Section>
      )}

      {!pending ? null : (
        <>
          <Section>
            <SectionHeader title="Register" />
            {registrable ? null : (
              <SectionBody>
                <Empty
                  title="NOTHING TO REGISTER"
                  description="The engine refused this pack's source. Fix it at the ref and propose again; the diagnostics above are its own."
                />
              </SectionBody>
            )}
            {registrable && !admin && (
              <SectionBody>
                <Empty
                  title="ADMIN ACCESS REQUIRED"
                  description="Operators propose imports; registering one into the launch registry is an admin's."
                />
              </SectionBody>
            )}
            {registrable && admin && (
              <>
                <SectionBody>
                  <FormGrid>
                    <TextField
                      id="import-id"
                      label="Registry id"
                      mono
                      required
                      value={registryId}
                      onChange={setRegistryId}
                      hint="Lowercase slug. Every launch of this pack is keyed by it."
                      error={registryId.length > 0 ? idError : null}
                    />
                    <TextField
                      id="import-description"
                      label="Description"
                      required
                      value={description}
                      onChange={setDescription}
                      hint="What this pack does, as the registry lists it."
                    />
                  </FormGrid>
                </SectionBody>
                <FormActions>
                  <Button
                    variant="filled"
                    disabled={
                      idError !== null || description.trim().length === 0 || register.isPending
                    }
                    onClick={() => {
                      void handleRegister();
                    }}
                  >
                    {register.isPending ? 'REGISTERING…' : existing === null ? 'REGISTER' : 'RE-PIN'}
                  </Button>
                  <Mono size="data" tone="ink-3">
                    {`pinned at ${row.rev.slice(0, 12)}`}
                  </Mono>
                </FormActions>
              </>
            )}
          </Section>

          <Section>
            <SectionHeader title="Edit instead" />
            <SectionBody>
              <FormGrid>
                <TextField
                  id="import-draft-id"
                  label="Draft id"
                  mono
                  value={draftId}
                  onChange={setDraftId}
                  hint="Opens a draft seeded from this proposal's frozen pack, to edit in the studio."
                />
              </FormGrid>
            </SectionBody>
            <FormActions>
              <Button
                disabled={draftId.trim().length === 0 || openDraft.isPending}
                onClick={() => {
                  void handleOpenDraft();
                }}
              >
                {openDraft.isPending ? 'OPENING…' : 'OPEN AS DRAFT'}
              </Button>
              <Button
                disabled={discard.isPending}
                onClick={() => {
                  void handleDiscard();
                }}
              >
                {discard.isPending ? 'DISCARDING…' : 'DISCARD'}
              </Button>
            </FormActions>
          </Section>
        </>
      )}
    </>
  );
}
