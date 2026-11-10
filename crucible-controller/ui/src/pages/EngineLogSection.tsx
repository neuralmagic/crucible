import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { LogView } from '../logs/LogView';
import { Mono, Section, SectionBody, SectionHeader, Spinner } from '../ui';
import { logBody, logPath, logSectionNote } from './taskEvidenceView';

/// The run's engine output, refreshed while the run is still working: the supervised subprocess
/// for a local run, the work pod's output for a pod run. A run whose output this deployment
/// cannot reach says where it lives instead.
export function EngineLogSection({ runId, live }: { runId: string; live: boolean }) {
  const log = $api.useQuery(
    'get',
    '/api/runs/{run_id}/log',
    { params: { path: { run_id: runId } } },
    { retry: false, refetchInterval: live ? 5_000 : false },
  );
  if (log.isPending) {
    return (
      <Section>
        <SectionHeader title="Engine log" />
        <SectionBody>
          <Spinner label="Loading the engine log" />
        </SectionBody>
      </Section>
    );
  }
  if (log.isError) {
    return (
      <Section>
        <SectionHeader title="Engine log" />
        <SectionBody>
          <Mono tone="ink-3">The engine log could not be loaded: {formatError(log.error)}</Mono>
        </SectionBody>
      </Section>
    );
  }
  return (
    <Section>
      <SectionHeader title="Engine log" note={logSectionNote(log.data)} />
      <SectionBody className="flex h-[24rem] flex-col overflow-hidden">
        {logPath(log.data) === null ? (
          <Mono tone="ink-3">{logBody(log.data)}</Mono>
        ) : (
          <LogView text={logBody(log.data)} testId="run-engine-log" follow />
        )}
      </SectionBody>
    </Section>
  );
}
