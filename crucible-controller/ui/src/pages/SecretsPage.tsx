import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { narrow } from '../ownerContext';
import { useOwnerContext } from '../useOwnerContext';
import { usePickerPrefs } from '../api/pickerPrefs';
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
  BareTextInput,
  CheckField,
  FormActions,
  FormError,
  FormGrid,
  Note,
  Notice,
  PasswordField,
  PickListField,
  SelectField,
  TextField,
} from './formControls';
import { optionValue, principalOptions, toggleFavorite, type PickOption } from './pickList';
import {
  agentVisibleWarning,
  bindBody,
  bindErrors,
  canBind,
  canRegister,
  CREDENTIAL_PRESETS,
  credentialErrors,
  credentialsValue,
  deleteBlocker,
  emptyBindForm,
  emptyCredentials,
  emptyRegisterForm,
  groupSecrets,
  isKeyShaped,
  KIND_OPTIONS,
  MINTERS,
  originLabel,
  projectionKindOptions,
  registerBody,
  registerErrors,
  SCOPE_KIND_OPTIONS,
  withOwners,
  visibilityOptions,
  withKind,
  withVisibility,
  type BindForm,
  type CredentialEntry,
  type Minter,
  type RegisterForm,
  type RegisterMode,
  type SecretDto,
} from './secretsView';

const NO_SECRETS: SecretDto[] = [];
const SECRETS_KEY = ['get', '/api/secrets'];

const MODE_OPTIONS: readonly { value: RegisterMode; label: string }[] = [
  { value: 'value', label: 'value (the hub writes it to Vault)' },
  { value: 'reference', label: 'reference (a vault:// path someone else owns)' },
  { value: 'mint', label: 'mint (issued fresh at every dispatch, never stored)' },
];

const MINTER_LABELS: Record<Minter, string> = {
  'github-app': 'github-app (an installation token for the controller\u2019s GitHub App)',
};

const MINTER_OPTIONS = MINTERS.map((minter) => ({ value: minter, label: MINTER_LABELS[minter] }));

interface CredentialRowsProps {
  idPrefix: string;
  entries: readonly CredentialEntry[];
  onChange: (entries: CredentialEntry[]) => void;
}

/// The rows an inference key is assembled from: the variable a harness reads, and its value. The
/// first row is preset to the variable the common providers read, so a single key needs no typing
/// beyond the key; an OpenAI-compatible or Anthropic-compatible service that wants more (an org id,
/// a project) gets another row.
function CredentialRows({ idPrefix, entries, onChange }: CredentialRowsProps) {
  const errors = credentialErrors(entries);
  const setAt = (index: number, patch: Partial<CredentialEntry>) => {
    onChange(entries.map((entry, i) => (i === index ? { ...entry, ...patch } : entry)));
  };
  return (
    <div className="grid gap-1.5">
      <span className="font-mono text-label font-semibold uppercase tracking-group text-ink-3">
        Credentials<span className="ml-1 text-red">*</span>
      </span>
      {entries.map((entry, i) => (
        <div key={i} className="grid gap-1">
          <div className="flex items-center gap-2">
            <BareTextInput
              id={`${idPrefix}-credential-name-${i}`}
              aria-label={`Variable ${i + 1}`}
              value={entry.name}
              onChange={(name) => { setAt(i, { name: name.toUpperCase() }); }}
              placeholder="OPENAI_API_KEY"
              mono
            />
            <BareTextInput
              id={`${idPrefix}-credential-value-${i}`}
              aria-label={`Value ${i + 1}`}
              type="password"
              value={entry.value}
              onChange={(value) => { setAt(i, { value }); }}
              placeholder="value"
              mono
            />
            {entries.length > 1 && (
              <Button
                className="border border-rule-hard px-2.5"
                aria-label="Remove credential"
                onClick={() => { onChange(entries.filter((_, j) => j !== i)); }}
              >
                −
              </Button>
            )}
          </div>
          {errors.has(i) && <Note>{errors.get(i)}</Note>}
        </div>
      ))}
      <div className="flex flex-wrap gap-2">
        <Button
          className="border border-rule-hard px-2.5"
          onClick={() => { onChange([...entries, { name: '', value: '' }]); }}
        >
          + ADD VARIABLE
        </Button>
        {CREDENTIAL_PRESETS.filter((preset) => !entries.some((e) => e.name === preset.name)).map(
          (preset) => (
            <Button
              key={preset.name}
              className="border border-rule-hard px-2.5"
              onClick={() => { onChange([...entries, { name: preset.name, value: '' }]); }}
            >
              + {preset.name}
            </Button>
          )
        )}
      </div>
      <p className="m-0 max-w-[66ch] text-data-lg text-ink-3">
        Each variable lands in the run pod as its own environment variable, broker-side only. The
        provider that spends this secret reads its key from the variable its protocol names
        (OPENAI_API_KEY for chat completions and responses, ANTHROPIC_API_KEY for messages), so that
        one has to be here.
      </p>
    </div>
  );
}

