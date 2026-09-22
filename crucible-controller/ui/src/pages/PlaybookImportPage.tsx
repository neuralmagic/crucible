import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { withOwner } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import { OwnerField } from './OwnerField';
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
import { FormActions, FormError, FormGrid, TextField } from './formControls';
import {
  candidatesBody,
  INITIAL_IMPORT,
  proposeBody,
  withCandidates,
  withSelection,
  withSource,
  type ImportState,
} from './playbookImportWizard';

const CRUMBS = [{ label: 'Playbooks', to: '/playbooks' }, { label: 'Import' }];

/// The first half of an import: name a repo and a ref, see which directories hold a pack, and
/// propose one. Proposing is where the wizard stops holding state — the server fetches, compiles
/// and stores the row, and the flow continues at that row's own URL, which is shareable and
/// survives a refresh.
export function PlaybookImportPage() {
  const navigate = useNavigate();
  const whoami = $api.useQuery('get', '/api/whoami');

  const listCandidates = $api.useMutation('post', '/api/playbooks/import/candidates');
  const propose = $api.useMutation('post', '/api/playbooks/imports');

  const [state, setState] = useState<ImportState>(INITIAL_IMPORT);
  const [error, setError] = useState<string | null>(null);
  const [ownerChoice, setOwnerChoice] = useState('');
  const ownerContext = useOwnerContext();
  const owner = withOwner(ownerChoice, ownerContext.context, ownerContext.owners);

  const { source, candidates, selected, rev } = state;

  const handleSelect = async (path: string) => {
    setError(null);
    setState((previous) => withSelection(previous, path));
    try {
      const dto = await propose.mutateAsync({ body: proposeBody(source, path, owner) });
      void navigate(`/playbooks/import/${dto.id}`);
    } catch (err: unknown) {
      setState((previous) => withSelection(previous, ''));
      setError(formatError(err));
    }
  };

  const handleList = async () => {
    setError(null);
    try {
      const dto = await listCandidates.mutateAsync({ body: candidatesBody(source) });
      const listed = withCandidates(state, dto);
      setState(listed);
      const lone = listed.candidates?.length === 1 ? listed.candidates[0] : undefined;
      if (lone !== undefined) await handleSelect(lone.path);
    } catch (err: unknown) {
      setState((previous) => ({ ...previous, candidates: null, selected: null }));
      setError(formatError(err));
    }
  };

  if (whoami.isPending) return <LoadingBlock />;
  const role = whoami.data?.role;
  if (role !== 'admin' && role !== 'operator') {
    return (
      <>
        <Breadcrumb items={CRUMBS} />
        <Empty
          title="OPERATOR ACCESS REQUIRED"
          description="Proposing an import clones a repo and compiles it on the controller, so it needs an operator session. Registering what it proposes stays with an admin."
        />
      </>
    );
  }

  return (
    <>
      <Breadcrumb items={CRUMBS} />
      <PageHeader
        eyebrow="Queue"
        title="Import a pack"
        description="Fetch a pack repo and propose one of its packs. The proposal is frozen at the commit it resolved to, and carries its own link."
      />

      <Section>
        <SectionHeader title="Source" />
        <SectionBody>
          <FormGrid>
            <TextField
              id="import-repo"
              label="Repo"
              mono
              autoFocus
              value={source.repo}
              onChange={(next) => {
                setState(withSource({ ...source, repo: next }));
                setError(null);
              }}
              hint="owner/repo or a clone URL."
              required
            />
            <TextField
              id="import-ref"
              label="Ref"
              mono
              value={source.gitRef}
              onChange={(next) => {
                setState(withSource({ ...source, gitRef: next }));
                setError(null);
              }}
              hint="Branch or tag. Blank is the repo's default branch."
            />
            <OwnerField id="import-owner" value={owner} onChange={setOwnerChoice} options={ownerContext.owners} />
          </FormGrid>
        </SectionBody>
        <FormActions>
          <Button
            variant="filled"
            disabled={source.repo.trim().length === 0 || listCandidates.isPending}
            onClick={() => {
              void handleList();
            }}
          >
            {listCandidates.isPending ? 'FETCHING…' : 'FETCH PACKS'}
          </Button>
          {rev === null ? null : (
            <Mono size="data" tone="ink-3">
              {rev.slice(0, 12)}
            </Mono>
          )}
        </FormActions>
      </Section>

      {error === null ? null : (
        <Section>
          <SectionBody>
            <FormError>{error}</FormError>
          </SectionBody>
        </Section>
      )}

      {candidates === null ? null : (
        <Section>
          <SectionHeader
            title="Packs"
            actions={
              <Mono size="data" tone="ink-3">
                {candidates.length}
              </Mono>
            }
          />
          {candidates.length === 0 ? (
            <SectionBody>
              <Empty
                title="NO PACKS AT THIS REF"
                description="No directory here holds a crucible.toml declaring a playbook workflow."
              />
            </SectionBody>
          ) : (
            <ul className="m-0 list-none p-0">
              {candidates.map((candidate) => (
                <li key={candidate.path} className="border-b border-rule">
                  <button
                    type="button"
                    disabled={propose.isPending}
                    aria-pressed={candidate.path === selected}
                    onClick={() => {
                      void handleSelect(candidate.path);
                    }}
                    className={`flex w-full cursor-pointer items-baseline gap-3 px-4.5 py-2.5 text-left font-mono text-data hover:bg-hi ${
                      candidate.path === selected ? 'bg-hi font-semibold text-ink' : 'text-ink-2'
                    }`}
                  >
                    <span>{candidate.path === '' ? '(repo root)' : candidate.path}</span>
                    <span className="text-ink-3">{candidate.workflow_file}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </Section>
      )}

      {propose.isPending && <LoadingBlock label="FETCHING AND COMPILING THE PACK" />}
    </>
  );
}
