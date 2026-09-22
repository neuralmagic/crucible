import type { components } from '../api/schema.d';
import type { PickOption } from './pickList';

export type SecretDto = components['schemas']['SecretDto'];
export type SecretBindingDto = components['schemas']['SecretBindingDto'];
export type RegisterSecretBody = components['schemas']['RegisterSecretBody'];
export type BindSecretBody = components['schemas']['BindSecretBody'];
export type DeclaredSecretDto = components['schemas']['DeclaredSecretDto'];
export type PackSecretsDto = components['schemas']['PackSecretsDto'];
export type SecretKind = components['schemas']['SecretKind'];
export type Visibility = components['schemas']['Visibility'];
export type ScopeKind = components['schemas']['ScopeKind'];
export type ProjectionKind = components['schemas']['ProjectionKind'];

/// The longest name the registry stores, and the character set it stores it in. Both are the
/// server's; repeating them here is what lets the form refuse before a round trip.
const MAX_NAME = 96;
const NAME_CHARS = /^[a-z0-9._-]+$/;

export const KIND_OPTIONS: readonly { value: SecretKind; label: string }[] = [
  { value: 'opaque', label: 'opaque (a token, key, or header value)' },
  { value: 'file', label: 'file (content written into the pod)' },
  { value: 'registry_authfile', label: 'registry_authfile (in-pod pulls and pushes)' },
  { value: 'kubeconfig', label: 'kubeconfig (a cluster the hub calls)' },
  { value: 'inference_api_key', label: 'inference_api_key (a model provider\'s credentials)' },
];

/// One entry of an inference key's credentials map: the environment variable it lands in, and
/// its value. The map is OpenShell's provider `credentials` shape, so what is registered here can
/// be handed to its gateway as one provider record.
export interface CredentialEntry {
  name: string;
  value: string;
}

/// The variable a provider's harness reads its key from, offered as the first row so a single-key
/// registration needs no typing beyond the key.
export const CREDENTIAL_PRESETS: readonly { label: string; name: string }[] = [
  { label: 'OpenAI / OpenAI-compatible', name: 'OPENAI_API_KEY' },
  { label: 'Anthropic / Messages-compatible', name: 'ANTHROPIC_API_KEY' },
];

const ENV_NAME = /^[A-Z_][A-Z0-9_]*$/;

export function emptyCredentials(): CredentialEntry[] {
  return [{ name: 'OPENAI_API_KEY', value: '' }];
}

/// The map as the registry stores it: one JSON object, variable to value, keys trimmed. Rows
/// with neither a name nor a value are blank rows the writer never filled and are dropped.
export function credentialsValue(entries: readonly CredentialEntry[]): string {
  const map: Record<string, string> = {};
  for (const entry of entries) {
    const name = entry.name.trim();
    if (name.length === 0 && entry.value.length === 0) continue;
    map[name] = entry.value;
  }
  return JSON.stringify(map);
}

/// Every refusal the credentials rows would earn from the server, keyed by row index.
export function credentialErrors(entries: readonly CredentialEntry[]): Map<number, string> {
  const errors = new Map<number, string>();
  const seen = new Set<string>();
  let filled = 0;
  entries.forEach((entry, index) => {
    const name = entry.name.trim();
    if (name.length === 0 && entry.value.length === 0) return;
    filled += 1;
    if (!ENV_NAME.test(name)) {
      errors.set(index, `"${name}" is not an environment variable name ([A-Z_][A-Z0-9_]*)`);
    } else if (seen.has(name)) {
      errors.set(index, `${name} is named twice`);
    } else if (entry.value.length === 0) {
      errors.set(index, `${name} has no value`);
    }
    seen.add(name);
  });
  if (filled === 0 && entries.length > 0) errors.set(0, 'name at least one variable');
  return errors;
}

export const SCOPE_KIND_OPTIONS: readonly { value: ScopeKind; label: string }[] = [
  { value: 'repo', label: 'repo' },
  { value: 'playbook', label: 'playbook' },
  { value: 'domain', label: 'domain' },
];