const helper = createDataColumnHelper<SecretDto>();

function columns(onOpen: (id: string) => void) {
  return helper.columns([
    helper.accessor('name', {
      header: 'Name',
      enableSorting: false,
      meta: { pad: 'tight', shrink: true },
      cell: ({ row, getValue }) => (
        <Button variant="quiet" onClick={() => { onOpen(row.original.id); }}>
          {getValue()}
        </Button>
      ),
    }),
    helper.accessor('owner', {
      header: 'Owner',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    }),
    helper.accessor('kind', {
      header: 'Kind',
      enableSorting: false,
      meta: { shrink: true, className: 'font-mono text-data text-ink-2' },
    }),
    helper.accessor('visibility', {
      header: 'Visibility',
      enableSorting: false,
      meta: { shrink: true },
      cell: ({ getValue }) => (
        <Mono size="data" tone={getValue() === 'agent_visible' ? 'ink' : 'ink-3'}>
          {getValue()}
        </Mono>
      ),
    }),
    helper.display({
      id: 'origin',
      header: 'Path',
      meta: { wrap: true, className: 'font-mono text-data text-ink-3' },
      cell: ({ row }) => originLabel(row.original),
    }),
  ]);
}

function SecretTable({
  rows,
  onOpen,
  emptyTitle,
  emptyDescription,
}: {
  rows: SecretDto[];
  onOpen: (id: string) => void;
  emptyTitle: string;
  emptyDescription: string;
}) {
  const table = useDataTable({ columns: columns(onOpen), data: rows, getRowId: (row) => row.id });
  return (
    <DataTable
      table={table}
      empty={<Empty title={emptyTitle} description={emptyDescription} />}
      footer={`${rows.length} secret${rows.length === 1 ? '' : 's'}`}
    />
  );
}

interface OwnerPickFieldProps {
  id: string;
  value: string;
  onChange: (owner: string) => void;
  options: readonly PickOption[];
  hint?: string;
}

/// The principal picker: the caller's own login and groups, filterable, with stars. The stars and
/// the filter text are stored against the user and follow them across devices.
function OwnerPickField({ id, value, onChange, options, hint }: OwnerPickFieldProps) {
  const { prefs, setPref } = usePickerPrefs();
  const [typed, setTyped] = useState<string | null>(null);
  const filter = typed ?? prefs.ownerFilter;
  return (
    <PickListField
      id={id}
      label="Owner"
      required
      value={value}
      onChange={onChange}
      options={options}
      favorites={prefs.ownerFavorites}
      onToggleFavorite={(owner) => {
        setPref('ownerFavorites', toggleFavorite(prefs.ownerFavorites, owner));
      }}
      filter={filter}
      onFilterChange={setTyped}
      onFilterSettled={() => {
        if (filter !== prefs.ownerFilter) setPref('ownerFilter', filter);
      }}
      hint={hint}
    />
  );
}

