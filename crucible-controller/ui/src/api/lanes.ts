import { $api } from './client';

/// Whether the caller is entitled to the autoresearch lane; `undefined` until whoami answers.
export function useAutoresearch(): boolean | undefined {
  const whoami = $api.useQuery('get', '/api/whoami');
  return whoami.data?.entitlements.includes('autoresearch');
}
