import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { $api } from '../api/client';
import type { components } from '../api/schema';
import { formatError } from '../api/errors';
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
  Toolbar,
  ToolbarGroup,
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
import { PlaybookParamFields } from './PlaybookParamFields';
import { PackDispatchNotice } from './PackDispatchNotice';
import { DispatchTargetField } from './DispatchTargetField';
import {
  browserTimeZone,
  clampCeiling,
  initialValues,
  launchBody,
  mapServerRejection,
  parseParamsSchema,
  previewMatches,
  runSnapshot,
  scheduleBody,
  validateCronExpr,
  validateMaxTime,
  validateParam,
  type LaunchMode,
  type ParamFieldSpec,
  type Recurrence,
} from './playbookLaunchForm';

type SchedulePreviewDto = components['schemas']['SchedulePreviewDto'];
type ScheduleDto = components['schemas']['ScheduleDto'];

const DEFAULT_MAX_COST = 5;
const DEFAULT_MAX_TIME = '30m';
const DEFAULT_CRON = '0 6 * * MON-FRI';
const PREVIEW_COUNT = 5;

const MODES: readonly { value: LaunchMode; label: string }[] = [
  { value: 'now', label: 'RUN NOW' },
  { value: 'schedule', label: 'ON A SCHEDULE' },
];

function formatFiring(iso: string): string {
  const at = new Date(iso);
  return Number.isNaN(at.getTime()) ? iso : `${iso}  (${at.toLocaleString()})`;
}

