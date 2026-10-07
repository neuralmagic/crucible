import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { narrow, withOwner } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import { OwnerField } from './OwnerField';
import {
  Button,
  createDataColumnHelper,
  DataTable,
  Empty,
  LoadingBlock,
  Mono,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
  useDataTable,
} from '../ui';
import {
  CheckField,
  FormActions,
  FormError,
  FormGrid,
  Note,
  SelectField,
  TextAreaField,
  TextField,
} from './formControls';
import { optionValue } from './pickList';
import { ProviderIcon } from './ProviderIcon';
import { SharesSection } from './SharesSection';
import {
  CLASS_OPTIONS,
  ROLE_OPTIONS,
  servesRole,
  defaultBody,
  defaultErrors,
  defaultFormOf,
  emptyDefaultForm,
  emptyProviderForm,
  formOf,
  harnessFor,
  harnessOptions,
  keyVariable,
  KIND_OPTIONS,
  PROTOCOL_OPTIONS,
  providerBody,
  providerErrors,
  reachLabel,
  registerProviderBody,
  SCOPE_OPTIONS,
  targetLabel,
  type DefaultForm,
  type DispatchDefaultDto,
  type ProviderDetailDto,
  type ProviderDto,
  type ProviderForm,
} from './providersView';

const PROVIDERS_KEY = ['get', '/api/providers'];
const PICKER_KEY = ['get', '/api/config/providers'];

const helper = createDataColumnHelper<ProviderDetailDto>();

function columns(onOpen: (id: string) => void) {
  return helper.columns([
    helper.accessor('id', {
      header: 'Id',
      enableSorting: false,
      meta: { pad: 'tight', shrink: true },
      cell: ({ row, getValue }) => (
        <Button variant="quiet" onClick={() => { onOpen(row.original.id); }}>
          {getValue()}
        </Button>
      ),
    }),
    helper.accessor('display_name', {
      header: 'Name',
      enableSorting: false,
      meta: { shrink: true },
      cell: ({ row, getValue }) => (
        <span className="inline-flex items-center gap-1.5">
          <ProviderIcon kind={row.original.kind} />
          {getValue()}
        </span>
      ),
    }),
    helper.accessor('owner', {
      header: 'Owner',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    }),
    helper.display({
      id: 'reach',
      header: 'Reaches',
      meta: { wrap: true, className: 'font-mono text-data text-ink-2' },
      cell: ({ row }) => reachLabel(row.original),
    }),
    helper.accessor('roles', {
      header: 'Serves',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
      cell: ({ getValue }) => getValue().join(' + '),
    }),
    helper.accessor('harness', {
      header: 'Harness',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
      cell: ({ getValue }) => getValue() ?? '—',
    }),
    helper.accessor('default_model', {
      header: 'Default model',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    }),
    helper.display({
      id: 'secret',
      header: 'Secret',
      meta: { shrink: true, className: 'font-mono text-data text-ink-3' },
      cell: ({ row }) =>
        row.original.secret_name === null || row.original.secret_name === undefined
          ? 'ambient'
          : `${row.original.secret_owner ?? '?'} / ${row.original.secret_name}`,
    }),
    helper.accessor('enabled', {
      header: 'Offered',
      enableSorting: false,
      meta: { shrink: true },
      cell: ({ getValue }) => (
        <Mono size="data" tone={getValue() ? 'ink' : 'ink-3'}>
          {getValue() ? 'yes' : 'disabled'}
        </Mono>
      ),
    }),
  ]);
}

function ProviderTable({ rows, onOpen }: { rows: ProviderDetailDto[]; onOpen: (id: string) => void }) {
  const table = useDataTable({ columns: columns(onOpen), data: rows, getRowId: (row) => row.id });
  return (
    <DataTable
      table={table}
      empty={
        <Empty
          title="No providers"
          description="Register one below. Until then every dispatch runs the pack manifest's own agent."
        />
      }
      footer={`${rows.length} provider${rows.length === 1 ? '' : 's'}`}
    />
  );
}

interface ProviderFormFieldsProps {
  idPrefix: string;
  form: ProviderForm;
  editing: boolean;
  onChange: (form: ProviderForm) => void;
}

