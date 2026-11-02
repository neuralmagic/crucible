import { describe, expect, it } from 'vitest';
import {
  allowedHarnesses,
  defaultBody,
  defaultErrors,
  defaultFormOf,
  emptyDefaultForm,
  targetLabel,
  defaultHarness,
  emptyProviderForm,
  formOf,
  harnessFor,
  harnessOptions,
  keyVariable,
  modelList,
  providerBody,
  providerErrors,
  reachLabel,
  registerProviderBody,
  type ProviderDetailDto,
} from './providersView';

const ONPREM: ProviderDetailDto = {
  id: 'onprem',
  display_name: 'On-prem vLLM',
  kind: 'custom',
  models: ['gpt-oss-120b'],
  default_model: 'gpt-oss-120b',
  secret_name: 'vllm-key',
  secret_owner: 'user:alice',
  owner: 'user:alice',
  endpoint: 'http://vllm.internal:8000/v1',
  protocol: 'responses',
  harness: 'codex',
  harness_override: null,
  roles: ['agent'],
  enabled: true,
  created_by: 'alice',
  created_at: '2026-09-02T00:00:00Z',
  updated_at: '2026-09-02T00:00:00Z',
  actions: ['read', 'update', 'delete'],
};

describe('providerBody', () => {
  it('sends the endpoint and protocol only for a custom provider', () => {
    const custom = providerBody({ ...formOf(ONPREM) });
    expect(custom.endpoint).toBe('http://vllm.internal:8000/v1');
    expect(custom.protocol).toBe('responses');
    expect(custom.secret_name).toBe('vllm-key');
    expect(custom.secret_owner).toBe('user:alice');

    const openai = providerBody({ ...emptyProviderForm(), displayName: 'OpenAI', endpoint: 'https://x' });
    expect('endpoint' in openai).toBe(false);
    expect('protocol' in openai).toBe(false);
    expect('default_model' in openai).toBe(false);
    expect('secret_name' in openai).toBe(false);
  });

  it('splits the model list on newlines and commas', () => {
    expect(modelList('a\nb, c\n\n')).toEqual(['a', 'b', 'c']);
    const body = registerProviderBody(
      { ...emptyProviderForm(), id: ' plat ', displayName: 'P', models: 'gpt-5.6-luna\ngpt-5.6-sol' },
      'user:alice',
    );
    expect(body.id).toBe('plat');
    expect(body.owner).toBe('user:alice');
    expect(body.models).toEqual(['gpt-5.6-luna', 'gpt-5.6-sol']);
  });
});

describe('harness', () => {
  it('offers the harnesses the server accepts for the kind or protocol, default first', () => {
    expect(allowedHarnesses('openai', 'chat_completions')).toEqual(['codex', 'opencode', 'pi']);
    expect(allowedHarnesses('vertex', 'messages')).toEqual(['claude', 'hermes']);
    expect(allowedHarnesses('custom', 'chat_completions')).toEqual(['opencode', 'pi']);
    expect(allowedHarnesses('custom', 'responses')).toEqual(['codex', 'pi']);
    expect(allowedHarnesses('custom', 'messages')).toEqual(['claude', 'opencode', 'pi']);
    expect(defaultHarness('custom', 'chat_completions')).toBe('opencode');
    expect(defaultHarness('anthropic', 'chat_completions')).toBe('claude');
    expect(harnessOptions('custom', 'chat_completions').map((o) => o.value)).toEqual(['', 'opencode', 'pi']);
    expect(harnessOptions('custom', 'chat_completions')[0].label).toBe('default (OpenCode)');
    expect(harnessOptions('custom', 'decisions')).toEqual([]);
    expect(defaultHarness('custom', 'system_one')).toBeUndefined();
  });

  it('keeps a chosen harness across a kind or protocol change only while it still applies', () => {
    expect(harnessFor('custom', 'chat_completions', 'pi')).toBe('pi');
    expect(harnessFor('custom', 'chat_completions', 'codex')).toBe('');
    expect(harnessFor('vertex', 'messages', 'pi')).toBe('');
    expect(harnessFor('openai', 'messages', '')).toBe('');
  });

  it('rides the body only when chosen, and loads back from the override', () => {
    const chosen = providerBody({ ...formOf(ONPREM), harness: 'pi' });
    expect(chosen.harness).toBe('pi');
    const unchosen = providerBody(formOf(ONPREM));
    expect('harness' in unchosen).toBe(false);
    expect(formOf({ ...ONPREM, harness: 'pi', harness_override: 'pi' }).harness).toBe('pi');
    expect(formOf(ONPREM).harness).toBe('');
  });

  it('refuses a harness the service cannot run', () => {
    const errors = providerErrors({ ...formOf(ONPREM), harness: 'claude' }, true);
    expect(errors.get('harness')).toContain('codex, pi');
    expect(providerErrors({ ...formOf(ONPREM), harness: 'pi' }, true).has('harness')).toBe(false);
  });
});

