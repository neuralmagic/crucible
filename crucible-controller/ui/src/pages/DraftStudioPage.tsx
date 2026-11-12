import { useEffect, useMemo, useReducer, useRef, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { $api, apiClient } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import {
  Breadcrumb,
  Button,
  CopyButton,
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
  SelectField,
  TextAreaField,
  TextField,
} from './formControls';
import { useMediaQuery } from '../useMediaQuery';
import { CodeSurface } from '../editor/CodeSurface';
import { CodeDiff } from '../editor/CodeDiff';
import { FileTreePanel } from '../editor/FileTreePanel';
import { CloneDraftDialog } from './CloneDraftDialog';
import { DeleteDraftDialog } from './DeleteDraftDialog';
import { RenameDraftDialog } from './RenameDraftDialog';
import { PlaybookParamFields } from './PlaybookParamFields';
import { SharesSection } from './SharesSection';
import { PackDispatchNotice } from './PackDispatchNotice';
import { ImagePickerField } from './ImagePickerField';
import { EngineLogSection } from './EngineLogSection';
import { TaskOutputSection } from './TaskOutputSection';
import { RunTaskGrid } from './RunTaskGrid';
import { RunTaskGraph } from './RunTaskGraph';
import { CancelLaunchControl } from './CancelLaunchControl';
import { LiveSession } from '../live/LiveSession';
import { ArtifactsPanel, CostSection, FilesPanel } from './RunDetailPage';
import { readSandboxImage, setSandboxImage } from './imagePicker';
import { ProviderModelField } from './ProviderModelField';
import { NO_AGENT_PICK, type AgentPick } from './agentPick';
import { WorkflowGraph } from './WorkflowGraph';
import { projectionLabel, reasonLabel } from './blueprints';
import { formatCost } from './runReport';
import {
  clampCeiling,
  initialValues,
  mapServerRejection,
  parseParamsSchema,
  type ParamFieldSpec,
} from './playbookLaunchForm';
import {
  anchorOf,
  cloneDraftBody,
  draftLaunchBody,
  EMPTY_CLONE,
  EMPTY_STUDIO,
  fireIsLive,
  flaggedFiles,
  isDirty,
  markersFor,
  pathsOf,
  redraftIntent,
  saveBody,
  staleBaseOf,
  originLabel,
  originMovedLabel,
  studioReducer,
  type CloneForm,
  type StaleBase,
} from './draftStudio';
import { runPath } from './runFileView';

type CompileDto = components['schemas']['DraftCompileDto'];
type GeneratedDto = components['schemas']['GeneratedDraftDto'];
type LeftBinding = components['schemas']['LeftBindingDto'];
type SecretDto = components['schemas']['SecretDto'];

/// What the last generate did, as the studio reports it back: one line under the panel.
interface GenerateReport {
  version: number;
  rounds: number;
  costUsd: number;
  ok: boolean;
  overBudget: boolean;
}

function reportOf(dto: GeneratedDto): GenerateReport {
  return {
    version: dto.version,
    rounds: dto.rounds,
    costUsd: dto.cost_usd,
    ok: dto.ok,
    overBudget: dto.over_budget,
  };
}

type GenerateProgress = components['schemas']['DraftProgressDto'];

/// The live tail of the drafting turn, read outside the query cache so the panel is its own
/// cadence, not the page's.
async function fetchGenerateProgress(id: string): Promise<GenerateProgress> {
  const { data } = await apiClient.GET('/api/playbook-drafts/{id}/generate/progress', {
    params: { path: { id } },
  });
  return data ?? { lines: [], running: false };
}

/// How many progress lines the panel shows; older lines slide off the top.
const PROGRESS_SHOWN = 12;

/// How often a live test-fire's detail refetches while the run works.
const FIRE_POLL_MS = 5000;

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
const NO_PLACEHOLDERS: readonly components['schemas']['PlaceholderDto'][] = [];

/// The studio: the pack's text on the left, what the pinned engine makes of it on the right. One
/// save answers with all three — the diagnostics anchored to their lines, the launch form the
/// schema renders, and the compiled graph — so the previews are never stale against the buffer.
export function DraftStudioPage() {
  const { id = '' } = useParams<{ id: string }>();
  return <DraftStudio key={id} id={id} />;
}

/// Keyed by the draft id, so a rename or a clone that lands on another draft starts from that
/// draft's own state rather than this one's.
function DraftStudio({ id }: { id: string }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const wide = useMediaQuery(WIDE);
  const draft = $api.useQuery('get', '/api/playbook-drafts/{id}', { params: { path: { id } } });
  const placeholders = draft.data?.placeholders ?? NO_PLACEHOLDERS;
  const files = $api.useQuery('get', '/api/playbook-drafts/{id}/files', {
    params: { path: { id } },
  });
  const stored = $api.useQuery('get', '/api/playbook-drafts/{id}/preview', {
    params: { path: { id } },
  });
  const caps = $api.useQuery('get', '/api/config/playbook-caps');
  const whoami = $api.useQuery('get', '/api/whoami');
  const githubTokens = $api.useQuery(
    'get',
    '/api/secrets',
    { params: { query: { kind: 'github_token' } } },
    { enabled: whoami.data?.user != null },
  );
  const origin = draft.data?.origin ?? null;
  const moved = originMovedLabel(origin);
  const originFiles = $api.useQuery(
    'get',
    '/api/playbook-drafts/{id}/origin/files',
    { params: { path: { id } } },
    { enabled: moved !== null }
  );

  // The draft's newest test-fire, adopted on load so a refresh keeps the card and refetched
  // after a launch so the card follows the fire that launch lit.
  const fires = $api.useQuery('get', '/api/playbook-runs', {
    params: { query: { playbook: id, origin: 'draft' } },
  });
  const fireKey = fires.data !== undefined && fires.data.length > 0 ? fires.data[0].key : null;
  const fire = $api.useQuery(
    'get',
    '/api/playbook-runs/{key}',
    { params: { path: { key: fireKey ?? '' } } },
    {
      enabled: fireKey !== null,
      refetchInterval: (query) =>
        query.state.data !== undefined && fireIsLive(query.state.data) ? FIRE_POLL_MS : false,
    },
  );
  // The run the run view follows: a fire's runs are newest first, so the first is the one this
  // fire is working on. Null until the dispatch produced a run.
  const fireRun = fire.data?.runs[0] ?? null;
  const fireRunLive = fireRun !== null && fireRun.status === 'running';

  const save = $api.useMutation('post', '/api/playbook-drafts/{id}/versions');
  const generate = $api.useMutation('post', '/api/playbook-drafts/{id}/generate');
  const drop = $api.useMutation('delete', '/api/playbook-drafts/{id}');
  const rename = $api.useMutation('post', '/api/playbook-drafts/{id}/rename');
  const clone = $api.useMutation('post', '/api/playbook-drafts/{id}/clone');
  const launch = $api.useMutation('post', '/api/playbook-drafts/{id}/launch');
  const graduate = $api.useMutation('post', '/api/playbook-drafts/{id}/graduate');
  const publish = $api.useMutation('post', '/api/playbook-drafts/{id}/publish');

  const [state, dispatch] = useReducer(studioReducer, EMPTY_STUDIO);
  const [preview, setPreview] = useState<CompileDto | null>(null);
  const [focus, setFocus] = useState<{ line: number; col: number; nonce: number } | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [stale, setStale] = useState<StaleBase | null>(null);
  const [overtaking, setOvertaking] = useState<Record<string, string>>({});
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [renameTo, setRenameTo] = useState('');
  const [renameError, setRenameError] = useState<string | null>(null);
  const [cloning, setCloning] = useState(false);
  const [cloneForm, setCloneForm] = useState<CloneForm>(EMPTY_CLONE);
  const [cloneError, setCloneError] = useState<string | null>(null);
  const [cloneLeft, setCloneLeft] = useState<readonly LeftBinding[] | null>(null);

  const [values, setValues] = useState<Record<string, string>>({});
  const [maxCost, setMaxCost] = useState(DEFAULT_MAX_COST);
  const [maxTime, setMaxTime] = useState(DEFAULT_MAX_TIME);
  const [agent, setAgent] = useState<AgentPick>(NO_AGENT_PICK);
  const [launchError, setLaunchError] = useState<string | null>(null);
  const [fieldErrors, setFieldErrors] = useState<ReadonlyMap<string, string>>(NO_ERRORS);

  const [intent, setIntent] = useState('');
  const [generateError, setGenerateError] = useState<string | null>(null);
  const [redraftError, setRedraftError] = useState<string | null>(null);
  const [generateReport, setGenerateReport] = useState<GenerateReport | null>(null);
  const [genElapsed, setGenElapsed] = useState(0);
  useEffect(() => {
    if (!generate.isPending) return;
    setGenElapsed(0);
    const started = Date.now();
    const timer = window.setInterval(() => {
      setGenElapsed(Math.floor((Date.now() - started) / 1000));
    }, 1000);
    return () => window.clearInterval(timer);
  }, [generate.isPending]);

  const [progress, setProgress] = useState<GenerateProgress>({ lines: [], running: false });
  /// Set once this page has seen a turn run: the panel keeps a finished turn's tail, but a fresh
  /// page must not show the lines of a generate that ended long before it loaded.
  const [watchedTurn, setWatchedTurn] = useState(false);
  // A refresh drops the mutation's pending state, but the turn runs on: the server's word on
  // mount is what picks it back up — including the intent the box held, which the refresh
  // emptied but the turn still runs on.
  useEffect(() => {
    void fetchGenerateProgress(id)
      .then((next) => {
        setProgress(next);
        if (next.running) {
          setWatchedTurn(true);
          setIntent((previous) => (previous.length === 0 ? next.intent ?? '' : previous));
        }
      })
      .catch(() => undefined);
  }, [id]);
  useEffect(() => {
    if (!generate.isPending && !progress.running) return;
    const timer = window.setInterval(() => {
      void fetchGenerateProgress(id).then(setProgress).catch(() => undefined);
    }, 2000);
    return () => window.clearInterval(timer);
  }, [generate.isPending, progress.running, id]);
  // A turn this page watched may have been started by a request that died with a refresh; when
  // the server says it ended, the version it saved is this page's to pick up.
  const wasRunning = useRef(false);
  const refetchFiles = files.refetch;
  const refetchDraft = draft.refetch;
  const refetchStored = stored.refetch;
  useEffect(() => {
    if (wasRunning.current && !progress.running) {
      void refetchFiles();
      void refetchDraft();
      void refetchStored();
    }
    wasRunning.current = progress.running;
  }, [progress.running, refetchFiles, refetchDraft, refetchStored]);

  const [gradRepo, setGradRepo] = useState('');
  const [gradPath, setGradPath] = useState('');
  const [gradSecretId, setGradSecretId] = useState('');
  const [gradError, setGradError] = useState<string | null>(null);
  const [prUrl, setPrUrl] = useState<string | null>(null);

  const [publishTo, setPublishTo] = useState('');
  const [publishError, setPublishError] = useState<string | null>(null);
  const [publishedRev, setPublishedRev] = useState<string | null>(null);

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

  const publishTarget = draft.data?.published_playbook ?? origin?.playbook ?? null;
  useEffect(() => {
    if (publishTarget === null) return;
    setPublishTo((previous) => (previous.length === 0 ? publishTarget : previous));
  }, [publishTarget]);

  const githubSecretOptions = useMemo(
    () => [
      { value: '', label: 'Controller identity' },
      ...(githubTokens.data ?? []).map((secret: SecretDto) => ({
        value: secret.id,
        label: `${secret.name} (${secret.owner})`,
      })),
    ],
    [githubTokens.data],
  );

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

  /// The turn spends before anything is stored, so a refusal still books its cost; what the
  /// buffers hold is seeded from the newest save, not from unsaved edits.
  const handleGenerate = async (intentText: string) => {
    if (retired) return;
    setGenerateError(null);
    setStale(null);
    setGenerateReport(null);
    setWatchedTurn(true);
    setProgress({ lines: [], running: true });
    try {
      const dto = await generate.mutateAsync({
        params: { path: { id } },
        body: { intent: intentText.trim() },
      });
      setPreview(dto);
      setGenerateReport(reportOf(dto));
      void files.refetch();
      void draft.refetch();
    } catch (err: unknown) {
      const overtaken = staleBaseOf(err);
      if (overtaken === null) {
        setGenerateError(formatError(err));
        return;
      }
      setStale(overtaken);
      try {
        setOvertaking(await fetchDraftFiles(id));
      } catch {
        setOvertaking({});
      }
    } finally {
      // The polling interval stops the moment running flips; this read catches the tail it missed.
      void fetchGenerateProgress(id).then(setProgress).catch(() => undefined);
    }
  };

  /// Feed a failed test-fire back into a drafting turn: the error becomes the intent and the turn
  /// runs on it at once — the box keeps the text, so the next round edits it rather than starting
  /// from a blank slate.
  const handleRedraft = async (runId: string | null, dispatchFailure: string | null) => {
    setRedraftError(null);
    let failure = dispatchFailure;
    if (failure === null && runId !== null) {
      try {
        const { data } = await apiClient.GET('/api/runs/{run_id}/tasks/log', {
          params: { path: { run_id: runId } },
        });
        failure = data?.text ?? null;
      } catch (err: unknown) {
        setRedraftError(formatError(err));
        return;
      }
    }
    const intentText = redraftIntent(failure);
    setIntent(intentText);
    await handleGenerate(intentText);
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
      await launch.mutateAsync({
        params: { path: { id } },
        body: draftLaunchBody(specs, values, { maxCost, maxTime, schemaDigest }, agent),
      });
      // The studio keeps the fire: the card below the button follows it, so the next edit and
      // the next launch never leave this page.
      void fires.refetch();
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

  const handleRename = async () => {
    setRenameError(null);
    try {
      const renamed = await rename.mutateAsync({
        params: { path: { id } },
        body: { id: renameTo.trim() },
      });
      await queryClient.invalidateQueries({ queryKey: ['get', '/api/playbook-drafts'] });
      setRenaming(false);
      void navigate(`/playbooks/drafts/${encodeURIComponent(renamed.id)}`, { replace: true });
    } catch (err: unknown) {
      setRenameError(formatError(err));
    }
  };

  const openCopy = (copy: string) => {
    setCloning(false);
    void navigate(`/playbooks/drafts/${encodeURIComponent(copy)}`);
  };

  const handleClone = async () => {
    setCloneError(null);
    try {
      const cloned = await clone.mutateAsync({
        params: { path: { id } },
        body: cloneDraftBody(cloneForm),
      });
      await queryClient.invalidateQueries({ queryKey: ['get', '/api/playbook-drafts'] });
      if (cloned.left_bindings.length === 0) {
        openCopy(cloned.id);
      } else {
        setCloneLeft(cloned.left_bindings);
      }
    } catch (err: unknown) {
      setCloneError(formatError(err));
    }
  };

  const handleGraduate = async () => {
    setGradError(null);
    try {
      const ack = await graduate.mutateAsync({
        params: { path: { id } },
        body: {
          repo: gradRepo.trim(),
          path: gradPath.trim(),
          ...(gradSecretId === '' ? {} : { github_secret_id: gradSecretId }),
        },
      });
      setPrUrl(ack.pr_url);
      void draft.refetch();
    } catch (err: unknown) {
      setGradError(formatError(err));
    }
  };

  const handlePublish = async () => {
    setPublishError(null);
    try {
      const ack = await publish.mutateAsync({
        params: { path: { id } },
        body: { playbook: publishTo.trim() },
      });
      setPublishedRev(ack.rev);
      void draft.refetch();
    } catch (err: unknown) {
      setPublishError(formatError(err));
    }
  };

  const crumbs = [
    { label: 'Playbooks', to: '/playbooks' },
    { label: 'Drafts', to: '/playbooks/drafts' },
    { label: id },
  ];

  if (draft.isPending || files.isPending) return <LoadingBlock label="Loading draft" />;
  if (draft.isError) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty title="No such draft" description={formatError(draft.error)} />
      </>
    );
  }

  const openUrl = draft.data?.graduation_pr_url ?? prUrl;

  const sourceColumn = (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto">
      {author ? (
        <Section>
          <SectionHeader title="Generate" />
          <SectionBody>
            <div className="grid gap-2" data-testid="draft-generate">
              <TextAreaField
                id="studio-generate-intent"
                label="Intent"
                placeholder="Describe the experiment you want — the model, the knob to sweep, how to rank the arms."
                hint="The engine's drafting turn writes the next version from the newest save and this intent. It takes a few minutes."
                value={intent}
                onChange={setIntent}
              />
            </div>
          </SectionBody>
          {generateError === null ? null : (
            <SectionBody>
              <FormError>{generateError}</FormError>
            </SectionBody>
          )}
          <FormActions>
            <Button
              variant="filled"
              className="uppercase"
              disabled={
                !writable ||
                isDirty(state) ||
                generate.isPending ||
                progress.running ||
                intent.trim().length === 0
              }
              onClick={() => {
                void handleGenerate(intent);
              }}
            >
              {generate.isPending
                ? `Drafting… ${genElapsed}s`
                : progress.running
                  ? 'Drafting…'
                  : 'Generate'}
            </Button>
            {isDirty(state) && (
              <Mono size="data" tone="ink-3">
                seeds from the newest save
              </Mono>
            )}
            {generateReport === null ? null : (
              <Mono size="data" tone="ink-3">
                {`v${generateReport.version} · ${generateReport.rounds} rounds · ${formatCost(generateReport.costUsd)} · check ${generateReport.ok ? 'passed' : 'failed'}`}
              </Mono>
            )}
          </FormActions>
          {generateReport !== null && generateReport.overBudget ? (
            <SectionBody>
              <Note>The drafting turn ran past its budget; what it wrote is stored as it stood.</Note>
            </SectionBody>
          ) : null}
          {!watchedTurn || progress.lines.length === 0 ? null : (
            <SectionBody>
              <div
                data-testid="draft-generate-progress"
                className="max-h-56 overflow-y-auto rounded border border-rule bg-surface p-2 font-mono text-data leading-5"
              >
                {progress.lines.slice(-PROGRESS_SHOWN).map((entry, index) => (
                  <div key={index} className="flex gap-2 whitespace-pre-wrap break-all">
                    <span className="shrink-0 text-ink-3">r{entry.round}</span>
                    <span>{entry.line}</span>
                  </div>
                ))}
              </div>
            </SectionBody>
          )}
        </Section>
      ) : null}

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
          <Empty title="No files" />
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
          className="uppercase"
          disabled={!author || retired || save.isPending}
          onClick={() => {
            void handleSave();
          }}
        >
          {save.isPending ? 'Compiling…' : 'Save'}
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
                  className="uppercase"
                  onClick={() => {
                    void handleReload();
                  }}
                >
                  Reload v{stale.currentVersion}
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
                <LoadingBlock label="Loading the origin" />
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

      {placeholders.length === 0 ? null : (
        <Section>
          <SectionHeader
            title="Placeholders"
            actions={
              <Button className="uppercase" render={<Link to="/secrets" />}>
                Bind
              </Button>
            }
          />
          <SectionBody>
            <ul className="m-0 grid list-none gap-1 p-0" data-testid="draft-placeholders">
              {placeholders.map((placeholder) => (
                <li key={placeholder.declared_name} className="flex flex-wrap gap-x-3">
                  <Mono size="data">{placeholder.declared_name}</Mono>
                  <Mono size="data" tone="ink-3">
                    {projectionLabel(placeholder)}
                  </Mono>
                  <span className="text-ink-2">{reasonLabel(placeholder.reason)}</span>
                </li>
              ))}
            </ul>
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
                const location =
                  anchor === null ? null : `${anchor.file}:${anchor.line}:${anchor.col}`;
                return (
                  <li
                    key={`${diagnostic.message}-${index}`}
                    className="relative border border-red bg-surface py-2 pr-9 pl-3 font-mono text-data whitespace-pre-wrap text-red"
                  >
                    <CopyButton
                      value={location === null ? diagnostic.message : `${location}\n${diagnostic.message}`}
                      label="diagnostic"
                      className="absolute top-1 right-1"
                    />
                    {anchor === null ? null : (
                      <button
                        type="button"
                        onClick={() => {
                          dispatch({ kind: 'select', file: anchor.file });
                          setFocus({ line: anchor.line, col: anchor.col, nonce: Date.now() });
                        }}
                        className="block cursor-pointer border-0 bg-transparent p-0 text-left font-mono text-data text-red underline hover:text-ink"
                      >
                        {location}
                      </button>
                    )}
                    {diagnostic.message}
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
              title="No form"
              description="This save extracted no schema. The diagnostics beside it are the engine's own."
            />
          </SectionBody>
        ) : parsed.kind === 'unrenderable' ? (
          <SectionBody>
            <Empty title="Form cannot be rendered" description={parsed.reason} />
          </SectionBody>
        ) : specs.length === 0 ? (
          <SectionBody>
            <Empty title="No parameters" description="This pack declares none." />
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
              title="No graph"
              description="This save compiled no plan. Fix the source and save again."
            />
          ) : (
            <WorkflowGraph graph={preview.graph} splitId="crucible.studio-graph" />
          )}
        </SectionBody>
      </Section>

      <SharesSection path="/api/playbook-drafts/{id}" id={id} />

      <Section>
        <SectionHeader title="Test fire" />
        {schemaDigest === null ? (
          <SectionBody>
            <Empty
              title="Nothing to launch"
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
                className="uppercase"
                disabled={
                  !actions.includes('launch') || retired || isDirty(state) || launch.isPending
                }
                onClick={() => {
                  void handleLaunch();
                }}
              >
                {launch.isPending ? 'Launching…' : 'Launch draft'}
              </Button>
              {isDirty(state) && (
                <Mono size="data" tone="ink-3">
                  save first
                </Mono>
              )}
            </FormActions>
          </>
        )}
        {fire.data === undefined || fireKey === null ? null : (
          <SectionBody>
            <div
              data-testid="draft-test-fire"
              className="grid gap-1.5 rounded border border-rule bg-surface p-2 font-mono text-data"
            >
              {fire.data.runs.map((run) => (
                <div key={run.run_id} className="flex flex-wrap items-center gap-2">
                  <Link to={runPath(run.run_id, fireKey)}>{run.name ?? run.run_id}</Link>
                  <span className="text-ink-3">{run.status}</span>
                  {run.cost_usd === null || run.cost_usd === undefined ? null : (
                    <span className="text-ink-3">{formatCost(run.cost_usd)}</span>
                  )}
                  {run.status === 'running' ? (
                    <CancelLaunchControl launchKey={fireKey} />
                  ) : null}
                  {run.status === 'failed' && writable ? (
                    <Button
                      className="uppercase"
                      disabled={isDirty(state) || generate.isPending || progress.running}
                      onClick={() => {
                        void handleRedraft(run.run_id, null);
                      }}
                    >
                      Re-draft from this failure
                    </Button>
                  ) : null}
                </div>
              ))}
              {fire.data.runs.length === 0 && fire.data.dispatch.failure != null && (
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-ink-3">never dispatched</span>
                  <Button
                    className="uppercase"
                    disabled={isDirty(state) || generate.isPending || progress.running}
                    onClick={() => {
                      void handleRedraft(null, fire.data?.dispatch.failure ?? null);
                    }}
                  >
                    Re-draft from this failure
                  </Button>
                </div>
              )}
            </div>
          </SectionBody>
        )}
        {redraftError === null ? null : (
          <SectionBody>
            <FormError>{redraftError}</FormError>
          </SectionBody>
        )}
      </Section>

      {fireRun === null || fireKey === null ? null : (
        <FireRunView
          key={fireRun.run_id}
          runId={fireRun.run_id}
          launchKey={fireKey}
          runCost={fireRun.cost_usd ?? null}
          live={fireRunLive}
          onEnded={() => void fire.refetch()}
        />
      )}

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
                <SelectField
                  id="studio-grad-secret"
                  label="GitHub credential"
                  value={gradSecretId}
                  onChange={setGradSecretId}
                  options={githubSecretOptions}
                  hint="A github_token secret pushes and opens the PR as its account."
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
                className="uppercase"
                disabled={
                  !actions.includes('approve') ||
                  retired ||
                  gradRepo.trim().length === 0 ||
                  graduate.isPending
                }
                onClick={() => {
                  void handleGraduate();
                }}
              >
                {graduate.isPending ? 'Exporting…' : 'Graduate'}
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

      {actions.includes('publish') ? (
        <Section>
          <SectionHeader title="Publish" note="no review" />
          <SectionBody>
            <FormGrid>
              <TextField
                id="studio-publish-playbook"
                label="Playbook"
                mono
                required
                value={publishTo}
                onChange={setPublishTo}
              />
            </FormGrid>
          </SectionBody>
          {publishError === null ? null : (
            <SectionBody>
              <FormError>{publishError}</FormError>
            </SectionBody>
          )}
          <FormActions>
            <Button
              variant="filled"
              className="uppercase"
              disabled={
                retired || isDirty(state) || publishTo.trim().length === 0 || publish.isPending
              }
              onClick={() => {
                void handlePublish();
              }}
            >
              {publish.isPending ? 'Publishing…' : 'Publish'}
            </Button>
            {draft.data?.published_playbook === null ||
            draft.data?.published_playbook === undefined ? null : (
              <Link
                to={`/playbooks/${draft.data.published_playbook}`}
                className="font-mono text-data"
              >
                {publishedRev === null
                  ? draft.data.published_playbook
                  : `${draft.data.published_playbook} @ ${publishedRev.slice(0, 19)}`}
              </Link>
            )}
          </FormActions>
        </Section>
      ) : null}
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
              className="font-mono text-micro text-ink-2 uppercase underline"
              data-testid="draft-tarball"
            >
              Tarball
            </a>
            <Button
              className="uppercase"
              disabled={!writable || isDirty(state)}
              onClick={() => {
                setRenameError(null);
                setRenameTo(id);
                setRenaming(true);
              }}
            >
              Rename
            </Button>
            <Button
              className="uppercase"
              disabled={isDirty(state)}
              onClick={() => {
                setCloneError(null);
                setCloneLeft(null);
                setCloneForm(EMPTY_CLONE);
                setCloning(true);
              }}
            >
              Clone
            </Button>
            <Button
              className="uppercase"
              disabled={!actions.includes('delete')}
              onClick={() => {
                setDeleteError(null);
                setConfirmDelete(true);
              }}
            >
              Delete draft
            </Button>
          </div>
        }
      />

      <RenameDraftDialog
        id={id}
        open={renaming}
        to={renameTo}
        pending={rename.isPending}
        error={renameError}
        onToChange={setRenameTo}
        onOpenChange={setRenaming}
        onConfirm={() => {
          void handleRename();
        }}
      />

      <CloneDraftDialog
        id={id}
        open={cloning}
        form={cloneForm}
        pending={clone.isPending}
        error={cloneError}
        left={cloneLeft}
        onFormChange={setCloneForm}
        onOpenChange={(open) => {
          if (!open && cloneLeft !== null) {
            openCopy(cloneForm.to.trim());
            return;
          }
          setCloning(open);
        }}
        onConfirm={() => {
          void handleClone();
        }}
        onOpenCopy={() => {
          openCopy(cloneForm.to.trim());
        }}
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

/// The run a test fire produced, in the studio pane: the same sections the run page shows a run,
/// in the same order, minus the header the fire card already is. A captured file opens in place —
/// the file's own URL belongs to the run page, and following it here would leave the studio.
function FireRunView({
  runId,
  launchKey,
  runCost,
  live,
  onEnded,
}: {
  runId: string;
  launchKey: string;
  runCost: number | null;
  live: boolean;
  onEnded: () => void;
}) {
  const [fileKey, setFileKey] = useState<string | null>(null);
  return (
    <>
      {live && (
        <Section>
          <SectionHeader title="Live session" />
          <SectionBody>
            <LiveSession runId={runId} compact onEnded={onEnded} />
          </SectionBody>
        </Section>
      )}
      <RunTaskGraph runId={runId} />
      <RunTaskGrid runId={runId} />
      <TaskOutputSection runId={runId} live={live} />
      <EngineLogSection runId={runId} live={live} />
      <FilesPanel
        runId={runId}
        launchKey={launchKey}
        fileKey={fileKey}
        live={live}
        onOpenFile={setFileKey}
      />
      <div className="grid grid-cols-2 max-wide:grid-cols-1">
        <CostSection runId={runId} runCost={runCost} />
        <ArtifactsPanel runId={runId} />
      </div>
    </>
  );
}
