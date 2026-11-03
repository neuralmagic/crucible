import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { $api } from '../api/client';
import { Button, Empty, Mono, QueryState, Section, SectionBody, SectionHeader } from '../ui';
import { remaining } from './decisions';

/// The browser's notification permission, or `unsupported` where there is no API.
function usePermission(): [NotificationPermission | 'unsupported', () => void] {
  const supported = typeof Notification !== 'undefined';
  const [permission, setPermission] = useState<NotificationPermission | 'unsupported'>(
    supported ? Notification.permission : 'unsupported',
  );
  useEffect(() => {
    if (supported) setPermission(Notification.permission);
  }, [supported]);
  const ask = () => {
    if (!supported) return;
    void Notification.requestPermission().then(setPermission);
  };
  return [permission, ask];
}

/// Open decision requests the caller may answer, newest first.
export function DecisionsSection() {
  const decisions = $api.useQuery('get', '/api/decisions');
  const [permission, ask] = usePermission();
  const open = decisions.data?.open ?? [];
  const now = Date.now();
  return (
    <Section>
      <SectionHeader
        title="Decisions"
        note={`${String(open.length)} waiting`}
        actions={
          permission === 'default' ? (
            <Button onClick={ask} data-testid="decisions-notify">
              Notify me
            </Button>
          ) : null
        }
      />
      <QueryState query={decisions} noun="decisions">
        {open.length === 0 ? (
          <Empty title="No decisions waiting" />
        ) : (
          <SectionBody padded={false}>
            <ul className="m-0 list-none p-0" data-testid="decisions-open">
              {open.map((d) => (
                <li key={d.id} className="border-b border-rule last:border-b-0">
                  <Link
                    to={`/decisions/${encodeURIComponent(d.id)}`}
                    className="flex flex-wrap items-baseline gap-3 px-4.5 py-2 hover:bg-hi"
                  >
                    <Mono size="data" weight="semibold">
                      {d.task}
                    </Mono>
                    {d.launch_key ? (
                      <Mono size="data" tone="ink-3">
                        {d.launch_key}
                      </Mono>
                    ) : null}
                    <Mono size="data" tone="ink-3">
                      {d.questions.join(', ')}
                    </Mono>
                    <span className="flex-1" />
                    <Mono size="data" tone="amber">
                      expires {remaining(d.expires_at, now)}
                    </Mono>
                  </Link>
                </li>
              ))}
            </ul>
          </SectionBody>
        )}
      </QueryState>
    </Section>
  );
}
