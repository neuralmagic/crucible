import { describe, expect, it } from 'vitest';
import type { ParamFieldSpec } from './playbookLaunchForm';
import {
  applyPreset,
  createBody,
  customState,
  headerLines,
  stateFromWebhook,
  initialState,
  parseHeaders,
  previewBody,
  type WebhookDto,
  type WebhookFormState,
  type WebhookPresetDto,
} from './webhookForm';

const SPECS: ParamFieldSpec[] = [
  { name: 'image', valueType: 'string', required: true, defaultValue: null, pattern: '^quay\\.io/', description: null },
  { name: 'tags', valueType: 'list', required: false, defaultValue: '["latest"]', pattern: null, description: null },
  { name: 'release', valueType: 'string', required: false, defaultValue: 'main', pattern: null, description: null },
];

const QUAY: WebhookPresetDto = {
  id: 'quay-push',
  title: 'quay.io repository push',
  verifier: 'path_token',
  header: null,
  filter: 'size(body.updated_tags) > 0',
  dedupe: 'delivery',
  derive: { repository: 'body.repository', image: 'body.docker_url', tags: 'body.updated_tags' },
};

describe('applyPreset', () => {
  it('derives the declared params the preset names and reports the rest', () => {
    const { state, unmatched } = applyPreset(QUAY, SPECS, initialState(SPECS));
    expect(state.choices.image).toMatchObject({ mode: 'derived', expression: 'body.docker_url' });
    expect(state.choices.tags).toMatchObject({ mode: 'derived', expression: 'body.updated_tags' });
    expect(state.choices.release).toMatchObject({ mode: 'fixed', value: 'main' });
    expect(unmatched).toEqual(['repository']);
    expect(state).toMatchObject({ verifier: 'path_token', header: '', filter: QUAY.filter, dedupe: 'delivery' });
  });

  it("takes a signed preset's header", () => {
    const github: WebhookPresetDto = { ...QUAY, verifier: 'hmac_sha256', header: 'x-hub-signature-256', derive: {} };
    const { state } = applyPreset(github, SPECS, initialState(SPECS));
    expect(state).toMatchObject({ verifier: 'hmac_sha256', header: 'x-hub-signature-256' });
  });
});

describe('customState', () => {
  it('starts the transform over after a preset and keeps the sender and bounds', () => {
    const github: WebhookPresetDto = { ...QUAY, verifier: 'hmac_sha256', header: 'x-hub-signature-256' };
    const preset = applyPreset(github, SPECS, initialState(SPECS)).state;
    const custom = customState(SPECS, { ...preset, maxLaunchesPerHour: 3, maxCost: 2 });
    expect(custom).toMatchObject({
      verifier: 'hmac_sha256',
      header: 'x-hub-signature-256',
      filter: 'true',
      dedupe: 'delivery',
      maxLaunchesPerHour: 3,
      maxCost: 2,
    });
    expect(custom.choices).toEqual(initialState(SPECS).choices);
  });
});

describe('createBody', () => {
  it('splits fixed values from derivations and drops the blanks', () => {
    const { state } = applyPreset(QUAY, SPECS, initialState(SPECS));
    state.choices.release = { mode: 'fixed', value: ' 3.2 ', expression: '' };
    state.choices.tags = { mode: 'derived', expression: '   ', value: '' };
    const body = createBody('on-image-push', 'sha256:abc', state);
    expect(body.params).toEqual({ release: '3.2' });
    expect(body.derive).toEqual({ image: 'body.docker_url' });
    expect(body.header).toBeNull();
    expect(body).toMatchObject({
      playbook: 'on-image-push',
      schema_digest: 'sha256:abc',
      verifier: 'path_token',
      max_launches_per_hour: 10,
      retention_days: 14,
      dispatch_target: null,
    });
  });

  it('sends the header only for a signed verifier', () => {
    const state: WebhookFormState = { ...initialState(SPECS), verifier: 'hmac_sha256', header: ' X-Hub-Signature-256 ' };
    expect(createBody('p', 'd', state).header).toBe('X-Hub-Signature-256');
  });
});

