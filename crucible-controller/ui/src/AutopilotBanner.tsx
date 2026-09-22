import { Status } from './ui';
import { $api } from './api/client';

export function AutopilotBanner() {
  const autopilot = $api.useQuery('get', '/api/autopilot', {}, { refetchInterval: 30_000 });

  if (!autopilot.isSuccess || autopilot.data.enabled) return null;

  const { changed_by: changedBy, reason } = autopilot.data;

  return (
    <div className="flex flex-none items-center gap-3 border-b border-rule-hard bg-surface px-3.5 py-1.5 font-mono text-data">
      <Status status="autopilot disabled" tone="amber" />
      {changedBy ? <span className="text-ink-2">by {changedBy}</span> : null}
      {reason ? <span className="text-ink-3">· {reason}</span> : null}
    </div>
  );
}