/// Only an opaque value can be shown to the agent; every file-shaped kind is broker-side only, so
/// the choice is not offered rather than offered and refused.
export function visibilityOptions(kind: SecretKind): readonly { value: Visibility; label: string }[] {
  const brokerOnly: { value: Visibility; label: string } = { value: 'broker_only', label: 'broker_only (the agent never sees it)' };
  if (kind !== 'opaque') return [brokerOnly];
  return [brokerOnly, { value: 'agent_visible', label: 'agent_visible (the agent can read it)' }];
}

/// Whether a kind's value is one line of text rather than a document. An inference key is a
/// credentials map, edited as rows rather than as a value at all.
export function isKeyShaped(kind: SecretKind): boolean {
  return kind === 'opaque' || kind === 'inference_api_key';
}

/// A file-shaped kind is written to a path. An environment variable would be echoed by anything
/// that dumps the environment, so it is not on the menu for those kinds.
export function projectionKindOptions(kind: SecretKind): readonly { value: ProjectionKind; label: string }[] {
  const file: { value: ProjectionKind; label: string } = { value: 'file', label: 'file (a path in the pod)' };
  if (!isKeyShaped(kind)) return [file];
  return [{ value: 'env', label: 'env (an environment variable)' }, file];
}

/// Where a registration gets its bytes: typed into the form, pointed at a path someone else owns
/// in Vault, or issued fresh at every dispatch by a minter and never stored at all.
export type RegisterMode = 'value' | 'reference' | 'mint';

/// The minters the registry can issue from. One so far; the select exists so a second needs no new
/// shape here.
export const MINTERS = ['github-app'] as const;
export type Minter = (typeof MINTERS)[number];

export interface RegisterForm {
  name: string;
  owner: string;
  kind: SecretKind;
  visibility: Visibility;
  mode: RegisterMode;
  value: string;
  /// The rows an inference key's value is assembled from; the other kinds ignore them.
  credentials: CredentialEntry[];
  reference: string;
  minter: Minter;
  vaultToken: string;
  /// Whether the writer has acknowledged that an agent-visible value can end up in a PR.
  acknowledged: boolean;
}

export function emptyRegisterForm(owner: string): RegisterForm {
  return {
    name: '',
    owner,
    kind: 'opaque',
    visibility: 'broker_only',
    mode: 'value',
    value: '',
    credentials: emptyCredentials(),
    reference: '',
    minter: 'github-app',
    vaultToken: '',
    acknowledged: false,
  };
}

/// The owner a form actually registers under. The options arrive with the session, which lands
/// after the first render, so a form that still holds an owner nobody offers falls back to the
/// first one rather than submitting a principal the caller does not hold.
export function withOwners(form: RegisterForm, owners: readonly PickOption[]): RegisterForm {
  if (owners.some((owner) => owner.value === form.owner)) return form;
  return { ...form, owner: owners[0]?.value ?? '' };
}

/// Changing the kind can invalidate the visibility that was chosen under the previous one, and an
/// acknowledgement only ever covers the choice that was on screen when it was given.
export function withKind(form: RegisterForm, kind: SecretKind): RegisterForm {
  const stillAgentVisible = kind === 'opaque' && form.visibility === 'agent_visible';
  return {
    ...form,
    kind,
    visibility: stillAgentVisible ? 'agent_visible' : 'broker_only',
    acknowledged: stillAgentVisible && form.acknowledged,
  };
}

export function withVisibility(form: RegisterForm, visibility: Visibility): RegisterForm {
  return { ...form, visibility, acknowledged: false };
}

/// The warning an agent-visible registration has to clear before it can be saved. Null whenever
/// the value stays broker-side, which is every other combination.
export function agentVisibleWarning(form: RegisterForm): string | null {
  if (form.visibility !== 'agent_visible') return null;
  return (
    'The agent inside the sandbox can read this value, so it can end up in a pull request, a ' +
    'transcript, or a commit. Register it broker_only unless the pack genuinely needs the agent ' +
    'to hold it.'
  );
}

