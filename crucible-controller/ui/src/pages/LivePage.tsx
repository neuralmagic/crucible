import { useCallback } from 'react';
import { Link } from 'react-router-dom';
import { $api } from '../api/client';
import { LiveSession } from '../live/LiveSession';
import { Empty, Identifier, PageHeader } from '../ui';
import { detailPath } from './launchView';

/// Mission control: every running run streaming at once — one compact live tile per run (status
/// header, live score sparkline, auto-following feed), each backed by its own SSE relay connection.
/// The list itself refetches on an interval and whenever any tile's stream ends, so finished runs
/// fall off the wall on their own.
export function LivePage() {
  const runs = $api.useQuery(
    'get',
    '/api/runs',
    { params: { query: { status: 'running', limit: 24 } } },
    { refetchInterval: 30_000 },
  );

  const onTileEnded = useCallback(() => {
    void runs.refetch();
  }, [runs]);

  const rows = runs.data ?? [];

  return (
    <>
      <PageHeader
        eyebrow="Execution"
        title="Live"
        description="Every running run, streaming. Open a tile's run page for the full feed with filters."
      />
      {rows.length === 0 ? (
        <Empty
          title="NOTHING RUNNING"
          description={
            <>
              Runs appear here the moment the controller launches a loop pod. Check{' '}
              <Link to="/runs" className="underline underline-offset-2">
                Runs
              </Link>{' '}
              for history.
            </>
          }
        />
      ) : (
        <div className="grid grid-cols-[repeat(auto-fill,minmax(30rem,1fr))] gap-4 px-4.5 py-4">
          {rows.map((run) => (
            <div key={run.run_id} className="min-w-0">
              <div className="flex flex-wrap items-center gap-2.5 pb-1.5">
                <Identifier to={`/runs/${encodeURIComponent(run.run_id)}`}>{run.run_id}</Identifier>
                {run.issue_key && (
                  <Identifier variant="inline" to={detailPath(run.issue_key)}>
                    {run.issue_key}
                  </Identifier>
                )}
              </div>
              <LiveSession runId={run.run_id} compact onEnded={onTileEnded} />
            </div>
          ))}
        </div>
      )}
    </>
  );
}
