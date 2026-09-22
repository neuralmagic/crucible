import { Popover } from '@base-ui-components/react/popover';
import { $api } from './api/client';

export function IdentityBadge() {
  const { data, isSuccess } = $api.useQuery('get', '/api/whoami');

  if (!isSuccess || !data.user) return null;

  return (
    <Popover.Root>
      <Popover.Trigger className="flex items-center border-l border-rule px-3 font-mono text-data font-medium text-ink hover:bg-hi">
        {data.user}
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Positioner sideOffset={1} align="end">
          <Popover.Popup className="min-w-52 border border-rule-hard bg-surface">
            <dl className="border-b border-rule px-3 py-2.5 font-mono text-data">
              <div className="flex justify-between gap-6">
                <dt className="text-ink-3">role</dt>
                <dd className="text-ink">{data.role}</dd>
              </div>
              <div className="mt-1 flex justify-between gap-6">
                <dt className="text-ink-3">groups</dt>
                <dd className="text-ink">{data.groups.length}</dd>
              </div>
            </dl>
            <a
              href="/auth/logout"
              className="block px-3 py-2.5 font-mono text-data text-ink-2 hover:bg-hi hover:text-ink"
            >
              Sign out
            </a>
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  );
}
