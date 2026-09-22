import { describe, expect, it } from 'vitest';
import {
  allowedHarnesses,
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
  enabled: true,
  created_by: 'alice',
  created_at: '2026-09-02T00:00:00Z',
  updated_at: '2026-09-02T00:00:00Z',
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
  });

  it('says where a custom provider is reached', () => {
    expect(reachLabel(ONPREM)).toBe('responses at http://vllm.internal:8000/v1');
    expect(reachLabel({ ...ONPREM, kind: 'openai' })).toBe('openai');
  });
});