/// Register a secret. The bytes ride this one request and are dropped with it: no route on the
/// registry returns a value, so the form clears itself the moment the server answers.
function RegisterSection({ owners }: { owners: readonly PickOption[] }) {
  const qc = useQueryClient();
  const register = $api.useMutation('post', '/api/secrets');
  const [entered, setForm] = useState<RegisterForm>(() => emptyRegisterForm(''));
  const form = withOwners(entered, owners);
  const [error, setError] = useState<string | null>(null);
  const [registered, setRegistered] = useState<string | null>(null);

  const errors = registerErrors(form);
  const warning = agentVisibleWarning(form);

  const submit = async () => {
    setError(null);
    setRegistered(null);
    try {
      const created = await register.mutateAsync({ body: registerBody(form) });
      setForm(emptyRegisterForm(form.owner));
      setRegistered(created.name);
      await qc.invalidateQueries({ queryKey: SECRETS_KEY });
    } catch (err: unknown) {
      setError(formatError(err));
    } finally {
      register.reset();
    }
  };

  return (
    <Section>
      <SectionHeader title="Register" />
      {owners.length === 0 ? (
        <SectionBody>
          <Empty
            title="SIGN IN TO REGISTER"
            description="A secret is owned by you or by one of your groups, both of which come from your session."
          />
        </SectionBody>
      ) : (
        <>
          <SectionBody>
            <FormGrid>
              <TextField
                id="secret-name"
                label="Name"
                mono
                required
                value={form.name}
                onChange={(name) => { setForm({ ...form, name }); }}
                hint="The name a pack declares and a binding satisfies."
                error={form.name.length > 0 ? (errors.get('name') ?? null) : null}
              />
              <OwnerPickField
                id="secret-owner"
                value={form.owner}
                onChange={(owner) => { setForm({ ...form, owner }); }}
                options={owners}
                hint="You, or a group in your validated claims. Every member of the owner can rotate and delete it; binding it also needs the operator role."
              />
              <SelectField
                id="secret-kind"
                label="Kind"
                required
                value={form.kind}
                onChange={(kind) => { setForm(withKind(form, optionValue(KIND_OPTIONS, kind, 'opaque'))); }}
                options={KIND_OPTIONS}
              />
              <SelectField
                id="secret-visibility"
                label="Visibility"
                required
                value={form.visibility}
                onChange={(visibility) => { setForm(withVisibility(form, optionValue(visibilityOptions(form.kind), visibility, 'broker_only'))); }}
                options={visibilityOptions(form.kind)}
              />
              <SelectField
                id="secret-mode"
                label="Source"
                required
                value={form.mode}
                onChange={(mode) => { setForm({ ...form, mode: optionValue(MODE_OPTIONS, mode, 'value') }); }}
                options={MODE_OPTIONS}
              />
            </FormGrid>
          </SectionBody>
          <SectionBody>
            {errors.has('kind') && <Note>{errors.get('kind')}</Note>}
            {form.mode === 'mint' ? (
              <SelectField
                id="secret-minter"
                label="Minter"
                required
                value={form.minter}
                onChange={(minter) => { setForm({ ...form, minter: optionValue(MINTER_OPTIONS, minter, 'github-app') }); }}
                options={MINTER_OPTIONS}
                hint="Nothing is stored. Every dispatch that binds this gets a credential issued then, and it lives no longer than the run."
              />
            ) : form.mode === 'value' && form.kind === 'inference_api_key' ? (
              <CredentialRows
                idPrefix="secret"
                entries={form.credentials}
                onChange={(credentials) => { setForm({ ...form, credentials }); }}
              />
            ) : form.mode === 'value' ? (
              <PasswordField
                id="secret-value"
                label="Value"
                required
                rows={isKeyShaped(form.kind) ? undefined : 6}
                value={form.value}
                onChange={(value) => { setForm({ ...form, value }); }}
                hint="Secrets are stored encrypted at rest in HashiCorp Vault."
              />
            ) : (
              <FormGrid>
                <TextField
                  id="secret-reference"
                  label="Vault reference"
                  mono
                  required
                  value={form.reference}
                  onChange={(reference) => { setForm({ ...form, reference }); }}
                  hint="vault://<mount>/<path>#<key>, at a path whose owner grants the hub read."
                  error={form.reference.length > 0 ? (errors.get('reference') ?? null) : null}
                />
                <PasswordField
                  id="secret-vault-token"
                  label="Your Vault token"
                  required
                  value={form.vaultToken}
                  onChange={(vaultToken) => { setForm({ ...form, vaultToken }); }}
                  hint="Used for one verification read of that path and then dropped; it is never stored."
                />
              </FormGrid>
            )}
          </SectionBody>
          {warning !== null && (
            <SectionBody>
              <Notice label="Agent-visible">{warning}</Notice>
              <CheckField
                id="secret-agent-visible-ack"
                label="I accept that the agent can read this value"
                checked={form.acknowledged}
                onChange={(acknowledged) => { setForm({ ...form, acknowledged }); }}
                description="The registration is refused here until this is ticked."
              />
            </SectionBody>
          )}
          {errors.get('visibility') !== undefined && (
            <SectionBody>
              <FormError>{errors.get('visibility')}</FormError>
            </SectionBody>
          )}
          {error !== null && (
            <SectionBody>
              <FormError>{error}</FormError>
            </SectionBody>
          )}
          {registered !== null && (
            <SectionBody>
              <Note>{`${registered} is registered.`}</Note>
            </SectionBody>
          )}
          <FormActions>
            <Button
              variant="filled"
              disabled={!canRegister(form) || register.isPending}
              onClick={() => { void submit(); }}
            >
              {register.isPending ? 'REGISTERING…' : 'REGISTER'}
            </Button>
          </FormActions>
        </>
      )}
    </Section>
  );
}

