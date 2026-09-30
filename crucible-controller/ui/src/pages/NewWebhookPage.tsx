import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
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
} from '../ui';
import {
  FormActions,
  FormError,
  FormGrid,
  MetaRow,
  NumberInputField,
  SelectField,
  TextAreaField,
  TextField,
} from './formControls';
import { mapServerRejection, parseParamsSchema, spellingOf, type ParamFieldSpec } from './playbookLaunchForm';
import { SecretOnce } from './SecretOnce';
import { CelEditor, useCelCheck } from './CelEditor';
import { byField, celCompletions, type CelDiagnosticDto } from './celTooling';
import {
  applyPreset,
  createBody,
  headerLines,
  initialState,
  previewBody,
  stateFromWebhook,
  type ParamChoice,
  type Verifier,
  type WebhookDto,
  type WebhookFormState,
} from './webhookForm';

type WebhookSecretDto = components['schemas']['WebhookSecretDto'];
type WebhookPreviewDto = components['schemas']['WebhookPreviewDto'];
type PreviewResultDto = components['schemas']['PreviewResultDto'];

const VERIFIERS: readonly { value: Verifier; label: string }[] = [
  { value: 'path_token', label: 'path_token: a token in the URL (quay.io)' },
  { value: 'hmac_sha256', label: 'hmac_sha256: a signed body (GitHub)' },
];

const MODES = [
  { value: 'fixed', label: 'fixed value' },
  { value: 'derived', label: 'derived (CEL)' },
];

const SAMPLE_QUAY = JSON.stringify(
  {
    repository: 'org/img',
    docker_url: 'quay.io/org/img',
    updated_tags: ['latest'],
  },
  null,
  2
);

const NO_DIAGNOSTICS: readonly CelDiagnosticDto[] = [];

function FieldNote({ error }: { error: string | undefined }) {
  if (error === undefined) return null;
  return (
    <Mono size="data" tone="red">
      {error}
    </Mono>
  );
}

function Result({ label, result }: { label: string; result: PreviewResultDto }) {
  return (
    <MetaRow label={label}>
      {result.error ? (
        <Mono size="data" tone="red">
          {result.error}
        </Mono>
      ) : (
        <pre className="m-0 overflow-x-auto font-mono text-data text-green">
          {JSON.stringify(result.value, null, 2)}
        </pre>
      )}
    </MetaRow>
  );
}

interface ParamRowProps {
  spec: ParamFieldSpec;
  choice: ParamChoice;
  onChange: (choice: ParamChoice) => void;
  error: string | undefined;
  diagnostics: readonly CelDiagnosticDto[];
  completions: ReturnType<typeof celCompletions> | undefined;
}

function ParamRow({ spec, choice, onChange, error, diagnostics, completions }: ParamRowProps) {
  const spelling = spellingOf(spec.valueType);
  return (
    <div className="grid gap-2 border-l-2 border-rule pl-3">
      <SelectField
        id={`mode-${spec.name}`}
        label={
          <>
            {spec.name}
            {spec.required ? ' (required)' : ''}
            {spelling !== null ? ` · ${spelling}` : ''}
          </>
        }
        value={choice.mode}
        onChange={(mode) => {
          onChange({ ...choice, mode: mode === 'derived' ? 'derived' : 'fixed' });
        }}
        options={MODES}
        hint={spec.description ?? undefined}
      />
      {choice.mode === 'derived' ? (
        <>
          <CelEditor
            field={`derive.${spec.name}`}
            label="Expression"
            value={choice.expression}
            onChange={(expression) => {
              onChange({ ...choice, expression });
            }}
            diagnostics={diagnostics}
            completions={completions}
          />
          <FieldNote error={error} />
        </>
      ) : (
        <TextField
          id={`value-${spec.name}`}
          label="Value"
          mono
          value={choice.value}
          onChange={(value) => {
            onChange({ ...choice, value });
          }}
          placeholder={spec.defaultValue ?? ''}
          error={error}
        />
      )}
    </div>
  );
}

interface WebhookEditorProps {
  playbookId: string;
  /// The webhook being edited; null creates one.
  existing: WebhookDto | null;
}