/// The registration form, shared by register and edit. What a kind has no use for is hidden rather
/// than disabled, so the form reads as the registration it will send.
function ProviderFormFields({ idPrefix, form, editing, onChange }: ProviderFormFieldsProps) {
  const errors = providerErrors(form, editing);
  const error = (field: string) => errors.get(field) ?? null;
  const variable = keyVariable(form.kind, form.protocol);
  return (
    <FormGrid>
      {editing ? null : (
        <TextField
          id={`${idPrefix}-id`}
          label="Id"
          mono
          required
          value={form.id}
          onChange={(id) => { onChange({ ...form, id }); }}
          hint="The slug every launch pins and every URL names. Lowercase letters, digits, dashes."
          error={form.id.length > 0 ? error('id') : null}
        />
      )}
      <TextField
        id={`${idPrefix}-display-name`}
        label="Display name"
        required
        value={form.displayName}
        onChange={(displayName) => { onChange({ ...form, displayName }); }}
        error={form.displayName.length > 0 ? error('displayName') : null}
      />
      <SelectField
        id={`${idPrefix}-kind`}
        label="Kind"
        required
        value={form.kind}
        onChange={(kind) => {
          const next = optionValue(KIND_OPTIONS, kind, 'openai');
          onChange({ ...form, kind: next, harness: harnessFor(next, form.protocol, form.harness) });
        }}
        options={KIND_OPTIONS}
        hint="The kind decides where the key goes and which harnesses can run it. A custom provider says both through its protocol."
      />
      {form.kind === 'custom' && (
        <>
          <TextField
            id={`${idPrefix}-endpoint`}
            label="Endpoint"
            mono
            required
            value={form.endpoint}
            onChange={(endpoint) => { onChange({ ...form, endpoint }); }}
            placeholder="http://vllm.internal:8000/v1"
            error={form.endpoint.length > 0 ? error('endpoint') : null}
          />
          <SelectField
            id={`${idPrefix}-protocol`}
            label="Protocol"
            required
            value={form.protocol}
            onChange={(protocol) => {
              const next = optionValue(PROTOCOL_OPTIONS, protocol, 'chat_completions');
              onChange({ ...form, protocol: next, harness: harnessFor(form.kind, next, form.harness) });
            }}
            options={PROTOCOL_OPTIONS}
          />
        </>
      )}
      {harnessOptions(form.kind, form.protocol).length > 0 && (
        <SelectField
          id={`${idPrefix}-harness`}
          label="Harness"
          value={form.harness}
          onChange={(harness) => { onChange({ ...form, harness }); }}
          options={harnessOptions(form.kind, form.protocol)}
          hint="The agent CLI a dispatch to this provider runs. Blank takes the default for the kind or protocol."
        />
      )}
      <TextAreaField
        id={`${idPrefix}-models`}
        label="Models"
        rows={3}
        value={form.models}
        onChange={(models) => { onChange({ ...form, models }); }}
        placeholder={form.kind === 'custom' ? 'gpt-oss-120b' : 'one per line; blank takes the curated list'}
        hint="What the launch pickers offer. A launch may still type a model outside the list."
      />
      {error('models') !== null && <Note>{error('models')}</Note>}
      <TextField
        id={`${idPrefix}-default-model`}
        label="Default model"
        mono
        required={form.kind === 'custom'}
        value={form.defaultModel}
        onChange={(defaultModel) => { onChange({ ...form, defaultModel }); }}
        hint={
          form.kind === 'custom'
            ? 'What a launch that names no model asks for. Your service decides what it serves, so this is required.'
            : 'Blank takes the kind\'s default.'
        }
        error={form.defaultModel.length > 0 || form.kind === 'custom' ? error('defaultModel') : null}
      />
      {form.kind !== 'vertex' && (
        <>
          <TextField
            id={`${idPrefix}-secret-name`}
            label="Secret"
            mono
            value={form.secretName}
            onChange={(secretName) => { onChange({ ...form, secretName }); }}
            hint={`An inference_api_key from the Secrets page${variable === null ? '' : `, carrying ${variable}`}. Blank runs on the pod's own environment.`}
            error={error('secretName')}
          />
          <TextField
            id={`${idPrefix}-secret-owner`}
            label="Secret owner"
            mono
            value={form.secretOwner}
            onChange={(secretOwner) => { onChange({ ...form, secretOwner }); }}
            placeholder="user:<login> or group:<path>"
            hint="Needed only when more than one owner registered that name."
            error={error('secretOwner')}
          />
        </>
      )}
      <CheckField
        id={`${idPrefix}-enabled`}
        label="Offered at launch"
        checked={form.enabled}
        onChange={(enabled) => { onChange({ ...form, enabled }); }}
        description="Off stops new work reaching it; a launch that already pinned it still resolves."
      />
    </FormGrid>
  );
}