/// One secret: where its bytes are, what it is bound to, how it rotates, and what the registry
/// recorded about it. Still metadata only.
function SecretDetail({
  id,
  owners,
  onClosed,
}: {
  id: string;
  owners: readonly PickOption[];
  onClosed: () => void;
}) {
  const qc = useQueryClient();
  const detail = $api.useQuery('get', '/api/secrets/{id}', { params: { path: { id } } });
  const audit = $api.useQuery('get', '/api/secrets/{id}/audit', { params: { path: { id } } });
  const rotate = $api.useMutation('post', '/api/secrets/{id}/rotate');
  const transfer = $api.useMutation('put', '/api/secrets/{id}/owner');
  const remove = $api.useMutation('delete', '/api/secrets/{id}');
  const bind = $api.useMutation('post', '/api/secrets/{id}/bindings');
  const unbind = $api.useMutation('delete', '/api/secrets/{id}/bindings/{binding_id}');

  const [rotated, setRotated] = useState('');

  const [rotatedCredentials, setRotatedCredentials] = useState<CredentialEntry[]>(emptyCredentials);

  const [newOwner, setNewOwner] = useState('');
  const [bindForm, setBindForm] = useState<BindForm | null>(null);
  const [error, setError] = useState<string | null>(null);

  if (detail.isPending) return <LoadingBlock label="LOADING THE SECRET" />;
  if (detail.isError || detail.data === undefined) {
    return <Empty title="NO SUCH SECRET" description={formatError(detail.error)} />;
  }

  const secret = detail.data;
  const rotatedValue =
    secret.kind === 'inference_api_key' ? credentialsValue(rotatedCredentials) : rotated;
  const canRotate =
    secret.kind === 'inference_api_key'
      ? credentialErrors(rotatedCredentials).size === 0
      : rotated.length > 0;
  const bindings = secret.bindings;
  const blocker = deleteBlocker(bindings);
  const form = bindForm ?? emptyBindForm(secret.kind);
  const bindProblems = bindErrors(form, secret);
  const transferTargets = owners.filter((owner) => owner.value !== secret.owner);

  const refresh = async () => {
    await Promise.all([
      qc.invalidateQueries({ queryKey: SECRETS_KEY }),
      detail.refetch(),
      audit.refetch(),
    ]);
  };

  const run = async (
    mutation: { reset: () => void },
    action: () => Promise<unknown>,
    after: () => void,
  ) => {
    setError(null);
    try {
      await action();
      after();
      await refresh();
    } catch (err: unknown) {
      setError(formatError(err));
    } finally {
      mutation.reset();
    }
  };

  return (
    <>
      <Section>
        <SectionHeader
          title={secret.name}
          note={`${secret.owner} · ${secret.kind} · ${secret.visibility}`}
          actions={
            <Button onClick={onClosed}>CLOSE</Button>
          }
        />
        <SectionBody className="grid gap-1 font-mono text-data text-ink-2">
          <div>{originLabel(secret)}</div>
          <div>{`${secret.mode} · consumed by the ${secret.consumer}`}</div>
          <div>{`registered by ${secret.created_by ?? 'an anonymous caller'} at ${secret.created_at}`}</div>
        </SectionBody>
        {error !== null && (
          <SectionBody>
            <FormError>{error}</FormError>
          </SectionBody>
        )}
      </Section>

      <Section>
        <SectionHeader title="Bindings" note="what projects this secret into a run" />
        <SectionBody className="grid gap-2">
          {bindings.length === 0 ? (
            <Note>Nothing is bound to this secret, so no launch resolves it.</Note>
          ) : (
            bindings.map((b) => (
              <div key={b.id} className="flex items-center gap-3 font-mono text-data text-ink-2">
                <span className="flex-1">
                  {`${b.scope_kind} ${b.scope_id}: ${b.declared_name} as ${b.projection_kind} ${b.projection}`}
                </span>
                <Button
                  disabled={unbind.isPending}
                  onClick={() => {
                    void run(
                      unbind,
                      () =>
                        unbind.mutateAsync({
                          params: { path: { id, binding_id: b.id } },
                        }),
                      () => undefined,
                    );
                  }}
                >
                  UNBIND
                </Button>
              </div>
            ))
          )}
        </SectionBody>
        <SectionBody>
          <FormGrid>
            <SelectField
              id="bind-scope-kind"
              label="Scope"
              required
              value={form.scopeKind}
              onChange={(scopeKind) => { setBindForm({ ...form, scopeKind: optionValue(SCOPE_KIND_OPTIONS, scopeKind, 'repo') }); }}
              options={SCOPE_KIND_OPTIONS}
            />
            <TextField
              id="bind-scope-id"
              label="Scope id"
              mono
              required
              value={form.scopeId}
              onChange={(scopeId) => { setBindForm({ ...form, scopeId }); }}
              hint="owner/repo, a playbook id, or a domain name."
              error={form.scopeId.length > 0 ? (bindProblems.get('scopeId') ?? null) : null}
            />
            <TextField
              id="bind-declared-name"
              label="Declared name"
              mono
              value={form.declaredName}
              onChange={(declaredName) => { setBindForm({ ...form, declaredName }); }}
              hint={`The name the pack declares. Blank means ${secret.name}.`}
              error={bindProblems.get('declaredName') ?? null}
            />
            <SelectField
              id="bind-projection-kind"
              label="Projection"
              required
              value={form.projectionKind}
              onChange={(projectionKind) => {
                setBindForm({ ...form, projectionKind: optionValue(projectionKindOptions(secret.kind), projectionKind, 'env') });
              }}
              options={projectionKindOptions(secret.kind)}
            />
            <TextField
              id="bind-projection"
              label={form.projectionKind === 'env' ? 'Variable name' : 'File path'}
              mono
              required
              value={form.projection}
              onChange={(projection) => { setBindForm({ ...form, projection }); }}
              error={form.projection.length > 0 ? (bindProblems.get('projection') ?? null) : null}
            />
            {secret.mode === 'reference' && (
              <PasswordField
                id="bind-vault-token"
                label="Your Vault token"
                required
                value={form.vaultToken}
                onChange={(vaultToken) => { setBindForm({ ...form, vaultToken }); }}
                hint="A reference is re-verified on every bind with the binder's own token."
              />
            )}
          </FormGrid>
        </SectionBody>
        <FormActions>
          <Button
            variant="filled"
            disabled={!canBind(form, secret) || bind.isPending}
            onClick={() => {
              void run(
                bind,
                () =>
                  bind.mutateAsync({
                    params: { path: { id } },
                    body: bindBody(form, secret),
                  }),
                () => { setBindForm(emptyBindForm(secret.kind)); },
              );
            }}
          >
            {bind.isPending ? 'BINDING…' : 'BIND'}
          </Button>
          {bindProblems.get('projectionKind') !== undefined && (
            <FormError>{bindProblems.get('projectionKind')}</FormError>
          )}
        </FormActions>
      </Section>

      <Section>
        <SectionHeader title="Rotate" note="the next pod start picks up the new bytes" />
        <SectionBody>
          {secret.mode === 'minted' ? (
            <Note>
              A minted secret has no stored version to rotate. It is issued fresh at every
              dispatch, so the next run already gets a new credential.
            </Note>
          ) : secret.mode === 'reference' ? (
            <Note>
              A reference has no version of its own to rotate. Change the value at the path it
              points at, or register a new pointer.
            </Note>
          ) : secret.kind === 'inference_api_key' ? (
            <CredentialRows
              idPrefix="secret-rotate"
              entries={rotatedCredentials}
              onChange={setRotatedCredentials}
            />
          ) : (
            <PasswordField
              id="secret-rotate-value"
              label="New value"
              rows={isKeyShaped(secret.kind) ? undefined : 6}
              value={rotated}
              onChange={setRotated}
              hint="Written as a new Vault version. A pod already running keeps what it holds."
            />
          )}
        </SectionBody>
        {secret.mode === 'managed' && (
          <FormActions>
            <Button
              disabled={!canRotate || rotate.isPending}
              onClick={() => {
                void run(
                  rotate,
                  () => rotate.mutateAsync({ params: { path: { id } }, body: { value: rotatedValue } }),
                  () => { setRotated(''); setRotatedCredentials(emptyCredentials()); },
                );
              }}
            >
              {rotate.isPending ? 'ROTATING…' : 'ROTATE'}
            </Button>
          </FormActions>
        )}
      </Section>

      <Section>
        <SectionHeader title="Owner" note={secret.owner} />
        <SectionBody>
          {transferTargets.length === 0 ? (
            <Note>You hold no other principal to hand this secret to.</Note>
          ) : (
            <FormGrid>
              <OwnerPickField
                id="secret-transfer-owner"
                value={newOwner}
                onChange={setNewOwner}
                options={transferTargets}
                hint={
                  secret.mode === 'managed'
                    ? 'The value moves to the new owner\u2019s path and providers that name it follow. Bindings stay.'
                    : 'Providers that name it follow. Bindings stay.'
                }
              />
            </FormGrid>
          )}
        </SectionBody>
        {transferTargets.length > 0 && (
          <FormActions>
            <Button
              disabled={!transferTargets.some((owner) => owner.value === newOwner) || transfer.isPending}
              onClick={() => {
                void run(
                  transfer,
                  () =>
                    transfer.mutateAsync({
                      params: { path: { id } },
                      body: { owner: newOwner },
                    }),
                  () => { setNewOwner(''); },
                );
              }}
            >
              {transfer.isPending ? 'TRANSFERRING…' : 'TRANSFER'}
            </Button>
          </FormActions>
        )}
      </Section>

      <Section>
        <SectionHeader title="Delete" />
        <SectionBody>
          {blocker === null ? (
            <Note>Removes the registry row and destroys the managed bytes. There is no undo.</Note>
          ) : (
            <FormError>{blocker}</FormError>
          )}
        </SectionBody>
        <FormActions>
          <Button
            disabled={blocker !== null || remove.isPending}
            onClick={() => {
              void run(
                remove,
                () => remove.mutateAsync({ params: { path: { id } } }),
                onClosed,
              );
            }}
          >
            {remove.isPending ? 'DELETING…' : 'DELETE'}
          </Button>
        </FormActions>
      </Section>

      <Section>
        <SectionHeader title="Audit" note="every recorded action, newest first" />
        <SectionBody className="grid gap-1 font-mono text-data text-ink-3">
          {audit.isPending ? (
            <LoadingBlock label="LOADING THE AUDIT TRAIL" />
          ) : audit.data === undefined || audit.data.length === 0 ? (
            <Note>Nothing recorded yet.</Note>
          ) : (
            audit.data.map((row) => (
              <div key={row.id}>
                {`${row.at} · ${row.action} · ${row.actor ?? 'the hub'}${row.detail === null || row.detail === undefined ? '' : ` · ${row.detail}`}`}
              </div>
            ))
          )}
        </SectionBody>
      </Section>
    </>
  );
}

