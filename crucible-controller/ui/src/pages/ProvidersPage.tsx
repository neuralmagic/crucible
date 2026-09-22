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
  type ProviderDetailDto,
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
    helper.accessor('harness', {
      header: 'Harness',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
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
          title="NO PROVIDERS"
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
            hint="The base URL the harness's API paths are appended to. Lands in the pod as OPENAI_BASE_URL or ANTHROPIC_BASE_URL, the same config key an OpenShell provider carries."
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
            hint="What the endpoint speaks. Chat completions runs OpenCode or Pi; responses runs Codex or Pi; messages runs Claude Code, OpenCode, or Pi."
          />
        </>
      )}
      <SelectField
        id={`${idPrefix}-harness`}
        label="Harness"
        value={form.harness}
        onChange={(harness) => { onChange({ ...form, harness }); }}
        options={harnessOptions(form.kind, form.protocol)}
        hint="The agent CLI a dispatch to this provider runs. Blank takes the default for the kind or protocol."
      />
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
          variant="filled"
          disabled={errors.size > 0 || register.isPending}
          onClick={() => { void submit(); }}
        >
          {register.isPending ? 'REGISTERING…' : 'REGISTER'}
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
        actions={<Button onClick={onClose}>CLOSE</Button>}
      />
      <SectionBody>
        <ProviderFormFields idPrefix={`edit-${provider.id}`} form={form} editing onChange={setForm} />
      </SectionBody>
      {error === null ? null : (
        <SectionBody>
          <FormError>{error}</FormError>
        </SectionBody>
      )}
      <FormActions>
        <Button
          variant="filled"
          disabled={errors.size > 0 || update.isPending}
          onClick={() => { void save(); }}
        >
          {update.isPending ? 'SAVING…' : 'SAVE'}
        </Button>
        <Button disabled={remove.isPending} onClick={() => { void deregister(); }}>
          {remove.isPending ? 'DEREGISTERING…' : 'DEREGISTER'}
        </Button>
      </FormActions>
    </Section>
  );
}

export function ProvidersPage() {
  const whoami = $api.useQuery('get', '/api/whoami');
  const admin = whoami.data?.role === 'admin';
  const providers = $api.useQuery('get', '/api/providers', undefined, { enabled: admin });
  const ownerContext = useOwnerContext();
  const [open, setOpen] = useState<string | null>(null);
  const rows = narrow(providers.data ?? [], ownerContext.context, (p) => p.owner);
  const selected = providers.data?.find((p) => p.id === open) ?? null;

  return (
    <div className="grid gap-3">
      <PageHeader eyebrow="System" title="Providers" />
      {!admin ? (
        <Section>
          <SectionBody>
            <Empty
              title="ADMINS ONLY"
              description="The provider registry decides where every launch can send work and which key pays for it."
            />
          </SectionBody>
        </Section>
      ) : providers.isLoading ? (
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
              <SharesSection path="/api/providers/{id}" id={selected.id} />
            </>
          )}
          <RegisterSection />
        </>
      )}
    </div>
  );
}
