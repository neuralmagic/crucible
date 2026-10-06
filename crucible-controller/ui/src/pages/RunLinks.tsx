import { $api } from '../api/client';
import { ExternalLinks, Section, SectionBody, SectionHeader } from '../ui';
import type { RunGraph } from './RunTaskGraph';

/// Every external result the run's tasks reported. Shares the graph endpoint's cache key with
/// `RunTaskGrid`, so drawing both costs one fetch.
export function RunLinks({ runId }: { runId: string }) {
  const query = $api.useQuery(
    'get',
    '/api/runs/{run_id}/graph',
    { params: { path: { run_id: runId } } },
    { retry: 2 },
  );
  const graph: RunGraph | undefined = query.data;
  const links = (graph?.results ?? []).flatMap((result) => result.links);
  if (links.length === 0) return null;
  return (
    <Section>
      <SectionHeader title="Links" />
      <SectionBody>
        <ExternalLinks links={links} />
      </SectionBody>
    </Section>
  );
}