/// The name field's refusal, in the server's own terms: the same length cap and character set the
/// registry parses with, so a name that passes here is a name the API accepts.
export function nameError(raw: string): string | null {
  const name = raw.trim().toLowerCase();
  if (name.length === 0) return 'a secret name cannot be empty';
  if (name.length > MAX_NAME) return `a secret name is at most ${MAX_NAME} characters`;
  if (!NAME_CHARS.test(name)) return `secret name "${name}" has a character outside [a-z0-9._-]`;
  return null;
}

/// Every field-level refusal on the register form, keyed by the field it belongs against.
export function registerErrors(form: RegisterForm): Map<string, string> {
  const errors = new Map<string, string>();
  const name = nameError(form.name);
  if (name !== null) errors.set('name', name);
  if (form.owner.trim().length === 0) {
    errors.set('owner', 'sign in to own a secret');
  }
  if (form.mode === 'value') {
    if (form.kind === 'inference_api_key') {
      const first = credentialErrors(form.credentials).values().next();
      if (!first.done) errors.set('value', first.value);
    } else if (form.value.length === 0) {
      errors.set('value', 'a registered value cannot be empty');
    }
  } else if (form.mode === 'mint') {
    if (form.kind !== 'opaque') {
      errors.set('kind', `a ${form.kind} secret cannot be minted; a minter issues an opaque token`);
    }
  } else {
    if (form.reference.trim().length === 0) {
      errors.set('reference', 'a reference is a vault://<mount>/<path>#<key> pointer');
    } else if (!form.reference.trim().startsWith('vault://')) {
      errors.set('reference', 'a reference starts with vault://');
    }
    if (form.vaultToken.trim().length === 0) {
      errors.set('vaultToken', 'a reference is verified with your own Vault token');
    }
  }
  if (form.visibility === 'agent_visible' && form.kind !== 'opaque') {
    errors.set('visibility', `a ${form.kind} secret is broker-side only and cannot be agent_visible`);
  }
  return errors;
}

/// Whether REGISTER may fire: every field valid, and the agent-visible warning acknowledged when
/// there is one. The acknowledgement is a gate, not a nag — an unacknowledged warning blocks.
export function canRegister(form: RegisterForm): boolean {
  if (registerErrors(form).size > 0) return false;
  return agentVisibleWarning(form) === null || form.acknowledged;
}

/// The registration body. Exactly one of `value`, `reference`, and `mint` rides, and the Vault
/// token rides only with a reference — nothing here is kept after the request.
export function registerBody(form: RegisterForm): RegisterSecretBody {
  const base = {
    name: form.name.trim().toLowerCase(),
    owner: form.owner,
    kind: form.kind,
    visibility: form.visibility,
  };
  if (form.mode === 'value') {
    const value = form.kind === 'inference_api_key' ? credentialsValue(form.credentials) : form.value;
    return { ...base, value };
  }
  if (form.mode === 'mint') return { ...base, mint: form.minter };
  return { ...base, reference: form.reference.trim(), vault_token: form.vaultToken.trim() };
}

export interface BindForm {
  scopeKind: ScopeKind;
  scopeId: string;
  declaredName: string;
  projectionKind: ProjectionKind;
  projection: string;
  vaultToken: string;
}

export function emptyBindForm(kind: SecretKind): BindForm {
  return {
    scopeKind: 'repo',
    scopeId: '',
    declaredName: '',
    projectionKind: isKeyShaped(kind) ? 'env' : 'file',
    projection: '',
    vaultToken: '',
  };
}

