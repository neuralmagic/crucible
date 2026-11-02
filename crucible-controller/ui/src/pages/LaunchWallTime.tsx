import { $api } from '../api/client';
import { launchWallSecs } from './launchView';
import { formatSecs } from './runGrid';

/** How long a launch ran, from its event trail; nothing while it runs. */
export function LaunchWallTime({ launchKey }: { launchKey: string }) {
  const issue = $api.useQuery('get', '/api/issues/{key}', { params: { path: { key: launchKey } } });
  const secs = launchWallSecs(issue.data?.events ?? []);
  if (secs === null) return null;
  return <span data-testid="wall-time">ran {formatSecs(secs)}</span>;
}
