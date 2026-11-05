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
  '/api/decisions',
  '/api/decisions/{id}',
  '/api/repos',
  '/api/runs',
  '/api/playbook-runs',
  '/api/schedules',
  '/api/ledger/summary',
] as const;

/** How long a burst of transitions accumulates before one refetch pass. A reconcile sweep emits
 * transitions in bursts of dozens; invalidating per frame refetched the unpaginated `/api/issues`
 * list (and every mounted issue detail) dozens of times over for one settled state. */
const COALESCE_MS = 2_000;

export function useLiveEvents() {
  const queryClient = useQueryClient();

  useEffect(() => {
    let eventSource: EventSource | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;
    // The stream survives restarts and tab switches, so "not the first open" is what tells an
    // invalidation that the page may have missed events while it was disconnected.
    let connected = false;

    const invalidate = () => {
      timer = null;
      for (const path of LIVE_PATHS) {
        void queryClient.invalidateQueries({ queryKey: ['get', path] });
      }
      // Individual issue detail pages (/api/issues/{key}) and run-scoped queries
      // (/api/runs/{run_id}/files and friends) use different query keys; prefix-match invalidates
      // them so the detail and run views stay live. Only *mounted* queries refetch, which is why an
      // expandable row must not mount its detail until it opens.
      void queryClient.invalidateQueries({
        predicate: (query) => {
          const key = query.queryKey;
          return (
            Array.isArray(key) &&
            key[0] === 'get' &&
            typeof key[1] === 'string' &&
            (key[1].startsWith('/api/issues/') || key[1].startsWith('/api/runs/'))
          );
        },
      });
    };

    // A browser gives one origin six connections and an open stream holds one of them for as long
    // as it is open, so a handful of background tabs starve every new request on that origin.
    // Hidden tabs therefore give their connection back; becoming visible reopens the stream and
    // invalidates, which covers whatever they missed while closed.
    const connect = () => {
      if (eventSource !== null) return;
      const source = openEventSource('/api/events/stream');
      // Leading-edge timer: the first event of a quiet period arms it and everything inside the
      // window rides along, so a burst costs exactly one pass. Transient stream errors are left to
      // EventSource's native reconnect.
      source.onmessage = () => {
        if (timer === null) timer = setTimeout(invalidate, COALESCE_MS);
      };
      // A controller restart drops every in-flight query on an open page; EventSource reconnects on
      // its own but no event need follow, so the page would sit on its loading state until a
      // reload. The first open is the page's own initial fetch and has nothing stale to replace.
      source.onopen = () => {
        if (connected) invalidate();
        connected = true;
      };
      eventSource = source;
    };

    const disconnect = () => {
      if (eventSource === null) return;
      eventSource.close();
      eventSource = null;
    };

    const onVisibility = () => {
      if (document.visibilityState === 'visible') connect();
      else disconnect();
    };

    onVisibility();
    document.addEventListener('visibilitychange', onVisibility);

    return () => {
      document.removeEventListener('visibilitychange', onVisibility);
      if (timer !== null) clearTimeout(timer);
      disconnect();
    };
  }, [queryClient]);
}