/// The webhook form, for a new webhook on one playbook or an edit of a stored one. The verifier is
/// chosen once, at creation. Every expression is checked on the server as it is typed, the preview
/// runs the transform on a sample or a recorded delivery, and the endpoint validates everything
/// again on save, pinning each refusal to its field.
function WebhookEditor({ playbookId, existing }: WebhookEditorProps) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const playbooks = $api.useQuery('get', '/api/playbooks');
  const schema = $api.useQuery('get', '/api/playbooks/{id}/schema', { params: { path: { id: playbookId } } });
  const presets = $api.useQuery('get', '/api/webhooks/presets');
  const language = $api.useQuery('get', '/api/webhooks/cel');
  const recorded = $api.useQuery(
    'get',
    '/api/webhooks/{id}/deliveries',
    { params: { path: { id: existing?.id ?? '' }, query: { limit: 25 } } },
    { enabled: existing !== null }
  );
  const create = $api.useMutation('post', '/api/webhooks');
  const replace = $api.useMutation('put', '/api/webhooks/{id}');
  const preview = $api.useMutation('post', '/api/webhooks/preview');

  const parsed = useMemo(
    () => (schema.data === undefined ? null : parseParamsSchema(schema.data)),
    [schema.data]
  );
  const specs: ParamFieldSpec[] = useMemo(
    () => (parsed !== null && parsed.kind === 'form' ? parsed.specs : []),
    [parsed]
  );
  const completions = useMemo(
    () => (language.data === undefined ? undefined : celCompletions(language.data)),
    [language.data]
  );

  const [form, setForm] = useState<WebhookFormState>(() => initialState([]));
  const [presetId, setPresetId] = useState('');
  const [unmatched, setUnmatched] = useState<string[]>([]);
  const [sample, setSample] = useState(SAMPLE_QUAY);
  const [sampleHeaders, setSampleHeaders] = useState('');
  const [previewed, setPreviewed] = useState<WebhookPreviewDto | null>(null);
  const [fieldErrors, setFieldErrors] = useState<ReadonlyMap<string, string>>(new Map());
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [created, setCreated] = useState<WebhookSecretDto | null>(null);

  useEffect(() => {
    if (parsed === null || parsed.kind !== 'form') return;
    setForm(existing === null ? initialState(parsed.specs) : stateFromWebhook(existing, parsed.specs));
  }, [parsed, existing]);

  const derived = useMemo(() => {
    const out: Record<string, string> = {};
    for (const [name, choice] of Object.entries(form.choices)) {
      if (choice.mode === 'derived') out[name] = choice.expression;
    }
    return out;
  }, [form.choices]);
  const checked = byField(useCelCheck(form.filter, form.dedupe, derived));
  const diagnosticsOf = (field: string) => checked.get(field) ?? NO_DIAGNOSTICS;

  const pack = playbooks.data?.find((p) => p.id === playbookId);
  const crumbs =
    existing === null
      ? [
          { label: 'Playbooks', to: '/playbooks' },
          { label: playbookId, to: `/playbooks/${encodeURIComponent(playbookId)}` },
          { label: 'New webhook' },
        ]
      : [
          { label: 'Webhooks', to: '/webhooks' },
          { label: existing.id.slice(0, 13), to: `/webhooks/${encodeURIComponent(existing.id)}` },
          { label: 'Edit' },
        ];

  if (schema.isPending || playbooks.isPending) return <LoadingBlock label="Loading form" />;
  if (schema.isError) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty title="No such playbook" description={formatError(schema.error)} />
      </>
    );
  }
  if (parsed === null || parsed.kind === 'unrenderable') {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty
          title="Form cannot be rendered"
          description={parsed === null ? 'The playbook served no schema.' : parsed.reason}
        />
      </>
    );
  }

  const update = (patch: Partial<WebhookFormState>) => {
    setForm((previous) => ({ ...previous, ...patch }));
    setPreviewed(null);
  };

  const choose = (preset: string) => {
    setPresetId(preset);
    const found = presets.data?.find((p) => p.id === preset);
    if (found === undefined) return;
    const applied = applyPreset(found, specs, form);
    setForm(applied.state);
    setUnmatched(applied.unmatched);
    setPreviewed(null);
  };

  const loadRecorded = (deliveryId: string) => {
    const delivery = recorded.data?.find((d) => d.id === deliveryId);
    if (delivery === undefined) return;
    setSample(delivery.body ?? '');
    setSampleHeaders(headerLines(delivery.headers));
    setPreviewed(null);
  };

  const fieldNames = [
    'verifier',
    'header',
    'filter',
    'dedupe',
    'max_launches_per_hour',
    'retention_days',
    'max_cost',
    'max_time',
    ...specs.flatMap((s) => [s.name, `derive.${s.name}`]),
  ];

  const applyRejection = (err: unknown) => {
    const rejection = mapServerRejection(err, fieldNames);
    setFieldErrors(rejection.fieldErrors);
    setSubmitError(rejection.general);
  };

  const runPreview = async () => {
    setSubmitError(null);
    const built = previewBody(form, sample, sampleHeaders);
    if ('error' in built) {
      setSubmitError(built.error);
      return;
    }
    try {
      setPreviewed(await preview.mutateAsync({ body: built.body }));
      setFieldErrors(new Map());
    } catch (err: unknown) {
      setPreviewed(null);
      applyRejection(err);
    }
  };

  const save = async () => {
    setSubmitError(null);
    const body = createBody(playbookId, pack?.schema_digest ?? '', form);
    try {
      if (existing === null) {
        setCreated(await create.mutateAsync({ body }));
      } else {
        await replace.mutateAsync({ params: { path: { id: existing.id } }, body });
        void queryClient.invalidateQueries({ queryKey: ['get', '/api/webhooks/{id}'] });
        void navigate(`/webhooks/${encodeURIComponent(existing.id)}`);
      }
      setFieldErrors(new Map());
    } catch (err: unknown) {
      applyRejection(err);
    }
  };

  const errorOf = (name: string) => fieldErrors.get(name);

  if (created !== null) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <PageHeader eyebrow="Webhook" title={`Webhook for ${playbookId}`} description="Created." />
        <SecretOnce
          secret={created.secret}
          url={created.delivery_url ?? null}
          path={created.webhook.delivery_path}
          verifier={created.webhook.verifier}
        />
        <FormActions>
          <Button
            variant="filled"
            className="uppercase"
            render={<Link to={`/webhooks/${encodeURIComponent(created.webhook.id)}`} />}
          >
            Open webhook
          </Button>
        </FormActions>
      </>
    );
  }

  return (
    <>
      <Breadcrumb items={crumbs} />
      <PageHeader
        eyebrow="Webhook"
        title={existing === null ? `Webhook for ${playbookId}` : `Edit webhook for ${playbookId}`}
        description="Launch this playbook when a sender posts a delivery. The filter decides which deliveries launch, the dedupe key launches each event once, and every param is fixed or derived from the delivery."
      />

      <Section>
        <SectionHeader title="Sender" note="fixed once created" />
        <SectionBody>
          {existing === null ? (
            <FormGrid>
              <SelectField
                id="preset"
                label="Preset"
                value={presetId}
                onChange={choose}
                options={[
                  { value: '', label: 'none' },
                  ...(presets.data ?? []).map((p) => ({ value: p.id, label: p.title })),
                ]}
                hint="Fills the verifier, the transform, and the params it can match by name."
              />
              {unmatched.length > 0 ? (
                <Mono size="data" tone="ink-3">
                  This playbook declares no {unmatched.join(', ')}; the preset's derivation for{' '}
                  {unmatched.length === 1 ? 'it' : 'them'} was skipped.
                </Mono>
              ) : null}
              <SelectField
                id="verifier"
                label="Verifier"
                value={form.verifier}
                onChange={(value) => {
                  update({ verifier: value === 'hmac_sha256' ? 'hmac_sha256' : 'path_token' });
                }}
                options={VERIFIERS}
              />
              <FieldNote error={errorOf('verifier')} />
              {form.verifier === 'hmac_sha256' ? (
                <TextField
                  id="header"
                  label="Signature header"
                  mono
                  value={form.header}
                  onChange={(header) => {
                    update({ header });
                  }}
                  placeholder="x-hub-signature-256"
                  error={errorOf('header')}
                />
              ) : null}
            </FormGrid>
          ) : (
            <div className="grid gap-2">
              <MetaRow label="Verifier">
                <Mono size="data">{existing.verifier}</Mono>
              </MetaRow>
              {existing.header ? (
                <MetaRow label="Signature header">
                  <Mono size="data">{existing.header}</Mono>
                </MetaRow>
              ) : null}
              <FieldNote error={errorOf('verifier')} />
            </div>
          )}
        </SectionBody>
      </Section>

      <Section>
        <SectionHeader title="Transform" note="CEL over body, headers, delivery, and received_at; checked as you type" />
        <SectionBody>
          <FormGrid>
            <CelEditor
              field="filter"
              label="Filter"
              lines={2}
              value={form.filter}
              onChange={(filter) => {
                update({ filter });
              }}
              diagnostics={diagnosticsOf('filter')}
              completions={completions}
              hint="A bool. A delivery it rejects settles filtered and launches nothing."
            />
            <FieldNote error={errorOf('filter')} />
            <CelEditor
              field="dedupe"
              label="Dedupe key"
              value={form.dedupe}
              onChange={(dedupe) => {
                update({ dedupe });
              }}
              diagnostics={diagnosticsOf('dedupe')}
              completions={completions}
              hint="A string or int. A key that already launched settles duplicate; `delivery` makes every delivery its own event."
            />
            <FieldNote error={errorOf('dedupe')} />
            {specs.map((spec) => {
              const choice = form.choices[spec.name];
              if (choice === undefined) return null;
              return (
                <ParamRow
                  key={spec.name}
                  spec={spec}
                  choice={choice}
                  onChange={(next) => {
                    update({ choices: { ...form.choices, [spec.name]: next } });
                  }}
                  error={errorOf(`derive.${spec.name}`) ?? errorOf(spec.name)}
                  diagnostics={diagnosticsOf(`derive.${spec.name}`)}
                  completions={completions}
                />
              );
            })}
          </FormGrid>
        </SectionBody>
      </Section>

      <Section>
        <SectionHeader title="Bounds" />
        <SectionBody>
          <FormGrid className="grid-cols-2">
            <NumberInputField
              id="rate"
              label="Launches per hour"
              value={form.maxLaunchesPerHour}
              min={1}
              max={120}
              onChange={(maxLaunchesPerHour) => {
                update({ maxLaunchesPerHour });
              }}
              error={errorOf('max_launches_per_hour')}
            />
            <NumberInputField
              id="retention"
              label="Keep deliveries (days)"
              value={form.retentionDays}
              min={1}
              max={90}
              onChange={(retentionDays) => {
                update({ retentionDays });
              }}
              error={errorOf('retention_days')}
            />
            <NumberInputField
              id="max-cost"
              label="Max cost per run (USD)"
              value={form.maxCost}
              min={0.01}
              step={0.5}
              onChange={(maxCost) => {
                update({ maxCost });
              }}
              error={errorOf('max_cost')}
            />
            <TextField
              id="max-time"
              label="Max time per run"
              mono
              value={form.maxTime}
              onChange={(maxTime) => {
                update({ maxTime });
              }}
              error={errorOf('max_time')}
            />
          </FormGrid>
        </SectionBody>
      </Section>

      <Section>
        <SectionHeader title="Preview" note="runs the transform above; stores nothing" />
        <SectionBody>
          <FormGrid>
            {existing !== null ? (
              <SelectField
                id="recorded"
                label="Use a recorded delivery"
                value=""
                onChange={loadRecorded}
                options={[
                  { value: '', label: recorded.data?.length ? 'choose one' : 'none recorded yet' },
                  ...(recorded.data ?? []).map((d) => ({
                    value: d.id,
                    label: `${d.received_at} · ${d.outcome}${d.reason ? ` · ${d.reason}` : ''}`,
                  })),
                ]}
                hint="Loads its body and headers as the sample, so the preview runs on what the sender actually posted."
              />
            ) : null}
            <TextAreaField id="sample" label="Sample body" rows={8} value={sample} onChange={setSample} />
            <TextAreaField
              id="headers"
              label="Sample headers"
              rows={2}
              value={sampleHeaders}
              onChange={setSampleHeaders}
              placeholder="X-GitHub-Event: release"
            />
            <div>
              <Button variant="quiet" className="uppercase" onClick={() => void runPreview()} disabled={preview.isPending}>
                Run preview
              </Button>
            </div>
            {previewed !== null ? (
              <div className="grid gap-2">
                <Result label="Filter" result={previewed.filter} />
                <Result label="Dedupe key" result={previewed.dedupe} />
                <Result label="Derived params" result={previewed.derive} />
              </div>
            ) : null}
          </FormGrid>
        </SectionBody>
      </Section>

      {submitError !== null && <FormError className="mx-4.5 my-3">{submitError}</FormError>}
      <FormActions>
        <Button
          variant="filled"
          className="uppercase"
          onClick={() => void save()}
          disabled={create.isPending || replace.isPending}
        >
          {existing === null ? 'Create webhook' : 'Save webhook'}
        </Button>
      </FormActions>
    </>
  );
}

/// A new webhook on the playbook in the route.
export function NewWebhookPage() {
  const { id = '' } = useParams<{ id: string }>();
  return <WebhookEditor playbookId={id} existing={null} />;
}

/// An edit of the webhook in the route.
export function EditWebhookPage() {
  const { id = '' } = useParams<{ id: string }>();
  const webhook = $api.useQuery('get', '/api/webhooks/{id}', { params: { path: { id } } });
  if (webhook.isPending) return <LoadingBlock label="Loading webhook" />;
  if (webhook.isError) {
    return (
      <>
        <Breadcrumb items={[{ label: 'Webhooks', to: '/webhooks' }, { label: id.slice(0, 13) }]} />
        <Empty title="No such webhook" description={formatError(webhook.error)} />
      </>
    );
  }
  return <WebhookEditor playbookId={webhook.data.playbook} existing={webhook.data} />;
}