function RegisterSection() {
  const qc = useQueryClient();
  const register = $api.useMutation('post', '/api/providers');
  const [form, setForm] = useState<ProviderForm>(emptyProviderForm);
  const [error, setError] = useState<string | null>(null);
  const [registered, setRegistered] = useState<string | null>(null);
  const [ownerChoice, setOwnerChoice] = useState('');
  const ownerContext = useOwnerContext();
  const owner = withOwner(ownerChoice, ownerContext.context, ownerContext.owners);
  const errors = providerErrors(form, false);

  const submit = async () => {
    setError(null);
    setRegistered(null);
    try {
      const created = await register.mutateAsync({ body: registerProviderBody(form, owner) });
      setForm(emptyProviderForm());
      setRegistered(created.id);
      await qc.invalidateQueries({ queryKey: PROVIDERS_KEY });
      await qc.invalidateQueries({ queryKey: PICKER_KEY });
    } catch (err: unknown) {
      setError(formatError(err));
    } finally {
      register.reset();
    }
  };

  return (
    <Section>
      <SectionHeader title="Register" />
      <SectionBody>
        <FormGrid>
          <OwnerField id="provider-owner" value={owner} onChange={setOwnerChoice} options={ownerContext.owners} />
        </FormGrid>
        <ProviderFormFields idPrefix="provider" form={form} editing={false} onChange={setForm} />
      </SectionBody>
      {error === null ? null : (
        <SectionBody>
          <FormError>{error}</FormError>
        </SectionBody>
      )}
      <FormActions>
        <Button
          className="uppercase"
          variant="filled"
          disabled={errors.size > 0 || register.isPending}
          onClick={() => { void submit(); }}
        >
          {register.isPending ? 'Registering…' : 'Register'}
        </Button>
        {registered !== null && (
          <Mono size="data" tone="ink-3">
            registered {registered}
          </Mono>
        )}
      </FormActions>
    </Section>
  );
}

function DetailSection({ provider, onClose }: { provider: ProviderDetailDto; onClose: () => void }) {
  const qc = useQueryClient();
  const update = $api.useMutation('put', '/api/providers/{id}');
  const remove = $api.useMutation('delete', '/api/providers/{id}');
  const [form, setForm] = useState<ProviderForm>(() => formOf(provider));
  const [error, setError] = useState<string | null>(null);
  const errors = providerErrors(form, true);
  const mayUpdate = provider.actions.includes('update');
  const mayDelete = provider.actions.includes('delete');

  const refresh = async () => {
    await qc.invalidateQueries({ queryKey: PROVIDERS_KEY });
    await qc.invalidateQueries({ queryKey: PICKER_KEY });
  };

  const save = async () => {
    setError(null);
    try {
      await update.mutateAsync({ params: { path: { id: provider.id } }, body: providerBody(form) });
      await refresh();
    } catch (err: unknown) {
      setError(formatError(err));
    } finally {
      update.reset();
    }
  };

  const deregister = async () => {
    setError(null);
    try {
      await remove.mutateAsync({ params: { path: { id: provider.id } } });
      await refresh();
      onClose();
    } catch (err: unknown) {
      setError(formatError(err));
    } finally {
      remove.reset();
    }
  };

  return (
    <Section>
      <SectionHeader
        title={provider.id}
        note={`registered by ${provider.created_by} · updated ${provider.updated_at}`}
        actions={<Button className="uppercase" onClick={onClose}>Close</Button>}
      />
      {mayUpdate ? (
        <SectionBody>
          <ProviderFormFields idPrefix={`edit-${provider.id}`} form={form} editing onChange={setForm} />
        </SectionBody>
      ) : null}
      {error === null ? null : (
        <SectionBody>
          <FormError>{error}</FormError>
        </SectionBody>
      )}
      {mayUpdate || mayDelete ? (
        <FormActions>
          {mayUpdate ? (
            <Button
              className="uppercase"
              variant="filled"
              disabled={errors.size > 0 || update.isPending}
              onClick={() => { void save(); }}
            >
              {update.isPending ? 'Saving…' : 'Save'}
            </Button>
          ) : null}
          {mayDelete ? (
            <Button className="uppercase" disabled={remove.isPending} onClick={() => { void deregister(); }}>
              {remove.isPending ? 'Deregistering…' : 'Deregister'}
            </Button>
          ) : null}
        </FormActions>
      ) : null}
    </Section>
  );
}

const defaultsHelper = createDataColumnHelper<DispatchDefaultDto>();

function defaultKey(row: DispatchDefaultDto): string {
  return `${row.scope_kind}:${row.scope_ref}:${row.workload_class}:${row.role}`;
}

