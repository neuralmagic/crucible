import { useEffect, useMemo, useReducer, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { $api, apiClient } from '../api/client';
import { formatError } from '../api/errors';
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
  Split,
  SplitHandle,
  SplitPane,
} from '../ui';
import {
  FormActions,
  FormError,
  FormGrid,
  Note,
  Notice,
  NumberInputField,
  TextField,
} from './formControls';
import { useMediaQuery } from '../useMediaQuery';
import { CodeSurface } from '../editor/CodeSurface';
import { CodeDiff } from '../editor/CodeDiff';
import { FileTreePanel } from '../editor/FileTreePanel';
import { DeleteDraftDialog } from './DeleteDraftDialog';
import { PlaybookParamFields } from './PlaybookParamFields';
import { SharesSection } from './SharesSection';
import { PackDispatchNotice } from './PackDispatchNotice';
import { ImagePickerField } from './ImagePickerField';
import { readSandboxImage, setSandboxImage } from './imagePicker';
import { ProviderModelField } from './ProviderModelField';
import { NO_AGENT_PICK, type AgentPick } from './agentPick';
import { WorkflowGraph } from './WorkflowGraph';
import {
  clampCeiling,
  initialValues,
  mapServerRejection,
  parseParamsSchema,
  type ParamFieldSpec,
} from './playbookLaunchForm';
import {
  anchorOf,
  draftLaunchBody,
  EMPTY_STUDIO,
  flaggedFiles,
  isDirty,
  markersFor,
  pathsOf,
  saveBody,
  staleBaseOf,
  originLabel,
  originMovedLabel,
  studioReducer,
  type StaleBase,
} from './draftStudio';

type CompileDto = components['schemas']['DraftCompileDto'];

/// The stored tree, read outside the query cache so the buffers are not reloaded.
async function fetchDraftFiles(id: string): Promise<Record<string, string>> {
  const { data } = await apiClient.GET('/api/playbook-drafts/{id}/files', {
    params: { path: { id } },
  });
  return data?.files ?? {};
}

const STUDIO_PANES = ['source', 'previews'];
/// Below this the two columns stack, and a divider between them would have nothing to divide.
const WIDE = '(min-width: 1200px)';

const DEFAULT_MAX_COST = 5;
const DEFAULT_MAX_TIME = '30m';
const MANIFEST = 'crucible.toml';
const NO_ERRORS: ReadonlyMap<string, string> = new Map();

