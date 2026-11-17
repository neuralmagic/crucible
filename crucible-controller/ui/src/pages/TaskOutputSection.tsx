import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { LogView } from '../logs/LogView';
import { Mono, Section, SectionBody, SectionHeader, Spinner } from '../ui';

/// The run's task output — every line a task printed, folded out of the session log under its
/// task's prefix and refreshed while the run is still working. This is the run's own console: the
/// engine log beside it carries the supervisor's coarse lines, while this one carries what the
/// tasks themselves said, live for a local run.
export function TaskOutputSection({ runId, live }: { runId: string; live: boolean }) {
  const log = $api.useQuery(
    'get',
    '/api/runs/{run_id}/tasks/log',
    { params: { path: { run_id: runId } } },
    { retry: false, refetchInterval: live ? 5_000 : false },
  );
  if (log.isPending) {
    return (
      <Section>
        <SectionHeader title="Task output" />
        <SectionBody>
          <Spinner label="Loading the task output" />
        </SectionBody>
      </Section>
    );
  }
  if (log.isError) {
    return (
      <Section>
        <SectionHeader title="Task output" />
        <SectionBody>
          <Mono tone="ink-3">The task output could not be loaded: {formatError(log.error)}</Mono>
        </SectionBody>
      </Section>
    );
  }
  const text = log.data.text ?? '';
  const body = text !== '' ? (log.data.truncated ? `… earlier output dropped\n${text}` : text) : (log.data.location ?? 'no task output recorded for this run');
  return (
    <Section>
      <SectionHeader
        title="Task output"
        note={live ? 'live' : log.data.truncated ? 'earlier output dropped' : undefined}
      />
      <SectionBody className="flex h-[24rem] flex-col overflow-hidden">
        {text === '' ? (
          <Mono tone="ink-3">{body}</Mono>
        ) : (
          <LogView text={body} testId="run-task-log" follow />
        )}
      </SectionBody>
    </Section>
  );
}
