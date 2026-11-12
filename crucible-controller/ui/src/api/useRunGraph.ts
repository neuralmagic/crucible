import { useQuery } from '@tanstack/react-query';
import { $api, apiClient } from './client';
import { formatError } from './errors';

/// A non-2xx the graph fetch failed on, carrying the status the parsed body drops — a retry
/// decision needs to know whether the state is stable, and only the status knows.
class HttpError extends Error {
  constructor(
    readonly status: number,
    body: unknown,
  ) {
    super(formatError(body));
  }
}

/// The run's admitted work graph — the one query every graph-drawing panel shares. The keys copy
/// openapi-react-query's `[method, path, init]` shape so a `$api` observer of the same endpoint
/// shares the cache entry. The run detail (shared with the page's own query, so no extra request)
/// gates the fetch: a run that never admitted a plan never requests the graph — the endpoint's
/// 404 is the same answer, but a console-noisy one. The gate errs toward probing: a detail that
/// errors or predates the field fetches as before, a fetched 404 resolves to `data: undefined`
/// rather than an error, and a transient 5xx keeps the two retries the panels always had.
export function useRunGraph(runId: string) {
  const detail = $api.useQuery('get', '/api/runs/{run_id}', {
    params: { path: { run_id: runId } },
  });
  const query = useQuery({
    queryKey: ['get', '/api/runs/{run_id}/graph', { params: { path: { run_id: runId } } }],
    queryFn: async ({ signal }) => {
      const { data, error, response } = await apiClient.GET('/api/runs/{run_id}/graph', {
        params: { path: { run_id: runId } },
        signal,
      });
      if (response.status === 404) return null;
      if (error) throw new HttpError(response.status, error);
      return data ?? null;
    },
    enabled: detail.isPending ? false : detail.data?.has_plan !== false,
    // A running run's graph keeps moving: the executor folds the session-so-far into the index,
    // so rows land while the engine lives rather than only at completion.
    refetchInterval: () => (detail.data?.run.status === 'running' ? 5000 : false),
    retry: (failureCount, error) =>
      !(error instanceof HttpError && error.status >= 400 && error.status < 500) &&
      failureCount < 2,
    // The plan-less state is settled: focus, reconnect, and a remounting panel must not re-probe it.
    refetchOnWindowFocus: (query) => query.state.data !== null,
    refetchOnReconnect: (query) => query.state.data !== null,
    refetchOnMount: (query) => query.state.data !== null,
  });
  return { ...query, data: query.data ?? undefined };
}