/// The bind form's refusals. A reference is re-verified on every bind, so the binder's own Vault
/// token is required here the same way it was at registration.
export function bindErrors(form: BindForm, secret: SecretDto): Map<string, string> {
  const errors = new Map<string, string>();
  if (form.scopeId.trim().length === 0) errors.set('scopeId', 'a binding needs a scope id');
  if (form.projection.trim().length === 0) {
    errors.set('projection', 'a binding needs a projection');
  }
  if (form.declaredName.trim().length > 0) {
    const declared = nameError(form.declaredName);
    if (declared !== null) errors.set('declaredName', declared);
  }
  if (secret.kind !== 'opaque' && form.projectionKind === 'env') {
    errors.set('projectionKind', `a ${secret.kind} secret is projected as a file, not an environment variable`);
  }
  if (secret.mode === 'reference' && form.vaultToken.trim().length === 0) {
    errors.set('vaultToken', 'binding a reference needs your own Vault token');
  }
  return errors;
}

export function canBind(form: BindForm, secret: SecretDto): boolean {
  return bindErrors(form, secret).size === 0;
}

/// The bind body. A blank declared name means the secret's own name, which is what the server
/// defaults to, so it is left off rather than sent as an empty string.
export function bindBody(form: BindForm, secret: SecretDto): BindSecretBody {
  const declared = form.declaredName.trim().toLowerCase();
  const body: BindSecretBody = {
    scope_kind: form.scopeKind,
    scope_id: form.scopeId.trim(),
    projection_kind: form.projectionKind,
    projection: form.projection.trim(),
  };
  if (declared.length > 0) body.declared_name = declared;
  if (secret.mode === 'reference') body.vault_token = form.vaultToken.trim();
  return body;
}

export interface SecretGroups {
  /// Secrets the signed-in caller owns personally.
  own: SecretDto[];
  /// Secrets owned by a group the caller is in.
  team: SecretDto[];
}

/// Split the registry listing into what is the caller's and what is their teams'. The server has
/// already filtered to principals the caller holds, so anything not owned `user:<them>` is a team
/// secret they are a member of.
export function groupSecrets(rows: readonly SecretDto[], user: string | null | undefined): SecretGroups {
  const mine = typeof user === 'string' ? `user:${user.trim().toLowerCase()}` : null;
  const own: SecretDto[] = [];
  const team: SecretDto[] = [];
  for (const row of rows) {
    if (mine !== null && row.owner === mine) own.push(row);
    else team.push(row);
  }
  return { own, team };
}

/// What a row says about where its bytes live: the KV version a managed secret is at, the pointer
/// a reference holds, or the minter a minted secret is issued by. Never a value — no route serves
/// one.
export function originLabel(secret: SecretDto): string {
  if (secret.mode === 'minted') return `${secret.vault_path} (issued at every dispatch)`;
  if (secret.mode === 'reference') return secret.vault_path;
  return secret.current_version === null || secret.current_version === undefined
    ? secret.vault_path
    : `${secret.vault_path} @ v${secret.current_version}`;
}

/// Why DELETE is refused before it is pressed: a bound secret has to be unbound first, and the
/// message names what is holding it.
export function deleteBlocker(bindings: readonly SecretBindingDto[]): string | null {
  if (bindings.length === 0) return null;
  const scopes = bindings.map((b) => `${b.scope_kind} ${b.scope_id}`).join(', ');
  return `still bound to ${scopes}; unbind it first`;
}

/// A declared name as the preview gate and the import review show it: the name the pack asks for,
/// and the projection it expects when the manifest named one.
export function declaredLabel(declared: DeclaredSecretDto): string {
  const projection = declared.projection ?? null;
  const kind = declared.projection_kind ?? null;
  if (projection === null || kind === null) return `${declared.name} (${declared.kind})`;
  return `${declared.name} (${declared.kind}) → ${kind} ${projection}`;
}

/// Whether the gate has a secrets section to draw at all.
export function hasDeclaredSecrets(secrets: PackSecretsDto | null | undefined): boolean {
  return secrets !== null && secrets !== undefined && secrets.declared.length > 0;
}
