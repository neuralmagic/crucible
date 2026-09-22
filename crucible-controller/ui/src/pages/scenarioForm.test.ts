import { describe, expect, it } from 'vitest';
import { NO_AGENT_PICK } from './agentPick';
import { scenarioAdoptBody, type ScenarioFormFields } from './scenarioForm';

const FIELDS: ScenarioFormFields = {
  title: '  faster p99  ',
  body: '  cut p99 latency under load  ',
  affectedRepos: ['  owner/repo  ', '   ', 'owner/other'],
  authoritative: false,
  gitRef: '',
  codegenContract: '',
  justification: '  customer ask  ',
  dispatchTarget: '',
  agent: NO_AGENT_PICK,
};

describe('scenarioAdoptBody', () => {
  it('trims every text field and drops blank repo rows', () => {
    const body = scenarioAdoptBody(FIELDS);
    expect(body.title).toBe('faster p99');
    expect(body.body).toBe('cut p99 latency under load');
    expect(body.justification).toBe('customer ask');
    expect(body.affected_repos).toEqual(['owner/repo', 'owner/other']);
  });

  it('omits git_ref entirely when the input is blank', () => {
    expect('git_ref' in scenarioAdoptBody(FIELDS)).toBe(false);
    expect('git_ref' in scenarioAdoptBody({ ...FIELDS, gitRef: '   ' })).toBe(false);
  });

  it('sends the trimmed git_ref when one is given', () => {
    expect(scenarioAdoptBody({ ...FIELDS, gitRef: '  nv_dev  ' }).git_ref).toBe('nv_dev');
  });

  it('carries the authoritative flag through', () => {
    expect(scenarioAdoptBody({ ...FIELDS, authoritative: true }).authoritative).toBe(true);
  });

  it('omits codegen_contract entirely when nothing is selected', () => {
    expect('codegen_contract' in scenarioAdoptBody(FIELDS)).toBe(false);
    expect('codegen_contract' in scenarioAdoptBody({ ...FIELDS, codegenContract: '  ' })).toBe(
      false,
    );
  });

  it('sends the trimmed codegen_contract when one is selected', () => {
    expect(scenarioAdoptBody({ ...FIELDS, codegenContract: ' deepgemm ' }).codegen_contract).toBe(
      'deepgemm',
    );
  });

  it('omits dispatch_target entirely when no cluster is chosen', () => {
    expect('dispatch_target' in scenarioAdoptBody(FIELDS)).toBe(false);
    expect('dispatch_target' in scenarioAdoptBody({ ...FIELDS, dispatchTarget: '  ' })).toBe(false);
  });

  it('sends the trimmed dispatch_target when a cluster is chosen', () => {
    expect(scenarioAdoptBody({ ...FIELDS, dispatchTarget: ' wharf ' }).dispatch_target).toBe('wharf');
  });

  it('omits both agent fields when no provider is picked', () => {
    const body = scenarioAdoptBody(FIELDS);
    expect('provider' in body).toBe(false);
    expect('model' in body).toBe(false);
  });

  it('sends a model only alongside the provider that offers it', () => {
    const picked = scenarioAdoptBody({
      ...FIELDS,
      agent: { provider: ' prod-openai ', model: ' gpt-5.6-luna ' },
    });
    expect(picked.provider).toBe('prod-openai');
    expect(picked.model).toBe('gpt-5.6-luna');

    const modelOnly = scenarioAdoptBody({ ...FIELDS, agent: { provider: '', model: 'gpt-5.6-sol' } });
    expect('provider' in modelOnly).toBe(false);
    expect('model' in modelOnly).toBe(false);

    const blankModel = scenarioAdoptBody({ ...FIELDS, agent: { provider: 'prod-openai', model: '  ' } });
    expect(blankModel.provider).toBe('prod-openai');
    expect('model' in blankModel).toBe(false);
  });

  it('sends both pins independently', () => {
    const body = scenarioAdoptBody({ ...FIELDS, gitRef: 'nv_dev', codegenContract: 'deepgemm' });
    expect(body.git_ref).toBe('nv_dev');
    expect(body.codegen_contract).toBe('deepgemm');
  });
});
