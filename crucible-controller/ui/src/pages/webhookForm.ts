import type { components } from '../api/schema.d';
import type { ParamFieldSpec } from './playbookLaunchForm';

export type WebhookBody = components['schemas']['WebhookBody'];
export type WebhookPreviewBody = components['schemas']['WebhookPreviewBody'];
export type WebhookPresetDto = components['schemas']['WebhookPresetDto'];

export type Verifier = 'path_token' | 'hmac_sha256';

/// How one declared param gets its value: fixed at save, or derived from each delivery.
export interface ParamChoice {
  mode: 'fixed' | 'derived';
  value: string;
  expression: string;
}

export interface WebhookFormState {
  verifier: Verifier;
  header: string;
  filter: string;
  dedupe: string;
  choices: Record<string, ParamChoice>;
  maxLaunchesPerHour: number;
  retentionDays: number;
  maxCost: number;
  maxTime: string;
  dispatchTarget: string;
}

export function defaultChoices(specs: readonly ParamFieldSpec[]): Record<string, ParamChoice> {
  const choices: Record<string, ParamChoice> = {};
  for (const spec of specs) {
    choices[spec.name] = { mode: 'fixed', value: spec.defaultValue ?? '', expression: '' };
  }
  return choices;
}

export function initialState(specs: readonly ParamFieldSpec[]): WebhookFormState {
  return {
    verifier: 'path_token',
    header: '',
    filter: 'true',
    dedupe: 'delivery',
    choices: defaultChoices(specs),
    maxLaunchesPerHour: 10,
    retentionDays: 14,
    maxCost: 5,
    maxTime: '30m',
    dispatchTarget: '',
  };
}

export interface PresetApplied {
  state: WebhookFormState;
  /// Derivations the preset suggests for params this playbook does not declare.
  unmatched: string[];
}

/// Fill the form from a preset: its verifier and transform, and a derivation for each declared param
/// the preset names. A param the preset does not name keeps whatever the form had.
export function applyPreset(
  preset: WebhookPresetDto,
  specs: readonly ParamFieldSpec[],
  state: WebhookFormState
): PresetApplied {
  const declared = new Set(specs.map((s) => s.name));
  const choices = { ...state.choices };
  const unmatched: string[] = [];
  for (const [name, expression] of Object.entries(preset.derive)) {
    const current = choices[name];
    if (declared.has(name) && current !== undefined) {
      choices[name] = { ...current, mode: 'derived', expression };
    } else {
      unmatched.push(name);
    }
  }
  return {
    state: {
      ...state,
      verifier: preset.verifier === 'hmac_sha256' ? 'hmac_sha256' : 'path_token',
      header: preset.header ?? '',
      filter: preset.filter,
      dedupe: preset.dedupe,
      choices,
    },
    unmatched,
  };
}

function split(choices: Record<string, ParamChoice>): {
  params: Record<string, string>;
  derive: Record<string, string>;
} {
  const params: Record<string, string> = {};
  const derive: Record<string, string> = {};
  for (const [name, choice] of Object.entries(choices)) {
    if (choice.mode === 'derived') {
      if (choice.expression.trim().length > 0) derive[name] = choice.expression.trim();
    } else if (choice.value.trim().length > 0) {
      params[name] = choice.value.trim();
    }
  }
  return { params, derive };
}

/// The create body. Blank fixed values are omitted so a param's default applies, and blank
/// derivations are omitted rather than sent as empty expressions.
export function createBody(playbook: string, schemaDigest: string, state: WebhookFormState): WebhookBody {
  const { params, derive } = split(state.choices);
  return {
    playbook,
    schema_digest: schemaDigest,
    verifier: state.verifier,
    header: state.verifier === 'hmac_sha256' ? state.header.trim() : null,
    filter: state.filter.trim(),
    dedupe: state.dedupe.trim(),
    derive,
    params,
    max_launches_per_hour: state.maxLaunchesPerHour,
    retention_days: state.retentionDays,
    max_cost: state.maxCost,
    max_time: state.maxTime.trim(),
    dispatch_target: state.dispatchTarget === '' ? null : state.dispatchTarget,
  };
}

/// `Name: value` lines as a header map. Blank lines are skipped; a line without a colon is an error.
export function parseHeaders(text: string): { headers: Record<string, string> } | { error: string } {
  const headers: Record<string, string> = {};
  for (const [index, raw] of text.split('\n').entries()) {
    const line = raw.trim();
    if (line.length === 0) continue;
    const colon = line.indexOf(':');
    if (colon <= 0) return { error: `line ${index + 1} is not "Name: value"` };
    headers[line.slice(0, colon).trim().toLowerCase()] = line.slice(colon + 1).trim();
  }
  return { headers };
}

/// The preview body for the form's transform and a sample delivery.
export function previewBody(
  state: WebhookFormState,
  sample: string,
  headerLines: string
): { body: WebhookPreviewBody } | { error: string } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(sample);
  } catch (err: unknown) {
    return { error: `The sample body is not JSON: ${err instanceof Error ? err.message : String(err)}` };
  }
  const headers = parseHeaders(headerLines);
  if ('error' in headers) return { error: `Headers: ${headers.error}` };
  const { derive } = split(state.choices);
  return {
    body: {
      filter: state.filter.trim(),
      dedupe: state.dedupe.trim(),
      derive,
      sample: parsed,
      headers: headers.headers,
    },
  };
}

export type WebhookDto = components['schemas']['WebhookDto'];

/// The form for editing a stored webhook: a param it derives is derived, one it fixes keeps its value,
/// and any other declared param starts at its default.
export function stateFromWebhook(webhook: WebhookDto, specs: readonly ParamFieldSpec[]): WebhookFormState {
  const choices = defaultChoices(specs);
  const derive = isStringMap(webhook.derive) ? webhook.derive : {};
  const params = isStringMap(webhook.params) ? webhook.params : {};
  for (const spec of specs) {
    const derived = derive[spec.name];
    const fixed = params[spec.name];
    if (derived !== undefined) choices[spec.name] = { mode: 'derived', value: '', expression: derived };
    else if (fixed !== undefined) choices[spec.name] = { mode: 'fixed', value: fixed, expression: '' };
  }
  return {
    verifier: webhook.verifier === 'hmac_sha256' ? 'hmac_sha256' : 'path_token',
    header: webhook.header ?? '',
    filter: webhook.filter,
    dedupe: webhook.dedupe,
    choices,
    maxLaunchesPerHour: webhook.max_launches_per_hour,
    retentionDays: webhook.retention_days,
    maxCost: webhook.max_cost,
    maxTime: webhook.max_time,
    dispatchTarget: webhook.dispatch_target ?? '',
  };
}

function isStringMap(value: unknown): value is Record<string, string> {
  return (
    typeof value === 'object' &&
    value !== null &&
    !Array.isArray(value) &&
    Object.values(value).every((v) => typeof v === 'string')
  );
}

/// A recorded delivery's headers as the preview's `Name: value` lines.
export function headerLines(headers: unknown): string {
  if (!isStringMap(headers)) return '';
  return Object.entries(headers)
    .map(([name, value]) => `${name}: ${value}`)
    .join('\n');
}
