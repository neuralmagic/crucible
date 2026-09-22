import type { components } from '../api/schema';
import type { PickOption } from './pickList';

export type ProviderDetailDto = components['schemas']['ProviderDetailDto'];
export type ProviderKind = components['schemas']['ProviderKind'];
export type InferenceProtocol = components['schemas']['InferenceProtocol'];
export type ProviderBody = components['schemas']['ProviderBody'];
export type RegisterProviderBody = components['schemas']['RegisterProviderBody'];

export const KIND_OPTIONS: readonly { value: ProviderKind; label: string }[] = [
  { value: 'openai', label: 'openai (api.openai.com, runs Codex by default)' },
  { value: 'anthropic', label: 'anthropic (api.anthropic.com, runs Claude Code by default)' },
  { value: 'vertex', label: 'vertex (Claude on Vertex, the deploy profile\'s ADC)' },
  { value: 'custom', label: 'custom (your own endpoint: vLLM, a proxy, a gateway)' },
];

export const PROTOCOL_OPTIONS: readonly { value: InferenceProtocol; label: string }[] = [
  { value: 'chat_completions', label: 'chat_completions (OpenAI Chat Completions, runs OpenCode by default)' },
  { value: 'responses', label: 'responses (OpenAI Responses / harmony, runs Codex by default)' },
  { value: 'messages', label: 'messages (Anthropic Messages, runs Claude Code by default)' },
];

/// The agent CLIs a registration may run its models under, default first: the server's own table
/// (`allowed_harnesses`), mirrored so the form offers only what the API accepts.
const KIND_HARNESSES: Record<Exclude<ProviderKind, 'custom'>, readonly string[]> = {
  anthropic: ['claude', 'opencode', 'pi'],
  vertex: ['claude', 'hermes'],
  openai: ['codex', 'opencode', 'pi'],
};

const PROTOCOL_HARNESSES: Record<InferenceProtocol, readonly string[]> = {
  messages: ['claude', 'opencode', 'pi'],
  chat_completions: ['opencode', 'pi'],
  responses: ['codex', 'pi'],
};

export function allowedHarnesses(kind: ProviderKind, protocol: InferenceProtocol): readonly string[] {
  return kind === 'custom' ? PROTOCOL_HARNESSES[protocol] : KIND_HARNESSES[kind];
}

export function defaultHarness(kind: ProviderKind, protocol: InferenceProtocol): string {
  return allowedHarnesses(kind, protocol)[0];
}

const HARNESS_LABELS: Record<string, string> = {
  claude: 'Claude Code',
  hermes: 'Hermes',
  codex: 'Codex',
  opencode: 'OpenCode',
  pi: 'Pi',
};

/// The harness picker's options: the kind's or protocol's default under a blank value, then each
/// harness that can speak to the service.
export function harnessOptions(kind: ProviderKind, protocol: InferenceProtocol): readonly PickOption[] {
  const fallback = defaultHarness(kind, protocol);
  return [
    { value: '', label: `default (${HARNESS_LABELS[fallback] ?? fallback})` },
    ...allowedHarnesses(kind, protocol).map((h) => ({ value: h, label: `${h} (${HARNESS_LABELS[h] ?? h})` })),
  ];
}

/// The harness a form's current choice resolves to once the kind or protocol moves: kept when the
/// new service can run it, otherwise back to the default.
export function harnessFor(kind: ProviderKind, protocol: InferenceProtocol, chosen: string): string {
  return allowedHarnesses(kind, protocol).includes(chosen) ? chosen : '';
}

/// The variable a provider's harness reads its key from, which its secret has to carry.
export function keyVariable(kind: ProviderKind, protocol: InferenceProtocol): string | null {
  switch (kind) {
    case 'anthropic':
      return 'ANTHROPIC_API_KEY';
    case 'openai':
      return 'OPENAI_API_KEY';
    case 'vertex':
      return null;
    case 'custom':
      return protocol === 'messages' ? 'ANTHROPIC_API_KEY' : 'OPENAI_API_KEY';
  }
}

export interface ProviderForm {
  id: string;
  displayName: string;
  kind: ProviderKind;
  /// One model per line; blank takes the kind's curated list (custom has none).
  models: string;
  defaultModel: string;
  endpoint: string;
  protocol: InferenceProtocol;
  /// The agent CLI to run under; blank takes the kind's or protocol's default.
  harness: string;
  secretName: string;
  secretOwner: string;
  enabled: boolean;
}