describe('previewBody', () => {
  it('parses the sample and the header lines', () => {
    const { state } = applyPreset(QUAY, SPECS, initialState(SPECS));
    const out = previewBody(state, '{"updated_tags":["latest"]}', 'X-Quay-Event: push\n\n');
    expect(out).toEqual({
      body: {
        filter: QUAY.filter,
        dedupe: 'delivery',
        derive: { image: 'body.docker_url', tags: 'body.updated_tags' },
        sample: { updated_tags: ['latest'] },
        headers: { 'x-quay-event': 'push' },
      },
    });
  });

  it('refuses a sample that is not JSON and a header line without a colon', () => {
    const state = initialState(SPECS);
    expect(previewBody(state, '{', '')).toHaveProperty('error');
    expect(previewBody(state, '{}', 'nonsense')).toEqual({ error: 'Headers: line 1 is not "Name: value"' });
  });
});

describe('parseHeaders', () => {
  it('lowercases names and keeps colons in values', () => {
    expect(parseHeaders('Content-Type: application/json\nX-Url: http://a:1')).toEqual({
      headers: { 'content-type': 'application/json', 'x-url': 'http://a:1' },
    });
  });
});

describe('stateFromWebhook', () => {
  it('derives what the webhook derives, fixes what it fixes, and defaults the rest', () => {
    const stored: Partial<WebhookDto> = {
      verifier: 'hmac_sha256',
      header: 'x-hub-signature-256',
      filter: 'true',
      dedupe: 'body.after',
      derive: { image: 'body.docker_url' },
      params: { release: '3.2' },
      max_launches_per_hour: 5,
      retention_days: 30,
      max_cost: 2,
      max_time: '10m',
      dispatch_target: 'hub',
    };
    const state = stateFromWebhook({ ...stateless(), ...stored }, SPECS);
    expect(state.choices.image).toEqual({ mode: 'derived', value: '', expression: 'body.docker_url' });
    expect(state.choices.release).toEqual({ mode: 'fixed', value: '3.2', expression: '' });
    expect(state.choices.tags).toEqual({ mode: 'fixed', value: '["latest"]', expression: '' });
    expect(state).toMatchObject({
      verifier: 'hmac_sha256',
      header: 'x-hub-signature-256',
      maxLaunchesPerHour: 5,
      retentionDays: 30,
      dispatchTarget: 'hub',
    });
  });
});

describe('headerLines', () => {
  it('writes recorded headers as the preview spells them', () => {
    expect(headerLines({ 'x-github-event': 'push', 'content-type': 'application/json' })).toBe(
      'x-github-event: push\ncontent-type: application/json'
    );
    expect(headerLines(null)).toBe('');
  });
});

function stateless(): WebhookDto {
  return {
    id: 'w1',
    playbook: 'p',
    adopted_repo: null,
    adopted_path: null,
    adopted_rev: null,
    params: {},
    derive: {},
    filter: 'true',
    dedupe: 'delivery',
    schema_digest: 'd',
    max_cost: 5,
    max_time: '30m',
    verifier: 'path_token',
    header: null,
    max_launches_per_hour: 10,
    retention_days: 14,
    delivery_path: '/hooks/w1',
    delivery_url: null,
    last_delivery_at: null,
    enabled: true,
    consecutive_failures: 0,
    created_by: null,
    owner_principal: null,
    owner_groups_at: null,
    owner_signin_required: false,
    owner_refresh_error: null,
    owner_refresh_at: null,
    dispatch_target: null,
    agent_provider: null,
    agent_model: null,
    created_at: '2026-09-29T00:00:00Z',
    updated_at: '2026-09-29T00:00:00Z',
  };
}
