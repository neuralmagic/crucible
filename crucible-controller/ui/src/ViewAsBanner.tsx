import { $api } from './api/client';
import { useViewAs } from './api/viewAs';
import { Button, Status } from './ui';

export function ViewAsBanner() {
  const whoami = $api.useQuery('get', '/api/whoami');
  const viewAs = useViewAs();
  const view = whoami.data?.impersonation;
  if (!view) return null;

  return (
    <div
      data-testid="view-as-banner"
      className="flex flex-none items-center gap-3 border-b border-rule-hard bg-surface px-3.5 py-1.5 font-mono text-data"
    >
      <Status status={`viewing as ${view.login}`} tone="amber" />
      <span className="text-ink-2">read-only</span>
      <span className="text-ink-3">· groups as of {view.groups_at ?? 'never'}</span>
      <div className="flex-1" />
      <Button disabled={viewAs.pending} onClick={() => { void viewAs.stop(); }}>
        STOP
      </Button>
    </div>
  );
}
