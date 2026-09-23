import { $api } from './client';

/// Whether this controller runs the autoresearch lane; `undefined` until the version answers.
export function useAutoresearch(): boolean | undefined {
  const version = $api.useQuery('get', '/api/version', {}, { staleTime: Infinity });
  return version.data?.autoresearch;
}