function defaultColumns(onEdit: ((row: DispatchDefaultDto) => void) | null, onClear: ((row: DispatchDefaultDto) => void) | null) {
  return defaultsHelper.columns([
    defaultsHelper.display({
      id: 'scope',
      header: 'Scope',
      meta: { shrink: true, className: 'font-mono text-data' },
      cell: ({ row }) =>
        onEdit === null ? (
          row.original.scope_kind === 'platform' ? 'platform' : row.original.scope_ref
        ) : (
          <Button variant="quiet" onClick={() => { onEdit(row.original); }}>
            {row.original.scope_kind === 'platform' ? 'platform' : row.original.scope_ref}
          </Button>
        ),
    }),
    defaultsHelper.accessor('workload_class', {
      header: 'Class',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    }),
    defaultsHelper.accessor('role', {
      header: 'Role',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    }),
    defaultsHelper.display({
      id: 'primary',
      header: 'Primary',
      meta: { className: 'font-mono text-data' },
      cell: ({ row }) => targetLabel(row.original.provider, row.original.model),
    }),
    defaultsHelper.display({
      id: 'fallback',
      header: 'Fallback',
      meta: { className: 'font-mono text-data text-ink-2' },
      cell: ({ row }) =>
        row.original.fallback_provider === null || row.original.fallback_provider === undefined
          ? '—'
          : targetLabel(row.original.fallback_provider, row.original.fallback_model),
    }),
    ...(onClear === null
      ? []
      : [
          defaultsHelper.display({
            id: 'clear',
            header: '',
            meta: { shrink: true },
            cell: ({ row }) => (
              <Button variant="quiet" onClick={() => { onClear(row.original); }}>
                CLEAR
              </Button>
            ),
          }),
        ]),
  ]);
}

function modelOptions(provider: ProviderDto | undefined, current: string) {
  if (provider === undefined) return [{ value: '', label: 'provider default' }];
  const models = provider.models.includes(current) || current.length === 0 ? provider.models : [...provider.models, current];
  return [
    { value: '', label: `default (${provider.default_model})` },
    ...models.map((m) => ({ value: m, label: m })),
  ];
}

function DefaultFormFields({
  form,
  providers,
  onChange,
}: {
  form: DefaultForm;
  providers: readonly ProviderDto[];
  onChange: (form: DefaultForm) => void;
}) {
  const errors = defaultErrors(form);
  const error = (field: string) => errors.get(field) ?? null;
  const providerOptions = providers
    .filter((p) => servesRole(p, form.role))
    .map((p) => ({ value: p.id, label: `${p.display_name} (${p.id})` }));
  const primary = providers.find((p) => p.id === form.provider);
  const fallback = providers.find((p) => p.id === form.fallbackProvider);
  return (
    <FormGrid>
      <SelectField
        id="default-scope"
        label="Scope"
        value={form.scopeKind}
        onChange={(scopeKind) => { onChange({ ...form, scopeKind: optionValue(SCOPE_OPTIONS, scopeKind, 'platform') }); }}
        options={SCOPE_OPTIONS}
      />
      {form.scopeKind === 'domain' && (
        <TextField
          id="default-domain"
          label="Domain"
          mono
          required
          value={form.scopeRef}
          onChange={(scopeRef) => { onChange({ ...form, scopeRef }); }}
          placeholder="owner/repo"
          error={form.scopeRef.length > 0 ? error('scopeRef') : null}
        />
      )}
      <SelectField
        id="default-class"
        label="Class"
        value={form.workloadClass}
        onChange={(workloadClass) => {
          onChange({ ...form, workloadClass: optionValue(CLASS_OPTIONS, workloadClass, 'autoresearch') });
        }}
        options={CLASS_OPTIONS}
      />
      <SelectField
        id="default-role"
        label="Role"
        value={form.role}
        onChange={(role) => {
          const next = optionValue(ROLE_OPTIONS, role, 'agent');
          const keeps = (id: string) => providers.some((p) => p.id === id && servesRole(p, next));
          onChange({
            ...form,
            role: next,
            ...(keeps(form.provider) ? {} : { provider: '', model: '' }),
            ...(keeps(form.fallbackProvider) ? {} : { fallbackProvider: '', fallbackModel: '' }),
          });
        }}
        options={ROLE_OPTIONS}
      />
      {error('role') !== null && <Note>{error('role')}</Note>}
      <SelectField
        id="default-provider"
        label="Primary"
        required
        value={form.provider}
        onChange={(provider) => { onChange({ ...form, provider, model: '' }); }}
        options={[{ value: '', label: '—' }, ...providerOptions]}
      />
      <SelectField
        id="default-model"
        label="Primary model"
        value={form.model}
        onChange={(model) => { onChange({ ...form, model }); }}
        options={modelOptions(primary, form.model)}
      />
      <SelectField
        id="default-fallback"
        label="Fallback"
        value={form.fallbackProvider}
        onChange={(fallbackProvider) => { onChange({ ...form, fallbackProvider, fallbackModel: '' }); }}
        options={[{ value: '', label: 'none' }, ...providerOptions]}
      />
      {form.fallbackProvider.length > 0 && (
        <SelectField
          id="default-fallback-model"
          label="Fallback model"
          value={form.fallbackModel}
          onChange={(fallbackModel) => { onChange({ ...form, fallbackModel }); }}
          options={modelOptions(fallback, form.fallbackModel)}
        />
      )}
      {error('fallbackProvider') !== null && <Note>{error('fallbackProvider')}</Note>}
    </FormGrid>
  );
}

