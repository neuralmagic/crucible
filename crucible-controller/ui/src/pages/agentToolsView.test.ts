import { describe, expect, it } from 'vitest';
import {
  AGENT_TOOLS_NOTICE,
  agentToolRows,
  agentToolsNotice,
  type AgentTool,
  type AgentToolDefaults,
} from './agentToolsView';

function tool(over: Partial<AgentTool> = {}): AgentTool {
  return {
    server: 'kueue',
    cluster: 'ocp',
    url: 'http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp',
    kube_user: 'casey',
    scopes: ['kueue.read', 'kueue.submit'],
    limits: { pull_secrets: 'quay-pull', namespace: 'crucible-kueue-e2e' },
    ...over,
  };
}

function defaults(over: Partial<AgentToolDefaults> = {}): AgentToolDefaults {
  return {
    revision: 'abc123',
    changelog: 'Runs now include kueue on ocp tools.',
    tools: [tool({ kube_user: null })],
    ...over,
  };
}

describe('agentToolRows', () => {
  it('names each server, who it acts as, its scopes and its limits in name order', () => {
    expect(agentToolRows([tool()])).toEqual([
      {
        key: 'kueue/ocp',
        title: 'kueue on ocp',
        identity: 'as casey',
        scopes: 'kueue.read kueue.submit',
        limits: 'namespace crucible-kueue-e2e · pull_secrets quay-pull',
        url: 'http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp',
      },
    ]);
  });

  it('says nothing about identity for a server that acts as no Kubernetes user', () => {
    const [row] = agentToolRows([
      tool({ server: 'buildit', kube_user: null, limits: {}, scopes: ['buildit.read'] }),
    ]);
    expect(row.identity).toBe('');
    expect(row.limits).toBe('');
  });

  it('shows nothing for a run granted nothing', () => {
    expect(agentToolRows([])).toEqual([]);
  });
});

describe('agentToolsNotice', () => {
  it('announces a revision the person has not seen', () => {
    expect(agentToolsNotice(defaults(), {})).toBe('Runs now include kueue on ocp tools.');
    expect(agentToolsNotice(defaults(), { [AGENT_TOOLS_NOTICE]: 'older' })).toBe(
      'Runs now include kueue on ocp tools.',
    );
  });

  it('stays quiet once the revision is the one they dismissed', () => {
    expect(agentToolsNotice(defaults(), { [AGENT_TOOLS_NOTICE]: 'abc123' })).toBeNull();
  });

  it('stays quiet while either side is loading, and when nothing is granted', () => {
    expect(agentToolsNotice(undefined, {})).toBeNull();
    expect(agentToolsNotice(defaults(), undefined)).toBeNull();
    expect(agentToolsNotice(defaults({ tools: [] }), {})).toBeNull();
  });
});
