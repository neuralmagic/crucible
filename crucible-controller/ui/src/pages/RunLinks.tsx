import { useRunGraph } from '../api/useRunGraph';
import { ExternalLinks } from '../ui';
import type { RunGraph } from './RunTaskGraph';

/// Every external result the run's tasks reported. Shares the graph endpoint's cache key with
/// `RunTaskGrid`, so drawing both costs one fetch.
export function RunLinks({ runId }: { runId: string }) {
  const query = useRunGraph(runId);
  const graph: RunGraph | undefined = query.data;
  const links = (graph?.results ?? []).flatMap((result) => result.links);
  if (links.length === 0) return null;
  return (
    <div data-testid="run-links">
      <ExternalLinks links={links} />
    </div>
  );
}