/// The registry as its owners see it: the secrets they own, the ones their groups own, and the
/// register/rotate/delete/bind controls over them. No value is ever on this page after a submit —
/// the API has no route that returns one.
export function SecretsPage() {
  const whoami = $api.useQuery('get', '/api/whoami');
  const secrets = $api.useQuery('get', '/api/secrets');
  const [selected, setSelected] = useState<string | null>(null);

  const ownerContext = useOwnerContext();
  const rows = narrow(secrets.data ?? NO_SECRETS, ownerContext.context, (row) => row.owner);
  const owners = principalOptions(ownerContext.owners);
  const groups = whoami.isPending ? null : groupSecrets(rows, whoami.data?.user);

  return (
    <>
      <PageHeader eyebrow="System" title="Secrets" />

      {secrets.isError ? (
        <Empty title="SECRETS UNAVAILABLE" description={formatError(secrets.error)} />
      ) : secrets.isPending || groups === null ? (
        <LoadingBlock label="LOADING SECRETS" />
      ) : (
        <>
          <Section>
            <SectionHeader title="Personal" />
            <SecretTable
              rows={groups.own}
              onOpen={setSelected}
              emptyTitle="NO SECRETS OF YOUR OWN"
              emptyDescription="Register one below to bind it to a repo, playbook, or domain."
            />
          </Section>

          <Section>
            <SectionHeader title="Your teams'" />
            <SecretTable
              rows={groups.team}
              onOpen={setSelected}
              emptyTitle="NO TEAM SECRETS"
              emptyDescription="A secret owned by one of your groups shows up here for every member."
            />
          </Section>
        </>
      )}

      {selected !== null && (
        <SecretDetail id={selected} owners={owners} onClosed={() => { setSelected(null); }} />
      )}

      <RegisterSection owners={owners} />
    </>
  );
}
