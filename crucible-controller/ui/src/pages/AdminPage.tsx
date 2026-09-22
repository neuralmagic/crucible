import { useEffect, useState } from 'react';
import { AlertDialog } from '@base-ui-components/react/alert-dialog';
import { Dialog } from '@base-ui-components/react/dialog';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { AutopilotToggle } from '../AutopilotToggle';
import { formatError } from '../api/errors';
import { useLiveEvents } from '../api/useLiveEvents';
import type { components } from '../api/schema.d';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  DIALOG_BACKDROP,
  DIALOG_TITLE,
  Empty,
  LoadingBlock,
  Mono,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
  Spinner,
  Status,
  statusTone,
  useDataTable,
} from '../ui';
import type { StatusTone } from '../ui';
import {
  ALL_TIERS,
  DEPRECATED_KNOBS,
  KNOB_KINDS,
  currentOverrideSet,
  formatKnobValue,
  isControlPlaneEvent,
  isKnobKey,
  withKnobValue,
  type KnobKind,
} from './adminConfig';
import {
  CheckField,
  FormError,
  FormGrid,
  Note,
  NumberInputField,
  TextAreaField,
  TextField,
} from './formControls';
import { issueStatusColor } from './issueStatus';

type KnobView = components['schemas']['KnobView'];
type ConfigOverridesBody = components['schemas']['ConfigOverridesBody'];
type Source = components['schemas']['Source'];
type RepoRow = components['schemas']['RepoHealthDto'];
type EventDto = components['schemas']['EventDto'];

const SOURCE_TONE: Record<Source, StatusTone> = {
  default: 'grey',
  env: 'blue',
  override: 'amber',
};

const DIALOG_POPUP =
  'fixed top-1/2 left-1/2 z-40 max-h-[85vh] w-[min(72ch,92vw)] -translate-x-1/2 -translate-y-1/2 overflow-y-auto border border-rule-hard bg-surface';

// --- feature flags & overrides -------------------------------------------------

