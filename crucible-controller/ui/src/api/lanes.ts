import { $api } from './client';

/// Whether the caller is entitled to the autoresearch lane; `undefined` until whoami answers.
export function useAutoresearch(): boolean | undefined {
  const whoami = $api.useQuery('get', '/api/whoami');
  return whoami.data?.entitlements.includes('autoresearch');
}

/// Whether this deployment gives runs agent tools; `undefined` until whoami answers.
export function useAgentTools(): boolean | undefined {
  const whoami = $api.useQuery('get', '/api/whoami');
  return whoami.data?.features.agent_tools;
}

/// Whether people link cluster identities or sign in to MCP clients here; `undefined` until whoami
/// answers.
export function useIdentities(): boolean | undefined {
  const whoami = $api.useQuery('get', '/api/whoami');
  return whoami.data?.features.identities;
}
