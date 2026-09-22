import { useMemo } from 'react';
import type { components } from '../api/schema';
import { $api } from '../api/client';
import { Section, SectionBody, SectionHeader } from '../ui';
import { runGraphView, targetLabel } from './taskGraph';
import type { OutputNode } from './taskGraph';
import { WorkflowGraph } from './WorkflowGraph';

export type RunGraph = components['schemas']['RunGraphDto'];

/// The run's admitted work graph on the shared flow surface. The endpoint 404s for a run that never
/// admitted a plan (every run from before the work-graph executor), which leaves `data` undefined —
/// so the whole panel simply isn't there rather than showing an error a reader can do nothing about.
export function RunTaskGraph({ runId }: { runId: string }) {
  const query = $api.useQuery(
    'get',
    '/api/runs/{run_id}/graph',
    { params: { path: { run_id: runId } } },
    { retry: 2 },
  );
  const graph: RunGraph | undefined = query.data;
  const view = useMemo(
    () =>
      graph === undefined ? null : runGraphView(graph.tasks, graph.results, graph.outputs ?? null),
    [graph],
  );

  if (!graph || view === null || graph.tasks.length === 0) return null;
  return (
    <Section>
      <SectionHeader title="Task graph" note={`plan v${graph.plan_version}`} />
      <SectionBody>
        <WorkflowGraph
          graph={view.graph}
          runtime={view.runtime}
          runId={runId}
          outputs={view.outputs}
        />
        <EngineDefaults bounds={view.engineDefaults} />
      </SectionBody>
    </Section>
  );
}

/// The bounds the engine filled in for kinds the pack never declared, folded away: a reader
/// checking what a pack may write sees the declared outputs in the graph and can open this to see
/// the ceiling on everything else.
function EngineDefaults({ bounds }: { bounds: OutputNode[] }) {
  if (bounds.length === 0) return null;
  return (
    <details className="mt-2 text-micro text-ink-2">
      <summary className="cursor-pointer select-none">
        {bounds.length} engine-default {bounds.length === 1 ? 'bound' : 'bounds'} (not declared by
        the pack)
      </summary>
      <ul className="mt-1 ml-4 list-disc font-mono">
        {bounds.map((b) => (
          <li key={b.kind}>
            {b.kind} {'\u00d7'}
            {b.count}
            {targetLabel(b.target) !== null && ` -> ${targetLabel(b.target)}`}
          </li>
        ))}
      </ul>
    </details>
  );
}
