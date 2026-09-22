import { useQueryClient } from '@tanstack/react-query';
import { useEffect } from 'react';
import { openEventSource } from './session';

// openapi-react-query keys are [method, path, init] — the 'get' prefix is load-bearing for the
// invalidation match.
const LIVE_PATHS = [
  '/api/overview',
  '/api/issues',
  '/api/events',
  '/api/approvals',
  '/api/repos',
  '/api/runs',
  '/api/ledger/summary',
] as const;

/** How long a burst of transitions accumulates before one refetch pass. A reconcile sweep emits
 * transitions in bursts of dozens; invalidating per frame refetched the unpaginated `/api/issues`
 * list (and every mounted issue detail) dozens of times over for one settled state. */
const COALESCE_MS = 2_000;

export function useLiveEvents() {
  const queryClient = useQueryClient();

  useEffect(() => {
    const eventSource = openEventSource('/api/events/stream');
    let timer: ReturnType<typeof setTimeout> | null = null;

    const invalidate = () => {
      timer = null;
      for (const path of LIVE_PATHS) {
        void queryClient.invalidateQueries({ queryKey: ['get', path] });
      }
      // Individual issue detail pages (/api/issues/{key}) use a different query key; prefix-match
      // invalidates them all so the detail + progress views stay live. Only *mounted* queries
      // refetch, which is why an expandable row must not mount its detail until it opens.
      void queryClient.invalidateQueries({
        predicate: (query) => {
          const key = query.queryKey;
          return (
            Array.isArray(key) &&
            key[0] === 'get' &&
            typeof key[1] === 'string' &&
            key[1].startsWith('/api/issues/')
          );
        },
      });
    };

    // Leading-edge timer: the first event of a quiet period arms it and everything inside the
    // window rides along, so a burst costs exactly one pass. Transient stream errors are left to
    // EventSource's native reconnect.
    eventSource.onmessage = () => {
      if (timer === null) timer = setTimeout(invalidate, COALESCE_MS);
    };

    return () => {
      if (timer !== null) clearTimeout(timer);
      eventSource.close();
    };
  }, [queryClient]);
}