export function emptyProviderForm(): ProviderForm {
  return {
    id: '',
    displayName: '',
    kind: 'openai',
    models: '',
    defaultModel: '',
    endpoint: '',
    protocol: 'chat_completions',
    harness: '',
    secretName: '',
    secretOwner: '',
    enabled: true,
  };
}

/// A registration, loaded back into the form for editing.
export function formOf(provider: ProviderDetailDto): ProviderForm {
  return {
    id: provider.id,
    displayName: provider.display_name,
    kind: provider.kind,
    models: provider.models.join('\n'),
    defaultModel: provider.default_model,
    endpoint: provider.endpoint ?? '',
    protocol: provider.protocol ?? 'chat_completions',
    harness: provider.harness_override ?? '',
    secretName: provider.secret_name ?? '',
    secretOwner: provider.secret_owner ?? '',
    enabled: provider.enabled,
  };
}

const ID = /^[a-z0-9-]+$/;
const MODEL = /^[A-Za-z0-9][A-Za-z0-9._:/-]*$/;

export function modelList(raw: string): string[] {
  return raw
    .split(/[\n,]/)
    .map((m) => m.trim())
    .filter((m) => m.length > 0);
}

/// Every field-level refusal, in the server's terms, so a form that passes here is one the API
/// accepts.
export function providerErrors(form: ProviderForm, editing: boolean): Map<string, string> {
  const errors = new Map<string, string>();
  const id = form.id.trim();
  if (!editing) {
    if (id.length === 0) errors.set('id', 'an id is required');
    else if (id.length > 64) errors.set('id', 'an id is at most 64 characters');
    else if (!ID.test(id)) errors.set('id', 'an id is lowercase letters, digits, and dashes');
  }
  if (form.displayName.trim().length === 0) errors.set('displayName', 'a display name is required');
  const bad = modelList(form.models).find((m) => !MODEL.test(m));
  if (bad !== undefined) errors.set('models', `"${bad}" is not a model name`);
  const defaultModel = form.defaultModel.trim();
  if (defaultModel.length > 0 && !MODEL.test(defaultModel)) {
    errors.set('defaultModel', `"${defaultModel}" is not a model name`);
  }
  if (form.kind === 'custom') {
    if (defaultModel.length === 0) {
      errors.set('defaultModel', 'a custom provider has to name its default model');
    }
    const endpoint = form.endpoint.trim();
    if (endpoint.length === 0) {
      errors.set('endpoint', 'a custom provider needs the base URL it is reached at');
    } else if (!/^https?:\/\/\S+$/.test(endpoint)) {
      errors.set('endpoint', 'an endpoint is an absolute http(s) URL');
    }
  }
  if (form.harness.length > 0 && !allowedHarnesses(form.kind, form.protocol).includes(form.harness)) {
    errors.set(
      'harness',
      `${form.harness} cannot run this provider; one of ${allowedHarnesses(form.kind, form.protocol).join(', ')}`,
    );
  }
  if (form.kind === 'vertex' && form.secretName.trim().length > 0) {
    errors.set('secretName', 'a vertex provider runs on the deploy profile\'s ADC and takes no secret');
  }
  if (form.secretOwner.trim().length > 0 && form.secretName.trim().length === 0) {
    errors.set('secretOwner', 'an owner needs a secret name to qualify');
  }
  return errors;
}

/// The body a form submits. The fields a kind has no use for stay off the wire so the server's
/// own checks read a registration, not a form.
export function providerBody(form: ProviderForm): ProviderBody {
  const models = modelList(form.models);
  const defaultModel = form.defaultModel.trim();
  const secretName = form.secretName.trim();
  const secretOwner = form.secretOwner.trim();
  return {
    display_name: form.displayName.trim(),
    kind: form.kind,
    models,
    ...(defaultModel.length > 0 ? { default_model: defaultModel } : {}),
    ...(form.kind === 'custom'
      ? { endpoint: form.endpoint.trim(), protocol: form.protocol }
      : {}),
    ...(form.harness.length > 0 ? { harness: form.harness } : {}),
    ...(secretName.length > 0 ? { secret_name: secretName } : {}),
    ...(secretName.length > 0 && secretOwner.length > 0 ? { secret_owner: secretOwner } : {}),
    enabled: form.enabled,
  };
}

export function registerProviderBody(form: ProviderForm, owner: string): RegisterProviderBody {
  return { id: form.id.trim(), owner, ...providerBody(form) };
}

/// One line for the list: what the provider speaks and where.
export function reachLabel(provider: ProviderDetailDto): string {
  if (provider.kind === 'custom') {
    return `${provider.protocol ?? '?'} at ${provider.endpoint ?? '?'}`;
  }
  return provider.kind;
}
