import { useState } from 'react';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { Button, cn } from '../ui';

/// Stops a running launch: the controller parks it and stops its runs — a pod run's pod is
/// deleted, a locally dispatched engine is signalled. A first press arms the button; the second
/// sends the cancel.
export function CancelLaunchControl({ launchKey }: { launchKey: string }) {
  const cancel = $api.useMutation('post', '/api/playbook-runs/{key}/cancel');
  const [armed, setArmed] = useState(false);
  if (cancel.isSuccess) {
    return <span className="text-sm text-ink-3">Cancelling</span>;
  }
  return (
    <div className="flex flex-col items-end gap-1">
      <Button
        className={cn('uppercase', armed && 'border border-red text-red')}
        disabled={cancel.isPending}
        onClick={() => {
          if (armed) {
            cancel.mutate({ params: { path: { key: launchKey } } });
          } else {
            setArmed(true);
          }
        }}
      >
        {armed ? 'Confirm cancel' : 'Cancel'}
      </Button>
      {cancel.isError && (
        <span className="text-sm text-red">{formatError(cancel.error)}</span>
      )}
    </div>
  );
}