function DefaultsSection({ admin }: { admin: boolean }) {
  const qc = useQueryClient();
  const registry = $api.useQuery('get', '/api/config/providers');
  const put = $api.useMutation('put', '/api/config/dispatch-defaults');
  const clear = $api.useMutation('delete', '/api/config/dispatch-defaults');
  const [form, setForm] = useState<DefaultForm>(emptyDefaultForm);
  const [error, setError] = useState<string | null>(null);
  const rows = registry.data?.defaults ?? [];
  const providers = registry.data?.providers ?? [];
  const errors = defaultErrors(form);

  const run = async (action: () => Promise<unknown>) => {
    setError(null);
    try {
      await action();
      await qc.invalidateQueries({ queryKey: PICKER_KEY });
    } catch (err: unknown) {
      setError(formatError(err));
    } finally {
      put.reset();
      clear.reset();
    }
  };

  const save = () =>
    run(async () => {
      await put.mutateAsync({ body: defaultBody(form) });
      setForm(emptyDefaultForm());
    });

  const remove = (row: DispatchDefaultDto) =>
    run(() =>
      clear.mutateAsync({
        params: {
          query: {
            scope_kind: row.scope_kind,
            workload_class: row.workload_class,
            role: row.role,
            ...(row.scope_kind === 'domain' ? { scope_ref: row.scope_ref } : {}),
          },
        },
      }),
    );

  const table = useDataTable({
    columns: defaultColumns(
      admin ? (row) => { setForm(defaultFormOf(row)); } : null,
      admin ? (row) => { void remove(row); } : null,
    ),
    data: rows,
    getRowId: defaultKey,
  });

  return (
    <Section>
      <SectionHeader title="Defaults" />
      <DataTable table={table} empty={<Empty title="NO DEFAULTS" />} />
      {admin ? (
        <>
          <SectionBody>
            <DefaultFormFields form={form} providers={providers} onChange={setForm} />
          </SectionBody>
          {error === null ? null : (
            <SectionBody>
              <FormError>{error}</FormError>
            </SectionBody>
          )}
          <FormActions>
            <Button
              variant="filled"
              disabled={errors.size > 0 || put.isPending}
              onClick={() => { void save(); }}
            >
              {put.isPending ? 'SAVING…' : 'SET DEFAULT'}
            </Button>
          </FormActions>
        </>
      ) : null}
    </Section>
  );
}

export function ProvidersPage() {
  const providers = $api.useQuery('get', '/api/providers');
  const whoami = $api.useQuery('get', '/api/whoami');
  const ownerContext = useOwnerContext();
  const [open, setOpen] = useState<string | null>(null);
  const rows = narrow(providers.data ?? [], ownerContext.context, (p) => p.owner);
  const selected = providers.data?.find((p) => p.id === open) ?? null;

  return (
    <div className="grid gap-3">
      <PageHeader eyebrow="System" title="Providers" />
      {providers.isLoading ? (
        <LoadingBlock />
      ) : (
        <>
          <Section>
            <SectionHeader
              title="Registry"
              note="what a launch may pick; the pack manifest's [agent] table is what an empty registry runs"
            />
            <ProviderTable rows={rows} onOpen={setOpen} />
          </Section>
          {selected === null ? null : (
            <>
              <DetailSection key={selected.id} provider={selected} onClose={() => { setOpen(null); }} />
              {selected.actions.includes('share') ? (
                <SharesSection path="/api/providers/{id}" id={selected.id} />
              ) : null}
            </>
          )}
          <DefaultsSection admin={whoami.data?.role === 'admin'} />
          <RegisterSection />
        </>
      )}
    </div>
  );
}