/// The launch form: the pack's declared params rendered from its stored schema, plus the ceilings
/// the launcher owns. The endpoint validates the same schema, so everything checked here is a
/// convenience and every refusal it answers with lands back on the input that produced it.
export function PlaybookLaunchPage() {
  const { id = '' } = useParams<{ id: string }>();
  const [search] = useSearchParams();
  const relaunchKey = search.get('relaunch');
  const navigate = useNavigate();
  const playbooks = $api.useQuery('get', '/api/playbooks');
  const schema = $api.useQuery('get', '/api/playbooks/{id}/schema', {
    params: { path: { id } },
  });
  const caps = $api.useQuery('get', '/api/config/playbook-caps');
  const source = $api.useQuery(
    'get',
    '/api/playbook-runs/{key}',
    { params: { path: { key: relaunchKey ?? '' } } },
    { enabled: relaunchKey !== null }
  );
  const launch = $api.useMutation('post', '/api/playbooks/{id}/launch');
  const preview = $api.useMutation('post', '/api/schedules/preview');
  const schedule = $api.useMutation('post', '/api/schedules');

  const parsed = useMemo(
    () => (schema.data === undefined ? null : parseParamsSchema(schema.data)),
    [schema.data]
  );
  const specs: ParamFieldSpec[] = parsed !== null && parsed.kind === 'form' ? parsed.specs : [];
  const snapshot = useMemo(
    () => (source.data === undefined ? null : runSnapshot(source.data.launch)),
    [source.data]
  );
  const awaitingSnapshot = relaunchKey !== null && source.data === undefined && !source.isError;

  const [mode, setMode] = useState<LaunchMode>('now');
  const [values, setValues] = useState<Record<string, string>>({});
  const [fieldErrors, setFieldErrors] = useState<ReadonlyMap<string, string>>(new Map());
  const [maxCost, setMaxCost] = useState(DEFAULT_MAX_COST);
  const [maxTime, setMaxTime] = useState(DEFAULT_MAX_TIME);
  // Blank means "the controller's default", which is what the body omits the field for.
  const [dispatchTarget, setDispatchTarget] = useState('');
  const [dispatchChoiceRequired, setDispatchChoiceRequired] = useState(false);
  const [ceilingErrors, setCeilingErrors] = useState<ReadonlyMap<string, string>>(new Map());
  const [cronExpr, setCronExpr] = useState(DEFAULT_CRON);
  const [tz, setTz] = useState(browserTimeZone);
  const [cronErrors, setCronErrors] = useState<ReadonlyMap<string, string>>(new Map());
  const [firings, setFirings] = useState<SchedulePreviewDto | null>(null);
  const [scheduled, setScheduled] = useState<ScheduleDto | null>(null);
  const [submitError, setSubmitError] = useState<string | null>(null);

  const costCap = caps.data?.max_cost ?? null;
  const timeCap = caps.data?.max_time ?? null;

  useEffect(() => {
    if (parsed === null || parsed.kind !== 'form' || awaitingSnapshot) return;
    setValues(initialValues(parsed.specs, snapshot?.values));
    if (snapshot === null) return;
    setMaxCost(clampCeiling(snapshot.maxCost, costCap));
    setMaxTime(snapshot.maxTime);
  }, [parsed, snapshot, awaitingSnapshot, costCap]);

  useEffect(() => {
    if (costCap === null) return;
    setMaxCost((previous) => clampCeiling(previous, costCap));
  }, [costCap]);

  const pack = playbooks.data?.find((p) => p.id === id);
  const recurrence: Recurrence = { playbook: id, cronExpr, tz };
  const previewed = previewMatches(firings, recurrence);

  const crumbs = [{ label: 'Playbooks', to: '/playbooks' }, { label: id || 'launch' }];

  if (schema.isPending || playbooks.isPending) return <LoadingBlock label="LOADING FORM" />;
  if (pack !== undefined && !pack.actions.includes('launch')) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty title="LAUNCH NOT PERMITTED" description={`Owned by ${pack.owner}`} />
      </>
    );
  }
  if (schema.isError) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty title="NO SUCH PLAYBOOK" description={formatError(schema.error)} />
      </>
    );
  }
  if (parsed === null || parsed.kind === 'unrenderable') {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty
          title="FORM CANNOT BE RENDERED"
          description={
            parsed === null
              ? 'The playbook served no schema.'
              : `${parsed.reason}. Re-register the pack, or launch it from the API.`
          }
        />
      </>
    );
  }

  const setValue = (name: string, value: string) => {
    setValues((previous) => ({ ...previous, [name]: value }));
    setFieldErrors((previous) => {
      if (!previous.has(name)) return previous;
      const next = new Map(previous);
      next.delete(name);
      return next;
    });
  };

  const checkField = (name: string) => {
    const spec = specs.find((s) => s.name === name);
    if (spec === undefined) return;
    const message = validateParam(spec, values[name] ?? '');
    setFieldErrors((previous) => {
      const next = new Map(previous);
      if (message === null) next.delete(name);
      else next.set(name, message);
      return next;
    });
  };

  const checkCeilings = (): boolean => {
    const next = new Map<string, string>();
    if (costCap !== null && maxCost > costCap) {
      next.set('max_cost', `max_cost is above this controller's cap of ${costCap}`);
    }
    if (maxCost <= 0) next.set('max_cost', 'max_cost must be a positive number of USD');
    const time = validateMaxTime(maxTime, timeCap);
    if (time !== null) next.set('max_time', time);
    setCeilingErrors(next);
    return next.size === 0;
  };

  const checkValues = (): boolean => {
    const clientErrors = new Map<string, string>();
    for (const spec of specs) {
      const message = validateParam(spec, values[spec.name] ?? '');
      if (message !== null) clientErrors.set(spec.name, message);
    }
    setFieldErrors(clientErrors);
    const ceilingsOk = checkCeilings();
    return clientErrors.size === 0 && ceilingsOk;
  };

  const applyRejection = (err: unknown) => {
    const rejection = mapServerRejection(err, [
      ...specs.map((s) => s.name),
      'max_cost',
      'max_time',
      'cron_expr',
      'tz',
    ]);
    const params = new Map<string, string>();
    const ceilings = new Map<string, string>();
    const cron = new Map<string, string>();
    for (const [field, message] of rejection.fieldErrors) {
      if (field === 'max_cost' || field === 'max_time') ceilings.set(field, message);
      else if (field === 'cron_expr' || field === 'tz') cron.set(field, message);
      else params.set(field, message);
    }
    setFieldErrors(params);
    setCeilingErrors(ceilings);
    setCronErrors(cron);
    setSubmitError(rejection.general);
  };

  const clearPreview = () => {
    setFirings(null);
    setCronErrors(new Map());
  };

  const runPreview = async () => {
    setSubmitError(null);
    const shape = validateCronExpr(cronExpr);
    if (shape !== null) {
      setCronErrors(new Map([['cron_expr', shape]]));
      return;
    }
    try {
      const dto = await preview.mutateAsync({
        body: { cron_expr: cronExpr.trim(), tz: tz.trim(), count: PREVIEW_COUNT },
      });
      setCronErrors(new Map());
      setFirings(dto);
    } catch (err: unknown) {
      setFirings(null);
      applyRejection(err);
    }
  };

  const handleLaunch = async () => {
    setSubmitError(null);
    if (!checkValues()) return;
    if (dispatchChoiceRequired && dispatchTarget === '') {
      setSubmitError('Choose a cluster for this run.');
      return;
    }
    try {
      const ack = await launch.mutateAsync({
        params: { path: { id } },
        body: launchBody(
          specs,
          values,
          { maxCost, maxTime, schemaDigest: pack?.schema_digest ?? '', dispatchTarget }
        ),
      });
      void navigate(`/issues/${encodeURIComponent(ack.key)}`);
    } catch (err: unknown) {
      applyRejection(err);
    }
  };

  const handleSchedule = async () => {
    setSubmitError(null);
    setScheduled(null);
    if (!checkValues()) return;
    if (dispatchChoiceRequired && dispatchTarget === '') {
      setSubmitError('Choose a cluster for this schedule.');
      return;
    }
    const shape = validateCronExpr(cronExpr);
    if (shape !== null) {
      setCronErrors(new Map([['cron_expr', shape]]));
      return;
    }
    try {
      const stored = await schedule.mutateAsync({
        body: scheduleBody(
          specs,
          values,
          { maxCost, maxTime, schemaDigest: pack?.schema_digest ?? '', dispatchTarget },
          recurrence
        ),
      });
      setScheduled(stored);
    } catch (err: unknown) {
      applyRejection(err);
    }
  };

  return (
    <>
      <Breadcrumb items={crumbs} />
      <PageHeader
        eyebrow="Queue"
        title={`Launch ${id}`}
        description={pack?.description ?? 'Run a registered playbook once, with the values you supply.'}
      />

      {relaunchKey !== null && (
        <Section>
          {source.isError ? (
            <Notice label="Snapshot unread">
              {`No snapshot for ${relaunchKey}: ${formatError(source.error)}. The form holds the pack's defaults.`}
            </Notice>
          ) : (
            <Notice label={source.data?.launch.schema_drifted === true ? 'Schema drifted' : 'Relaunch'}>
              {source.data?.launch.schema_drifted === true
                ? `Prefilled from ${relaunchKey}, authorized against ${source.data.launch.schema_digest}. The pack has been re-pinned since, so these values may no longer satisfy it.`
                : `Prefilled from ${relaunchKey}. Every value is editable before this fires.`}
            </Notice>
          )}
        </Section>
      )}

      <Toolbar>
        <ToolbarGroup label="Fire" options={MODES} value={mode} onChange={setMode} />
      </Toolbar>

      {pack === undefined ? null : <PackDispatchNotice dispatch={pack.dispatch} />}

      <Section>
        <SectionHeader
          title="Parameters"
          actions={
            pack === undefined ? undefined : (
              <Mono size="data" tone="ink-3">
                {pack.schema_digest}
              </Mono>
            )
          }
        />
        <SectionBody>
          {specs.length === 0 ? (
            <Empty title="NO PARAMETERS" description="This pack declares none; launch it as is." />
          ) : (
            <PlaybookParamFields
              idPrefix="launch"
              specs={specs}
              values={values}
              errors={fieldErrors}
              onChange={setValue}
              onBlur={checkField}
            />
          )}
        </SectionBody>
      </Section>

      <Section>
        <SectionHeader title="Ceilings" />
        <Notice label="Launcher-owned">
          These bound the run and are not the pack&apos;s to set. They cannot exceed this
          deploy&apos;s caps
          {costCap === null || timeCap === null ? '' : ` (${costCap} USD, ${timeCap})`}.
        </Notice>
        <SectionBody>
          <FormGrid>
            <NumberInputField
              id="launch-max-cost"
              label="Max cost (USD)"
              value={maxCost}
              min={0}
              max={costCap ?? undefined}
              step={0.5}
              onChange={(next) => {
                setMaxCost(clampCeiling(next, costCap));
                setCeilingErrors(new Map());
              }}
              error={ceilingErrors.get('max_cost') ?? null}
              hint={costCap === null ? undefined : `Capped at $${costCap} per run.`}
            />
            <DispatchTargetField
              id="launch-dispatch-target"
              playbook={id}
              value={dispatchTarget}
              onChange={(next) => {
                setDispatchTarget(next);
                setSubmitError(null);
              }}
              onChoiceRequired={setDispatchChoiceRequired}
            />
            <TextField
              id="launch-max-time"
              label="Max time"
              mono
              value={maxTime}
              onChange={(next) => {
                setMaxTime(next);
                setCeilingErrors(new Map());
              }}
              onBlur={() => {
                checkCeilings();
              }}
              error={ceilingErrors.get('max_time') ?? null}
              hint={`Wall clock: 90s, 30m, 2h${timeCap === null ? '' : `. Capped at ${timeCap}.`}`}
            />
          </FormGrid>
        </SectionBody>
      </Section>

      {mode === 'schedule' && (
        <Section>
          <SectionHeader title="Recurrence" />
          <Notice label="Preview first">
            The firings are computed server-side, in the zone below, by the sweep that will run
            them. Preview them before this schedule is stored.
          </Notice>
          <SectionBody>
            <FormGrid>
              <TextField
                id="launch-cron-expr"
                label="Cron expression"
                mono
                required
                value={cronExpr}
                onChange={(next) => {
                  setCronExpr(next);
                  clearPreview();
                }}
                error={cronErrors.get('cron_expr') ?? null}
                hint="Five fields: minute hour day month weekday. 0 6 * * MON-FRI is 06:00 on weekdays."
              />
              <TextField
                id="launch-tz"
                label="Time zone"
                mono
                required
                value={tz}
                onChange={(next) => {
                  setTz(next);
                  clearPreview();
                }}
                error={cronErrors.get('tz') ?? null}
                hint="IANA zone: UTC, America/New_York."
              />
              <div className="grid gap-2">
                <div>
                  <Button
                    className="border border-rule-hard"
                    onClick={() => void runPreview()}
                    disabled={preview.isPending}
                  >
                    {preview.isPending ? 'PREVIEWING…' : 'PREVIEW FIRINGS'}
                  </Button>
                </div>
                {previewed && firings !== null ? (
                  <Note>
                    {firings.firings.length === 0 ? (
                      'This expression has no firing ahead.'
                    ) : (
                      <ol className="m-0 grid list-none gap-0.5 p-0 font-mono text-data">
                        {firings.firings.map((at) => (
                          <li key={at}>{formatFiring(at)}</li>
                        ))}
                      </ol>
                    )}
                  </Note>
                ) : (
                  <Note>The next firings appear here once previewed.</Note>
                )}
              </div>
            </FormGrid>
          </SectionBody>
        </Section>
      )}

      {scheduled !== null && (
        <Section>
          <SectionHeader title="Scheduled" />
          <SectionBody>
            <Note>
              {`${scheduled.id} fires ${scheduled.cron_expr} (${scheduled.tz}); next at ${scheduled.next_due_at ?? 'no further occurrence'}.`}
            </Note>
          </SectionBody>
        </Section>
      )}

      {submitError !== null && (
        <SectionBody>
          <FormError>{submitError}</FormError>
        </SectionBody>
      )}

      <FormActions>
        {mode === 'now' ? (
          <Button variant="filled" onClick={() => void handleLaunch()} disabled={launch.isPending}>
            {launch.isPending ? 'LAUNCHING…' : 'LAUNCH'}
          </Button>
        ) : (
          <>
            <Button
              variant="filled"
              onClick={() => void handleSchedule()}
              disabled={schedule.isPending || !previewed}
            >
              {schedule.isPending ? 'SCHEDULING…' : 'SCHEDULE'}
            </Button>
            {!previewed && (
              <Mono size="data" tone="ink-3">
                Preview the firings to enable this.
              </Mono>
            )}
          </>
        )}
        <Button render={<Link to="/playbooks" />}>CANCEL</Button>
      </FormActions>
    </>
  );
}