function KnobEditDialog({ knob, allKnobs, onClose }: { knob: KnobView; allKnobs: KnobView[]; onClose: () => void }) {
  const qc = useQueryClient();
  const mutation = $api.useMutation('put', '/api/config/overrides');

  const kind: KnobKind | null = isKnobKey(knob.name) ? KNOB_KINDS[knob.name] : null;
  const rawValue: unknown = knob.value;
  const [boolVal, setBoolVal] = useState<boolean>(typeof rawValue === 'boolean' ? rawValue : false);
  const [numVal, setNumVal] = useState<number>(typeof rawValue === 'number' ? rawValue : 0);
  const [tiersVal, setTiersVal] = useState<string[]>(
    Array.isArray(rawValue) ? rawValue.filter((t): t is string => typeof t === 'string') : [],
  );
  const [justification, setJustification] = useState('');
  const [submitError, setSubmitError] = useState<string | null>(null);

  const canSubmit =
    kind !== null &&
    justification.trim().length > 0 &&
    (kind !== 'tiers' || tiersVal.length > 0) &&
    !mutation.isPending;

  const handleSubmit = async () => {
    setSubmitError(null);
    if (!isKnobKey(knob.name) || kind === null) return;
    const value: number | boolean | string[] =
      kind === 'bool' ? boolVal : kind === 'tiers' ? tiersVal : numVal;
    const nextSet = withKnobValue(currentOverrideSet(allKnobs), knob.name, value);
    const body: ConfigOverridesBody = { ...nextSet, justification: justification.trim() };
    try {
      await mutation.mutateAsync({ body });
      await Promise.all([
        qc.invalidateQueries({ queryKey: ['get', '/api/config'] }),
        qc.invalidateQueries({ queryKey: ['get', '/api/events'] }),
      ]);
      onClose();
    } catch (err: unknown) {
      setSubmitError(formatError(err));
    }
  };

  return (
    <Dialog.Root
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <Dialog.Portal>
        <Dialog.Backdrop className={DIALOG_BACKDROP} />
        <Dialog.Popup className={DIALOG_POPUP} aria-label={`Edit ${knob.name}`}>
          <Dialog.Title className={DIALOG_TITLE}>Override {knob.name}</Dialog.Title>
          <div className="px-4 py-3.5">
            <Dialog.Description className="mt-0 mb-3.5 max-w-[70ch] text-ink-2">
              {knob.description}
            </Dialog.Description>
            {kind === null || !knob.overridable ? (
              <Note>This knob is not runtime-overridable.</Note>
            ) : (
              <FormGrid>
                {kind === 'bool' ? (
                  <CheckField
                    id="knob-value"
                    label={boolVal ? 'true' : 'false'}
                    checked={boolVal}
                    onChange={setBoolVal}
                  />
                ) : kind === 'tiers' ? (
                  <div className="grid gap-1.5">
                    <span className="font-mono text-label font-semibold uppercase tracking-group text-ink-3">
                      Value
                    </span>
                    <div className="flex w-fit border border-rule-hard [&>*]:border-r [&>*]:border-rule [&>*:last-child]:border-r-0">
                      {ALL_TIERS.map((tier) => (
                        <Button
                          key={tier}
                          selected={tiersVal.includes(tier)}
                          onClick={() => {
                            setTiersVal((prev) =>
                              prev.includes(tier)
                                ? prev.filter((t) => t !== tier)
                                : ALL_TIERS.filter((t) => prev.includes(t) || t === tier),
                            );
                          }}
                        >
                          {tier}
                        </Button>
                      ))}
                    </div>
                  </div>
                ) : (
                  <NumberInputField
                    id="knob-value"
                    label="Value"
                    value={numVal}
                    onChange={setNumVal}
                    min={0}
                    step={kind === 'int' ? 1 : 0.5}
                  />
                )}
                <TextAreaField
                  id="knob-justification"
                  label="Justification"
                  required
                  value={justification}
                  onChange={setJustification}
                  rows={3}
                  placeholder="Why this override, and for how long?"
                  autoFocus
                  hint="Recorded verbatim in the audit event."
                />
              </FormGrid>
            )}
            {submitError !== null && <FormError className="mt-3.5">{submitError}</FormError>}
          </div>
          <div className="flex justify-end border-t border-rule">
            <Button onClick={onClose}>CANCEL</Button>
            <Button variant="filled" onClick={() => void handleSubmit()} disabled={!canSubmit}>
              {mutation.isPending ? 'WRITING…' : 'WRITE OVERRIDE'}
            </Button>
          </div>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

const knobHelper = createDataColumnHelper<KnobView>();

function knobColumns(isAdmin: boolean, onEdit: (knob: KnobView) => void) {
  return knobHelper.columns([
    knobHelper.accessor('name', {
      header: 'Knob',
      enableSorting: false,
      meta: { shrink: true },
      cell: ({ row }) => (
        <span className="flex items-center gap-2">
          <Mono weight="semibold" tone="ink">
            {row.original.name}
          </Mono>
          {isKnobKey(row.original.name) && DEPRECATED_KNOBS.has(row.original.name) && (
            <Mono size="micro" uppercase tone="amber" className="border border-amber px-1">
              deprecated
            </Mono>
          )}
        </span>
      ),
    }),
    knobHelper.accessor('value', {
      header: 'Value',
      enableSorting: false,
      meta: { shrink: true, align: 'end' },
      cell: ({ getValue }) => <Mono weight="semibold">{formatKnobValue(getValue())}</Mono>,
    }),
    knobHelper.accessor('source', {
      header: 'Source',
      enableSorting: false,
      meta: { shrink: true },
      cell: ({ getValue }) => <Status status={getValue()} tone={SOURCE_TONE[getValue()]} />,
    }),
    knobHelper.accessor('description', {
      header: 'Description',
      enableSorting: false,
      meta: { wrap: true, className: 'text-ink-2' },
    }),
    knobHelper.display({
      id: 'actions',
      header: '',
      meta: { shrink: true, align: 'end' },
      cell: ({ row }) =>
        isAdmin ? (
          <Button
            className="border border-rule-hard px-2.5"
            disabled={!row.original.overridable}
            onClick={() => {
              onEdit(row.original);
            }}
          >
            EDIT
          </Button>
        ) : null,
    }),
  ]);
}

function KnobsTable({ knobs, isAdmin, onEdit }: { knobs: KnobView[]; isAdmin: boolean; onEdit: (knob: KnobView) => void }) {
  const columns = knobColumns(isAdmin, onEdit);
  const table = useDataTable({ columns, data: knobs, getRowId: (knob) => knob.name });
  return <DataTable table={table} />;
}

function FlagsSection({ isAdmin }: { isAdmin: boolean }) {
  const config = $api.useQuery('get', '/api/config');
  const [editing, setEditing] = useState<KnobView | null>(null);

  return (
    <Section>
      <SectionHeader title="Feature flags & overrides" note="default < env < override" />
      <SectionBody>
        <p className="m-0 max-w-[80ch] text-ink-2">
          Every runtime knob with its effective value and where it came from. Overrides are
          ConfigMap-backed and replace the whole set on write.
        </p>
        {!isAdmin && <Note className="mt-3.5">Admin access required to edit overrides.</Note>}
      </SectionBody>
      {config.isError ? (
        <Empty title="CONFIG UNAVAILABLE" description={formatError(config.error)} />
      ) : config.isPending ? (
        <LoadingBlock label="LOADING CONFIG" />
      ) : (
        <KnobsTable knobs={config.data.knobs} isAdmin={isAdmin} onEdit={setEditing} />
      )}
      {editing && (
        <KnobEditDialog
          key={editing.name}
          knob={editing}
          allKnobs={config.data?.knobs ?? []}
          onClose={() => {
            setEditing(null);
          }}
        />
      )}
    </Section>
  );
}

// --- repos / onboarding --------------------------------------------------------

function AddRepoForm({ allowedOrgs }: { allowedOrgs: string[] }) {
  const qc = useQueryClient();
  const mutation = $api.useMutation('post', '/api/repos');
  const [repo, setRepo] = useState('');
  const [justification, setJustification] = useState('');
  const [error, setError] = useState<string | null>(null);

  const canSubmit = repo.trim().length > 0 && justification.trim().length > 0 && !mutation.isPending;

  const handleSubmit = async () => {
    setError(null);
    try {
      await mutation.mutateAsync({ body: { repo: repo.trim(), justification: justification.trim() } });
      await Promise.all([
        qc.invalidateQueries({ queryKey: ['get', '/api/repos'] }),
        qc.invalidateQueries({ queryKey: ['get', '/api/events'] }),
      ]);
      setRepo('');
      setJustification('');
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  return (
    <FormGrid>
      <TextField
        id="add-repo"
        label="Repository"
        required
        value={repo}
        onChange={setRepo}
        placeholder="org/name"
        mono
        hint={
          allowedOrgs.length > 0 ? (
            <span className="flex flex-wrap items-center gap-1.5">
              Allowed orgs:
              {allowedOrgs.map((org) => (
                <Mono key={org} className="border border-rule-hard px-1">
                  {org}
                </Mono>
              ))}
            </span>
          ) : (
            'No orgs are whitelisted — new-repo adds are locked closed (set CONTROLLER_ALLOWED_ORGS).'
          )
        }
      />
      <TextAreaField
        id="add-repo-justification"
        label="Justification"
        required
        value={justification}
        onChange={setJustification}
        rows={2}
        placeholder="Why onboard this repo?"
      />
      {error !== null && <FormError>{error}</FormError>}
      <div>
        <Button variant="filled" onClick={() => void handleSubmit()} disabled={!canSubmit}>
          {mutation.isPending ? 'ADDING…' : 'ADD REPO'}
        </Button>
      </div>
    </FormGrid>
  );
}

const repoHelper = createDataColumnHelper<RepoRow>();

interface RepoActions {
  isAdmin: boolean;
  busy: boolean;
  onPause: (repo: string) => void;
  onResume: (repo: string) => void;
  onUnwatch: (repo: string) => void;
}

function countColumn(id: 'new' | 'scoped' | 'awaiting_approval' | 'running' | 'pr_open' | 'parked' | 'done' | 'total', header: string) {
  return repoHelper.accessor(id, {
    header,
    enableSorting: false,
    meta: { shrink: true, align: 'end', className: 'font-mono text-data' },
  });
}

function repoColumns(actions: RepoActions) {
  return repoHelper.columns([
    repoHelper.accessor('repo', {
      header: 'Repo',
      enableSorting: false,
      meta: { shrink: true },
      cell: ({ getValue }) => (
        <Mono weight="semibold" tone="ink">
          {getValue()}
        </Mono>
      ),
    }),
    repoHelper.display({
      id: 'state',
      header: 'State',
      meta: { shrink: true },
      cell: ({ row }) => (
        <span className="flex items-center gap-2">
          <Status
            status={row.original.watched ? 'watched' : 'unwatched'}
            tone={row.original.watched ? 'green' : 'grey'}
          />
          {row.original.paused && <Status status="paused" tone="amber" />}
        </span>
      ),
    }),
    repoHelper.accessor('added_by', {
      header: 'Added by',
      enableSorting: false,
      meta: { shrink: true, hideNarrow: true },
      cell: ({ row }) => (
        <Mono title={row.original.added_at ?? undefined}>{row.original.added_by ?? '—'}</Mono>
      ),
    }),
    countColumn('new', 'New'),
    countColumn('scoped', 'Scoped'),
    countColumn('awaiting_approval', 'Await'),
    countColumn('running', 'Running'),
    countColumn('pr_open', 'PR'),
    countColumn('parked', 'Parked'),
    countColumn('done', 'Done'),
    countColumn('total', 'Total'),
    repoHelper.accessor('watermark', {
      header: 'Watermark',
      enableSorting: false,
      meta: { shrink: true, hideNarrow: true },
      cell: ({ getValue }) => <Mono tone="ink-3">{getValue() || '—'}</Mono>,
    }),
    repoHelper.display({
      id: 'actions',
      header: '',
      meta: { shrink: true, align: 'end' },
      cell: ({ row }) => {
        if (!actions.isAdmin) return null;
        const repo = row.original.repo;
        return (
          <span className="flex justify-end gap-1.5">
            {row.original.paused ? (
              <Button
                className="border border-rule-hard px-2.5"
                disabled={actions.busy}
                onClick={() => {
                  actions.onResume(repo);
                }}
              >
                RESUME
              </Button>
            ) : (
              <Button
                className="border border-rule-hard px-2.5"
                disabled={actions.busy}
                onClick={() => {
                  actions.onPause(repo);
                }}
              >
                PAUSE
              </Button>
            )}
            <Button
              className="border border-red px-2.5 text-red hover:bg-red hover:text-surface"
              onClick={() => {
                actions.onUnwatch(repo);
              }}
            >
              UNWATCH
            </Button>
          </span>
        );
      },
    }),
  ]);
}

function ReposTable({ repos, actions }: { repos: RepoRow[]; actions: RepoActions }) {
  const columns = repoColumns(actions);
  const table = useDataTable({ columns, data: repos, getRowId: (row) => row.repo });
  return (
    <DataTable
      table={table}
      empty={<Empty title="NO REPOSITORIES" description="No repositories tracked yet." />}
    />
  );
}

function ReposSection({ isAdmin }: { isAdmin: boolean }) {
  const qc = useQueryClient();
  const repos = $api.useQuery('get', '/api/repos');
  const access = $api.useQuery('get', '/api/access');
  const pause = $api.useMutation('post', '/api/repos/{repo}/pause');
  const resume = $api.useMutation('post', '/api/repos/{repo}/resume');
  const unwatch = $api.useMutation('delete', '/api/repos/{repo}');
  const [confirmUnwatch, setConfirmUnwatch] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const refresh = () => qc.invalidateQueries({ queryKey: ['get', '/api/repos'] });

  const runAction = async (fn: () => Promise<unknown>) => {
    setActionError(null);
    try {
      await fn();
      await refresh();
    } catch (err: unknown) {
      setActionError(formatError(err));
    }
  };

  const actions: RepoActions = {
    isAdmin,
    busy: pause.isPending || resume.isPending,
    onPause: (repo) => void runAction(() => pause.mutateAsync({ params: { path: { repo } } })),
    onResume: (repo) => void runAction(() => resume.mutateAsync({ params: { path: { repo } } })),
    onUnwatch: setConfirmUnwatch,
  };

  return (
    <Section>
      <SectionHeader title="Repos & onboarding" note="the watch set discovery sweeps" />
      <SectionBody>
        <p className="m-0 max-w-[80ch] text-ink-2">
          Add a repo (org must be whitelisted, must exist on GitHub); pause to stop discovery without
          losing its rows; unwatch to drop it from the sweep (rows and issues are kept).
        </p>
        <div className="mt-3.5">
          {isAdmin ? (
            <AddRepoForm allowedOrgs={access.data?.allowed_orgs ?? []} />
          ) : (
            <Note>Admin access required to add or manage repos.</Note>
          )}
        </div>
        {actionError !== null && <FormError className="mt-3.5">{actionError}</FormError>}
      </SectionBody>

      {repos.isError ? (
        <Empty title="FAILED TO LOAD REPOS" description={formatError(repos.error)} />
      ) : repos.isPending ? (
        <LoadingBlock label="LOADING REPOS" />
      ) : (
        <ReposTable repos={repos.data} actions={actions} />
      )}

      <AlertDialog.Root
        open={confirmUnwatch !== null}
        onOpenChange={(open) => {
          if (!open) setConfirmUnwatch(null);
        }}
      >
        <AlertDialog.Portal>
          <AlertDialog.Backdrop className={DIALOG_BACKDROP} />
          <AlertDialog.Popup className={DIALOG_POPUP}>
            <AlertDialog.Title className={DIALOG_TITLE}>Unwatch repository?</AlertDialog.Title>
            <AlertDialog.Description className="m-0 px-4 py-3.5 text-ink-2">
              Discovery stops for <Mono weight="semibold" tone="ink">{confirmUnwatch}</Mono>. Its
              rows and issues are kept — this only removes it from the sweep. You can add it back
              later.
            </AlertDialog.Description>
            <div className="flex justify-end border-t border-rule">
              <Button
                onClick={() => {
                  setConfirmUnwatch(null);
                }}
              >
                CANCEL
              </Button>
              <Button
                variant="filled"
                className="bg-red"
                disabled={unwatch.isPending}
                onClick={() => {
                  const repo = confirmUnwatch;
                  if (repo === null) return;
                  void runAction(() => unwatch.mutateAsync({ params: { path: { repo } } })).then(() =>
                    setConfirmUnwatch(null),
                  );
                }}
              >
                {unwatch.isPending ? 'UNWATCHING…' : 'UNWATCH'}
              </Button>
            </div>
          </AlertDialog.Popup>
        </AlertDialog.Portal>
      </AlertDialog.Root>
    </Section>
  );
}

// --- autopilot -----------------------------------------------------------------

function AutopilotSection({ isAdmin }: { isAdmin: boolean }) {
  const autopilot = $api.useQuery('get', '/api/autopilot');

  if (isAdmin) return <AutopilotToggle />;

  return (
    <Section>
      <SectionHeader title="Autopilot" />
      {autopilot.isPending ? (
        <LoadingBlock label="LOADING AUTOPILOT" />
      ) : autopilot.isError ? (
        <Empty title="AUTOPILOT UNAVAILABLE" description={formatError(autopilot.error)} />
      ) : (
        <SectionBody className="grid gap-3">
          <Status
            status={autopilot.data.enabled ? 'enabled' : 'disabled'}
            tone={autopilot.data.enabled ? 'green' : 'amber'}
          />
          {autopilot.data.changed_by && (
            <div className="grid gap-1">
              <Mono>
                changed by{' '}
                <Mono weight="semibold" tone="ink">
                  {autopilot.data.changed_by}
                </Mono>
              </Mono>
              {autopilot.data.reason && <Mono>reason {autopilot.data.reason}</Mono>}
            </div>
          )}
          <Note>Admin access required to flip the kill switch.</Note>
        </SectionBody>
      )}
    </Section>
  );
}

// --- manual reconcile ------------------------------------------------------------

function ReconcileSection({ isAdmin }: { isAdmin: boolean }) {
  const qc = useQueryClient();
  const mutation = $api.useMutation('post', '/api/reconcile');
  const [error, setError] = useState<string | null>(null);
  const [acked, setAcked] = useState(false);

  useEffect(() => {
    if (!acked) return;
    const t = window.setTimeout(() => setAcked(false), 4000);
    return () => window.clearTimeout(t);
  }, [acked]);

  const handleClick = async () => {
    setError(null);
    try {
      await mutation.mutateAsync({});
      setAcked(true);
      await qc.invalidateQueries({ queryKey: ['get', '/api/events'] });
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  return (
    <Section>
      <SectionHeader title="Reconcile" note="5 minute discovery cadence" />
      <SectionBody>
        <p className="m-0 max-w-[80ch] text-ink-2">
          After flipping a knob, forcing a re-rank, or fixing creds, kick the controller here instead
          of waiting: one discovery sweep plus a full re-enqueue of every non-terminal issue, run
          immediately. Idempotent — repeat clicks while a pass runs coalesce into one.
        </p>
        <div className="mt-3.5">
          {isAdmin ? (
            <span className="flex flex-wrap items-center gap-3.5">
              <Button
                variant="filled"
                disabled={mutation.isPending}
                onClick={() => void handleClick()}
              >
                {mutation.isPending ? 'RECONCILING…' : 'RECONCILE NOW'}
              </Button>
              {acked && (
                <Mono size="label" uppercase tone="green" weight="semibold">
                  triggered — sweep running in the daemon
                </Mono>
              )}
            </span>
          ) : (
            <Note>Admin access required to force a reconcile.</Note>
          )}
        </div>
        {error !== null && <FormError className="mt-3.5">{error}</FormError>}
      </SectionBody>
    </Section>
  );
}

// --- access & audit ------------------------------------------------------------

function LoginList({ title, logins }: { title: string; logins: string[] }) {
  return (
    <div className="flex flex-wrap items-baseline gap-2.5">
      <Mono size="label" uppercase weight="semibold" tone="ink-3" className="w-20 tracking-group">
        {title}
      </Mono>
      {logins.length === 0 ? (
        <Mono tone="ink-3">none (locked closed)</Mono>
      ) : (
        logins.map((login) => (
          <Mono key={login} className="border border-rule-hard px-1">
            {login}
          </Mono>
        ))
      )}
    </div>
  );
}

const activityHelper = createDataColumnHelper<EventDto>();

const activityColumns = activityHelper.columns([
  activityHelper.accessor('key', {
    header: 'Identifier',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => (
      <Mono weight="semibold" tone="ink">
        {getValue()}
      </Mono>
    ),
  }),
  activityHelper.display({
    id: 'transition',
    header: 'Transition',
    meta: { shrink: true },
    cell: ({ row }) => (
      <span className="flex items-center gap-2">
        <Status status={row.original.from} tone={statusTone(issueStatusColor(row.original.from))} />
        <Mono tone="ink-3">→</Mono>
        <Status status={row.original.to} tone={statusTone(issueStatusColor(row.original.to))} />
      </span>
    ),
  }),
  activityHelper.display({
    id: 'reason',
    header: 'Reason',
    meta: { wrap: true, className: 'text-ink-2' },
    cell: ({ row }) => row.original.reason?.text ?? '—',
  }),
  activityHelper.accessor('actor', {
    header: 'Actor',
    enableSorting: false,
    meta: { shrink: true },
    cell: ({ getValue }) => <Mono>{getValue() || '—'}</Mono>,
  }),
  activityHelper.accessor('ts', {
    header: 'When',
    enableSorting: false,
    meta: { shrink: true, align: 'end' },
    cell: ({ getValue }) => <Mono tone="ink-3">{new Date(getValue()).toLocaleString()}</Mono>,
  }),
]);

function ActivityTable({ events }: { events: EventDto[] }) {
  const table = useDataTable({
    columns: activityColumns,
    data: events,
    getRowId: (event, index) => `${event.ts}-${event.key}-${index}`,
  });
  return (
    <DataTable
      table={table}
      empty={<Empty title="NO ACTIVITY" description="Nothing recorded yet." />}
    />
  );
}

function AccessSection() {
  const access = $api.useQuery('get', '/api/access');
  const events = $api.useQuery('get', '/api/events', { params: { query: { limit: 200 } } });

  const activity = (events.data ?? []).filter(isControlPlaneEvent).slice(0, 30);

  return (
    <Section>
      <SectionHeader title="Access & audit" note="whitelists are pinned at deploy" />
      <SectionBody>
        <p className="m-0 max-w-[80ch] text-ink-2">
          Role whitelists come from the environment. Below them, the recent control-plane activity:
          config overrides, repo watch-set changes, autopilot flips, and human-initiated issue
          overrides.
        </p>
        {access.isError ? (
          <Note className="mt-3.5">Failed to load access lists ({formatError(access.error)}).</Note>
        ) : access.isPending ? (
          <div className="mt-3.5">
            <Spinner />
          </div>
        ) : (
          <div className="mt-3.5 grid gap-2">
            <LoginList title="Admins" logins={access.data.admins} />
            <LoginList title="Operators" logins={access.data.operators} />
          </div>
        )}
      </SectionBody>

      {events.isError ? (
        <Empty title="FAILED TO LOAD ACTIVITY" description={formatError(events.error)} />
      ) : events.isPending ? (
        <LoadingBlock label="LOADING ACTIVITY" />
      ) : (
        <ActivityTable events={activity} />
      )}
    </Section>
  );
}

// --- page ----------------------------------------------------------------------

export function AdminPage() {
  useLiveEvents();
  const whoami = $api.useQuery('get', '/api/whoami');

  if (whoami.isError) {
    return <Empty title="ADMIN UNAVAILABLE" description={formatError(whoami.error)} />;
  }

  if (whoami.isPending) {
    return <LoadingBlock label="LOADING ADMIN" />;
  }

  const isAdmin = whoami.data.role === 'admin';

  return (
    <>
      <PageHeader
        eyebrow="System"
        title="Admin"
        description={
          isAdmin
            ? `Signed in as ${whoami.data.user ?? 'unknown'} (admin). Mutating actions book to your login in the audit log.`
            : 'Read-only view. Mutating sections require admin access.'
        }
      />
      <FlagsSection isAdmin={isAdmin} />
      <ReposSection isAdmin={isAdmin} />
      <AutopilotSection isAdmin={isAdmin} />
      <ReconcileSection isAdmin={isAdmin} />
      <AccessSection />
    </>
  );
}
