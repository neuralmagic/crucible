import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { CodeSurface } from '../editor/CodeSurface';
import { FileTreePanel } from '../editor/FileTreePanel';
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
import { sourceLabel } from './playbookSource';
import { SharesSection } from './SharesSection';

export function PlaybookDetailPage() {
  const { id = '' } = useParams();
  const navigate = useNavigate();
  const detail = $api.useQuery('get', '/api/playbooks/{id}', {
    params: { path: { id } },
  });
  const create = $api.useMutation('post', '/api/playbook-drafts');
  const [active, setActive] = useState('');
  const [draftId, setDraftId] = useState('');
  const [description, setDescription] = useState('');
  const [error, setError] = useState<string | null>(null);

  const files = useMemo(() => detail.data?.files ?? {}, [detail.data?.files]);
  const paths = useMemo(() => Object.keys(files).sort(), [files]);
  useEffect(() => {
    if (active === '' && paths.length > 0) setActive(paths[0]);
  }, [active, paths]);
  useEffect(() => {
    if (detail.data === undefined || description !== '') return;
    setDescription(`Edit of ${detail.data.id}`);
  }, [description, detail.data]);

  if (detail.isPending) return <LoadingBlock label="LOADING PLAYBOOK" />;
  if (detail.isError) return <Empty title="PLAYBOOK UNAVAILABLE" description={formatError(detail.error)} />;

  const playbook = detail.data;
  const clone = async () => {
    setError(null);
    try {
      await create.mutateAsync({
        body: {
          id: draftId.trim(),
          description: description.trim(),
          template: playbook.id,
          template_rev: playbook.rev,
          template_digest: playbook.tar_digest,
        },
      });
      void navigate(`/playbooks/drafts/${encodeURIComponent(draftId.trim())}`);
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  return (
    <>
      <Breadcrumb items={[{ label: 'Playbooks', to: '/playbooks' }, { label: playbook.id }]} />
      <PageHeader
        eyebrow="Pinned pack"
        title={playbook.id}
        description={playbook.description}
        actions={
          <Button variant="filled" render={<Link to={`/playbooks/${encodeURIComponent(playbook.id)}/launch`} />}>
            LAUNCH
          </Button>
        }
      />

      <Section>
        <SectionHeader title="Source" note="read only" />
        <SectionBody>
          <div className="mb-3 flex flex-wrap gap-x-6 gap-y-1 font-mono text-data text-ink-2">
            <span>{sourceLabel(playbook.source)}</span>
            <span>@ {playbook.rev}</span>
            <span>{playbook.tar_digest}</span>
          </div>
          <div className="flex min-h-[32rem] flex-col gap-3 min-[900px]:flex-row">
            <FileTreePanel paths={paths} active={active} onSelect={setActive} />
            {active === '' ? (
              <Empty title="NO FILES" />
            ) : (
              <CodeSurface
                path={active}
                value={files[active] ?? ''}
                onChange={() => {}}
                readOnly
                label={`${active} source`}
                testId="playbook-inspector"
              />
            )}
          </div>
        </SectionBody>
      </Section>

      <SharesSection path="/api/playbooks/{id}" id={playbook.id} />

      <Section>
        <SectionHeader title="Edit a copy" note={`seeded from ${playbook.rev.slice(0, 7)}`} />
        <SectionBody>
          <FormGrid>
            <TextField
              id="clone-draft-id"
              label="Draft id"
              mono
              required
              value={draftId}
              onChange={setDraftId}
              hint="A new lowercase slug; the registered playbook remains unchanged."
            />
            <TextField
              id="clone-description"
              label="Description"
              required
              value={description}
              onChange={setDescription}
            />
          </FormGrid>
          {error === null ? null : <FormError className="mt-3">{error}</FormError>}
        </SectionBody>
        <FormActions>
          <Mono size="data" tone="ink-3">The copy opens in Draft Studio as version 1.</Mono>
          <Button
            variant="filled"
            disabled={draftId.trim() === '' || description.trim() === '' || create.isPending}
            onClick={() => void clone()}
          >
            {create.isPending ? 'CLONING…' : 'EDIT A COPY'}
          </Button>
        </FormActions>
      </Section>
    </>
  );
}