describe('providerErrors', () => {
  it('holds a custom provider to an endpoint and a default model', () => {
    const errors = providerErrors({ ...emptyProviderForm(), kind: 'custom', displayName: 'x', id: 'x' }, false);
    expect(errors.get('endpoint')).toContain('base URL');
    expect(errors.get('defaultModel')).toContain('default model');
    const bad = providerErrors({ ...formOf(ONPREM), endpoint: 'vllm.internal/v1' }, true);
    expect(bad.get('endpoint')).toContain('http(s)');
    expect(providerErrors(formOf(ONPREM), true).size).toBe(0);
  });

  it('checks the id only on registration and refuses a vertex secret', () => {
    expect(providerErrors({ ...emptyProviderForm(), id: 'Bad Id', displayName: 'x' }, false).get('id')).toContain('lowercase');
    expect(providerErrors({ ...emptyProviderForm(), id: 'Bad Id', displayName: 'x' }, true).has('id')).toBe(false);
    const vertex = providerErrors({ ...emptyProviderForm(), id: 'v', displayName: 'V', kind: 'vertex', secretName: 'k' }, false);
    expect(vertex.get('secretName')).toContain('ADC');
  });
});

describe('labels', () => {
  it('names the variable a provider reads its key from', () => {
    expect(keyVariable('openai', 'chat_completions')).toBe('OPENAI_API_KEY');
    expect(keyVariable('custom', 'messages')).toBe('ANTHROPIC_API_KEY');
    expect(keyVariable('custom', 'responses')).toBe('OPENAI_API_KEY');
    expect(keyVariable('vertex', 'messages')).toBeNull();
    expect(keyVariable('custom', 'decisions')).toBe('OPENAI_API_KEY');
    expect(keyVariable('custom', 'system_one')).toBeNull();
  });

  it('says where a custom provider is reached', () => {
    expect(reachLabel(ONPREM)).toBe('responses at http://vllm.internal:8000/v1');
    expect(reachLabel({ ...ONPREM, kind: 'openai' })).toBe('openai');
  });
});

describe('dispatch defaults', () => {
  const primary = { ...emptyDefaultForm(), provider: 'pricetag-glm' };

  it('sends a fallback only when one is picked', () => {
    expect(defaultBody(primary)).toEqual({
      scope_kind: 'platform',
      workload_class: 'autoresearch',
      role: 'agent',
      provider: 'pricetag-glm',
    });
    expect(
      defaultBody({ ...primary, fallbackProvider: 'vertex', fallbackModel: ' claude-sonnet-5 ' }),
    ).toEqual({
      scope_kind: 'platform',
      workload_class: 'autoresearch',
      role: 'agent',
      provider: 'pricetag-glm',
      fallback_provider: 'vertex',
      fallback_model: 'claude-sonnet-5',
    });
  });

  it('drops a fallback model left behind when the fallback is cleared', () => {
    expect(defaultBody({ ...primary, fallbackModel: 'claude-sonnet-5' })).not.toHaveProperty('fallback_model');
    expect(defaultErrors({ ...primary, fallbackModel: 'claude-sonnet-5' }).get('fallbackModel')).toBe(
      'a fallback model needs a fallback provider',
    );
  });

  it('sends scope_ref only for a domain default', () => {
    const domain = { ...primary, scopeKind: 'domain' as const, scopeRef: ' org/vllm ' };
    expect(defaultBody(domain).scope_ref).toBe('org/vllm');
    expect(defaultBody({ ...primary, scopeRef: 'org/vllm' })).not.toHaveProperty('scope_ref');
  });

  it('refuses what the server refuses', () => {
    expect(defaultErrors(emptyDefaultForm()).get('provider')).toBe('pick a provider');
    expect(defaultErrors({ ...primary, role: 'decision' }).get('role')).toBe(
      'an autoresearch loop runs no route task',
    );
    expect(defaultErrors({ ...primary, role: 'decision', workloadClass: 'playbook' }).has('role')).toBe(false);
    expect(defaultErrors({ ...primary, scopeKind: 'domain', scopeRef: 'vllm' }).get('scopeRef')).toBe(
      'a domain is spelled owner/repo',
    );
    expect(defaultErrors({ ...primary, fallbackProvider: 'pricetag-glm' }).get('fallbackProvider')).toBe(
      'the fallback is the same provider and model as the primary',
    );
    expect(
      defaultErrors({ ...primary, fallbackProvider: 'pricetag-glm', fallbackModel: 'glm-small' }).size,
    ).toBe(0);
    expect(defaultErrors({ ...primary, fallbackProvider: 'vertex', fallbackModel: 'bad model' }).get('fallbackModel')).toBe(
      '"bad model" is not a model name',
    );
  });

  it('loads a stored default back into the form', () => {
    const form = defaultFormOf({
      scope_kind: 'domain',
      scope_ref: 'org/vllm',
      workload_class: 'playbook',
      role: 'decision',
      provider: 'pricetag-glm',
      model: null,
      fallback_provider: 'vertex',
      fallback_model: 'claude-sonnet-5',
    });
    expect(defaultBody(form)).toEqual({
      scope_kind: 'domain',
      scope_ref: 'org/vllm',
      workload_class: 'playbook',
      role: 'decision',
      provider: 'pricetag-glm',
      fallback_provider: 'vertex',
      fallback_model: 'claude-sonnet-5',
    });
  });

  it('labels a target with its model only when one is pinned', () => {
    expect(targetLabel('vertex', null)).toBe('vertex');
    expect(targetLabel('vertex', 'claude-sonnet-5')).toBe('vertex · claude-sonnet-5');
  });
});