/// The studio: the pack's text on the left, what the pinned engine makes of it on the right. One
/// save answers with all three — the diagnostics anchored to their lines, the launch form the
/// schema renders, and the compiled graph — so the previews are never stale against the buffer.
export function DraftStudioPage() {
  const { id = '' } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const wide = useMediaQuery(WIDE);
  const whoami = $api.useQuery('get', '/api/whoami');
  const draft = $api.useQuery('get', '/api/playbook-drafts/{id}', { params: { path: { id } } });
  const files = $api.useQuery('get', '/api/playbook-drafts/{id}/files', {
    params: { path: { id } },
  });
  const stored = $api.useQuery('get', '/api/playbook-drafts/{id}/preview', {
    params: { path: { id } },
  });
  const caps = $api.useQuery('get', '/api/config/playbook-caps');
  const origin = draft.data?.origin ?? null;
  const moved = originMovedLabel(origin);
  const originFiles = $api.useQuery(
    'get',
    '/api/playbook-drafts/{id}/origin/files',
    { params: { path: { id } } },
    { enabled: moved !== null }
  );

  const save = $api.useMutation('post', '/api/playbook-drafts/{id}/versions');
  const drop = $api.useMutation('delete', '/api/playbook-drafts/{id}');
  const launch = $api.useMutation('post', '/api/playbook-drafts/{id}/launch');
  const graduate = $api.useMutation('post', '/api/playbook-drafts/{id}/graduate');

  const [state, dispatch] = useReducer(studioReducer, EMPTY_STUDIO);
  const [preview, setPreview] = useState<CompileDto | null>(null);
  const [focus, setFocus] = useState<{ line: number; col: number; nonce: number } | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [stale, setStale] = useState<StaleBase | null>(null);
  const [overtaking, setOvertaking] = useState<Record<string, string>>({});
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  const [values, setValues] = useState<Record<string, string>>({});
  const [maxCost, setMaxCost] = useState(DEFAULT_MAX_COST);
  const [maxTime, setMaxTime] = useState(DEFAULT_MAX_TIME);
  const [agent, setAgent] = useState<AgentPick>(NO_AGENT_PICK);
  const [launchError, setLaunchError] = useState<string | null>(null);
  const [fieldErrors, setFieldErrors] = useState<ReadonlyMap<string, string>>(NO_ERRORS);

  const [gradRepo, setGradRepo] = useState('');
  const [gradPath, setGradPath] = useState('');
  const [gradError, setGradError] = useState<string | null>(null);
  const [prUrl, setPrUrl] = useState<string | null>(null);

  const loaded = files.data;
  useEffect(() => {
    if (loaded === undefined) return;
    dispatch({ kind: 'load', files: loaded.files, version: loaded.version });
  }, [loaded]);

  const target = origin?.repo ?? null;
  const targetPath = origin?.path ?? null;
  useEffect(() => {
    if (target === null) return;
    setGradRepo((previous) => (previous.length === 0 ? target : previous));
    setGradPath((previous) => (previous.length === 0 ? (targetPath ?? '') : previous));
  }, [target, targetPath]);

  const first = stored.data;
  useEffect(() => {
    if (first === undefined) return;
    setPreview(first);
  }, [first]);

  const parsed = useMemo(
    () =>
      preview?.params_schema === undefined || preview.params_schema === null
        ? null
        : parseParamsSchema(preview.params_schema),
    [preview]
  );
  const specs: ParamFieldSpec[] = useMemo(
    () => (parsed !== null && parsed.kind === 'form' ? parsed.specs : []),
    [parsed]
  );
  const schemaDigest = preview?.schema_digest ?? null;

  useEffect(() => {
    setValues((previous) => ({ ...initialValues(specs), ...previous }));
    // Only the field set matters here: a recompile that keeps the same params keeps what was typed.
  }, [specs]);

  const paths = useMemo(() => pathsOf(state.files), [state.files]);
  const diagnostics = useMemo(() => preview?.diagnostics ?? [], [preview]);
  const active = state.active;
  const markers = useMemo(
    () => markersFor(active, diagnostics, paths),
    [active, diagnostics, paths]
  );
  const flagged = useMemo(() => flaggedFiles(diagnostics, paths), [diagnostics, paths]);
  const retired = draft.data?.retired_at !== null && draft.data?.retired_at !== undefined;
  const admin = whoami.data?.role === 'admin';
  const actions = draft.data?.actions ?? [];
  const author = actions.includes('update');
  const writable = author && !retired;

  const handleSave = async () => {
    if (retired) return;
    setSaveError(null);
    setStale(null);
    const posted = state.revision;
    dispatch({ kind: 'saveStarted' });
    try {
      const dto = await save.mutateAsync({
        params: { path: { id } },
        body: saveBody(state),
      });
      setPreview(dto);
      dispatch({ kind: 'saveSettled', revision: posted, version: dto.version });
      void draft.refetch();
    } catch (err: unknown) {
      dispatch({ kind: 'saveFailed' });
      const overtaken = staleBaseOf(err);
      if (overtaken === null) {
        setSaveError(formatError(err));
        return;
      }
      setStale(overtaken);
      // The refusal names the version that won but not its text, and the diff is the point of the
      // prompt: read the stored tree without touching the buffers this writer still holds.
      try {
        setOvertaking(await fetchDraftFiles(id));
      } catch {
        setOvertaking({});
      }
    }
  };

  /// Take the version that overtook this save: the editor reloads onto it, and the buffers the
  /// merge prompt is holding are the ones the writer copies their own edits back out of.
  const handleReload = async () => {
    setStale(null);
    setOvertaking({});
    setSaveError(null);
    const [reloaded, recompiled] = await Promise.all([
      files.refetch(),
      stored.refetch(),
      draft.refetch(),
    ]);
    if (reloaded.data !== undefined) {
      dispatch({ kind: 'load', files: reloaded.data.files, version: reloaded.data.version });
    }
    if (recompiled.data !== undefined) setPreview(recompiled.data);
  };

  const handleLaunch = async () => {
    setLaunchError(null);
    setFieldErrors(NO_ERRORS);
    try {
      const ack = await launch.mutateAsync({
        params: { path: { id } },
        body: draftLaunchBody(specs, values, { maxCost, maxTime, schemaDigest }, agent),
      });
      void navigate(`/playbook-runs/${encodeURIComponent(ack.key)}`);
    } catch (err: unknown) {
      const rejection = mapServerRejection(
        err,
        specs.map((spec) => spec.name)
      );
      setFieldErrors(rejection.fieldErrors);
      setLaunchError(rejection.general ?? formatError(err));
    }
  };

  const handleDelete = async () => {
    setDeleteError(null);
    try {
      await drop.mutateAsync({ params: { path: { id } } });
      setConfirmDelete(false);
      void navigate('/playbooks/drafts');
    } catch (err: unknown) {
      setDeleteError(formatError(err));
    }
  };

  const handleGraduate = async () => {
    setGradError(null);
    try {
      const ack = await graduate.mutateAsync({
        params: { path: { id } },
        body: { repo: gradRepo.trim(), path: gradPath.trim() },
      });
      setPrUrl(ack.pr_url);
      void draft.refetch();
    } catch (err: unknown) {
      setGradError(formatError(err));
    }
  };

  const crumbs = [
    { label: 'Playbooks', to: '/playbooks' },
    { label: 'Drafts', to: '/playbooks/drafts' },
    { label: id },
  ];

  if (draft.isPending || files.isPending) return <LoadingBlock label="LOADING DRAFT" />;
  if (draft.isError) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty title="NO SUCH DRAFT" description={formatError(draft.error)} />
      </>
    );
  }

  const openUrl = draft.data?.graduation_pr_url ?? prUrl;

  const sourceColumn = (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto">
      <div className="flex min-h-[20rem] flex-1 flex-col gap-3 min-[900px]:flex-row">
        <FileTreePanel
          paths={paths}
          active={active}
          flagged={flagged}
          onSelect={(path) => {
            dispatch({ kind: 'select', file: path });
          }}
          onAdd={
            writable
              ? (path) => {
                  dispatch({ kind: 'add', file: path });
                }
              : undefined
          }
          onRename={
            writable
              ? (from, to) => {
                  dispatch({ kind: 'rename', from, to });
                }
              : undefined
          }
          onRemove={
            writable
              ? (path) => {
                  dispatch({ kind: 'remove', path });
                }
              : undefined
          }
        />
        {active === '' ? (
          <Empty title="NO FILES" />
        ) : (
          <CodeSurface
            path={active}
            value={state.files[active] ?? ''}
            onChange={(next) => {
              dispatch({ kind: 'edit', file: active, content: next });
            }}
            markers={markers}
            focus={focus}
            onSave={() => {
              void handleSave();
            }}
            readOnly={retired || !author}
            label={`${active} source`}
            testId="draft-editor"
          />
        )}
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <Button
          variant="filled"
          disabled={!author || retired || save.isPending}
          onClick={() => {
            void handleSave();
          }}
        >
          {save.isPending ? 'COMPILING…' : 'SAVE'}
        </Button>
        <Mono size="data" tone="ink-3">
          {isDirty(state) ? 'unsaved' : `saved · v${preview?.version ?? 0}`}
        </Mono>
      </div>

      {saveError === null ? null : <FormError>{saveError}</FormError>}

      {stale === null ? null : (
        <Section>
          <Notice label="Merge">
            <div className="grid gap-2" data-testid="draft-merge-prompt">
              <span>
                {`${stale.savedBy ?? 'Someone else'} saved v${stale.currentVersion} at ${stale.savedAt}, over the v${stale.baseVersion} this edit started from. Nothing was overwritten: your text is still in the editor.`}
              </span>
              <CodeDiff
                path={active}
                original={overtaking[active] ?? ''}
                modified={state.files[active] ?? ''}
                testId="draft-merge-diff"
              />
              <Note>
                The diff holds v{stale.currentVersion} as stored against your buffer, for{' '}
                {active === '' ? 'this pack' : active}. Pick another file in the tree to diff it
                too.
              </Note>
              <span>
                <Button
                  onClick={() => {
                    void handleReload();
                  }}
                >
                  RELOAD V{stale.currentVersion}
                </Button>
              </span>
              <Note>
                Reloading replaces the buffers with that save. Copy anything you still want out
                of the editor first.
              </Note>
            </div>
          </Notice>
        </Section>
      )}

      {moved === null ? null : (
        <Section>
          <SectionHeader
            title="Rebase"
            actions={
              <Mono size="data" tone="ink-3">
                {origin?.current_rev ?? ''}
              </Mono>
            }
          />
          <SectionBody>
            <div className="grid gap-2" data-testid="draft-rebase">
              <span className="font-mono text-data text-ink-2">{moved}</span>
              {originFiles.isPending ? (
                <LoadingBlock label="LOADING THE ORIGIN" />
              ) : originFiles.isError ? (
                <FormError>{formatError(originFiles.error)}</FormError>
              ) : (
                <CodeDiff
                  path={active}
                  original={originFiles.data?.files[active] ?? ''}
                  modified={state.files[active] ?? ''}
                  testId="draft-rebase-diff"
                />
              )}
              <Note>
                The diff holds the origin as it stands now against your buffer, for{' '}
                {active === '' ? 'this pack' : active}. Pick another file in the tree to diff it
                too; copying across is the rebase.
              </Note>
            </div>
          </SectionBody>
        </Section>
      )}

      <Section>
        <SectionHeader
          title="Diagnostics"
          actions={
            <Mono size="data" tone="ink-3">
              {diagnostics.length === 0 ? 'clean' : `${diagnostics.length}`}
            </Mono>
          }
        />
        <SectionBody>
          {diagnostics.length === 0 ? (
            <Note>The engine compiled this save without complaint.</Note>
          ) : (
            <ul className="m-0 grid list-none gap-1 p-0" data-testid="draft-diagnostics">
              {diagnostics.map((diagnostic, index) => {
                const anchor = anchorOf(diagnostic, paths);
                return (
                  <li key={`${diagnostic.message}-${index}`}>
                    <button
                      type="button"
                      disabled={anchor === null}
                      onClick={() => {
                        if (anchor === null) return;
                        dispatch({ kind: 'select', file: anchor.file });
                        setFocus({ line: anchor.line, col: anchor.col, nonce: Date.now() });
                      }}
                      className="w-full cursor-pointer border border-red bg-surface px-3 py-2 text-left font-mono text-data whitespace-pre-wrap text-red hover:bg-hi"
                    >
                      {anchor === null
                        ? diagnostic.message
                        : `${anchor.file}:${anchor.line}:${anchor.col}\n${diagnostic.message}`}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </SectionBody>
      </Section>
    </div>
  );

  const previewsColumn = (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto">
      {preview?.dispatch === undefined ? null : (
        <PackDispatchNotice dispatch={preview.dispatch} />
      )}
      {preview?.dispatch === undefined || retired || !author ? null : (
        <Section>
          <SectionHeader title="Sandbox image" />
          <SectionBody>
            <ImagePickerField
              idPrefix="studio"
              dispatch={preview.dispatch}
              current={readSandboxImage(state.files[MANIFEST] ?? '')}
              onPick={(reference) => {
                dispatch({
                  kind: 'edit',
                  file: MANIFEST,
                  content: setSandboxImage(state.files[MANIFEST] ?? '', reference),
                });
              }}
            />
          </SectionBody>
        </Section>
      )}

      <Section>
        <SectionHeader title="Launch form" />
        {parsed === null ? (
          <SectionBody>
            <Empty
              title="NO FORM"
              description="This save extracted no schema. The diagnostics beside it are the engine's own."
            />
          </SectionBody>
        ) : parsed.kind === 'unrenderable' ? (
          <SectionBody>
            <Empty title="FORM CANNOT BE RENDERED" description={parsed.reason} />
          </SectionBody>
        ) : specs.length === 0 ? (
          <SectionBody>
            <Empty title="NO PARAMETERS" description="This pack declares none." />
          </SectionBody>
        ) : (
          <SectionBody>
            <PlaybookParamFields
              idPrefix="studio"
              specs={specs}
              values={values}
              errors={fieldErrors}
              onChange={(name, value) => {
                setValues((previous) => ({ ...previous, [name]: value }));
              }}
              onBlur={() => undefined}
            />
          </SectionBody>
        )}
      </Section>

      <Section>
        <SectionHeader title="Graph" />
        <SectionBody>
          {preview?.graph === undefined || preview.graph === null ? (
            <Empty
              title="NO GRAPH"
              description="This save compiled no plan. Fix the source and save again."
            />
          ) : (
            <WorkflowGraph graph={preview.graph} />
          )}
        </SectionBody>
      </Section>

      <SharesSection path="/api/playbook-drafts/{id}" id={id} />

      <Section>
        <SectionHeader title="Test fire" />
        {schemaDigest === null ? (
          <SectionBody>
            <Empty
              title="NOTHING TO LAUNCH"
              description="The newest save has no form, so there is nothing to authorize a run against."
            />
          </SectionBody>
        ) : (
          <>
            <SectionBody>
              <FormGrid>
                <NumberInputField
                  id="studio-max-cost"
                  label="Max cost (USD)"
                  value={maxCost}
                  onChange={(next) => {
                    setMaxCost(clampCeiling(next, caps.data?.max_cost ?? null));
                  }}
                  hint={
                    caps.data === undefined ? undefined : `This controller caps it at ${caps.data.max_cost}.`
                  }
                />
                <TextField
                  id="studio-max-time"
                  label="Max time"
                  mono
                  value={maxTime}
                  onChange={setMaxTime}
                  hint={
                    caps.data === undefined ? undefined : `This controller caps it at ${caps.data.max_time}.`
                  }
                />
                <ProviderModelField
                  idPrefix="studio"
                  workloadClass="playbook"
                  value={agent}
                  onChange={(next) => {
                    setAgent(next);
                    setFieldErrors(NO_ERRORS);
                  }}
                />
              </FormGrid>
            </SectionBody>
            {launchError === null ? null : (
              <SectionBody>
                <FormError>{launchError}</FormError>
              </SectionBody>
            )}
            <FormActions>
              <Button
                variant="filled"
                disabled={
                  !actions.includes('launch') || retired || isDirty(state) || launch.isPending
                }
                onClick={() => {
                  void handleLaunch();
                }}
              >
                {launch.isPending ? 'LAUNCHING…' : 'LAUNCH DRAFT'}
              </Button>
              {isDirty(state) && (
                <Mono size="data" tone="ink-3">
                  save first
                </Mono>
              )}
            </FormActions>
          </>
        )}
      </Section>

      <Section>
        <SectionHeader title="Graduate" />
        <SectionBody>
          <Note>
            Exports the newest compiling save as a pull request. Once that pack is merged and
            imported, this draft retires.
          </Note>
        </SectionBody>
        {openUrl === null || openUrl === undefined ? (
          <>
            <SectionBody>
              <FormGrid>
                <TextField
                  id="studio-grad-repo"
                  label="Repo"
                  mono
                  required
                  value={gradRepo}
                  onChange={setGradRepo}
                  hint="owner/repo the export PR opens against."
                />
                <TextField
                  id="studio-grad-path"
                  label="Path"
                  mono
                  value={gradPath}
                  onChange={setGradPath}
                  hint="The pack directory inside that repo. Blank is the repo root."
                />
              </FormGrid>
            </SectionBody>
            {gradError === null ? null : (
              <SectionBody>
                <FormError>{gradError}</FormError>
              </SectionBody>
            )}
            <FormActions>
              <Button
                variant="filled"
                disabled={!admin || retired || gradRepo.trim().length === 0 || graduate.isPending}
                onClick={() => {
                  void handleGraduate();
                }}
              >
                {graduate.isPending ? 'EXPORTING…' : 'GRADUATE'}
              </Button>
            </FormActions>
          </>
        ) : (
          <SectionBody>
            <a href={openUrl} target="_blank" rel="noreferrer" className="font-mono text-data">
              {openUrl}
            </a>
          </SectionBody>
        )}
      </Section>
    </div>
  );

  return (
    <div className="flex h-full min-h-0 flex-col">
      <Breadcrumb items={crumbs} />
      <PageHeader
        eyebrow="Queue"
        title={id}
        description={draft.data?.description ?? ''}
        actions={
          <div className="flex items-center gap-3">
            <Mono size="data" tone="ink-3">
              {`v${draft.data?.latest_version ?? 0}`}
            </Mono>
            <span data-testid="draft-origin">
              <Mono size="data" tone="ink-3">{`from ${originLabel(origin)}`}</Mono>
            </span>
            <a
              href={`/api/playbook-drafts/${encodeURIComponent(id)}/tarball`}
              className="font-mono text-micro text-ink-2 underline"
              data-testid="draft-tarball"
            >
              TARBALL
            </a>
            <Button
              disabled={!actions.includes('delete')}
              onClick={() => {
                setDeleteError(null);
                setConfirmDelete(true);
              }}
            >
              DELETE DRAFT
            </Button>
          </div>
        }
      />

      <DeleteDraftDialog
        id={id}
        open={confirmDelete}
        pending={drop.isPending}
        error={deleteError}
        onOpenChange={setConfirmDelete}
        onConfirm={() => {
          void handleDelete();
        }}
      />

      {retired && (
        <Section>
          <Notice label="Retired">
            The graduated pack was imported, so this draft is read-only. Edit the registered pack
            instead.
          </Notice>
        </Section>
      )}

      {wide ? (
        <Split id="crucible.studio" panelIds={STUDIO_PANES} className="min-h-0 flex-1 px-4.5 py-4">
          <SplitPane id="source" defaultSize="50%" minSize="25%" className="pr-3">
            {sourceColumn}
          </SplitPane>
          <SplitHandle label="Resize the editor" />
          <SplitPane id="previews" minSize="20%" className="pl-3">
            {previewsColumn}
          </SplitPane>
        </Split>
      ) : (
        <div className="grid gap-4 px-4.5 py-4">
          {sourceColumn}
          {previewsColumn}
        </div>
      )}
    </div>
  );
}
