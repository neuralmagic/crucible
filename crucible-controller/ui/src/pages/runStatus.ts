export const RUN_STATUSES = ['finished', 'running', 'incomplete', 'escalated', 'failed'] as const;

export type RunStatus = (typeof RUN_STATUSES)[number];

export type RunStatusColor = 'blue' | 'green' | 'orange' | 'grey' | 'red';

const RUN_STATUS_COLORS: Record<RunStatus, RunStatusColor> = {
  finished: 'green',
  running: 'blue',
  incomplete: 'grey',
  escalated: 'orange',
  failed: 'red',
};

const STATUS_SET: ReadonlySet<string> = new Set(RUN_STATUSES);

function isRunStatus(status: string): status is RunStatus {
  return STATUS_SET.has(status);
}

/** Label color for a run status off the wire; grey for anything outside the vocabulary. */
export function runStatusColor(status: string): RunStatusColor {
  return isRunStatus(status) ? RUN_STATUS_COLORS[status] : 'grey';
}
