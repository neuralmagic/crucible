import type { components } from '../api/schema.d';

type LongText = components['schemas']['LongText'];

// The work-pod (turn) wire vocabulary — the exact spellings `WorkPodState::parse` and
// `WorkKind::parse_label` accept (crucible-controller/src/workpod.rs). `?state=`/`?kind=`
// filter values and badge colors both key off these; anything else 400s the API.
export const TURN_STATES = [
  'queued',
  'running',
  'succeeded',
  'failed',
  'collected',
  'swept',
] as const;

export type TurnState = (typeof TURN_STATES)[number];

export const TURN_KINDS = ['grounded-rank', 'scope', 'run'] as const;

export type TurnStateColor = 'blue' | 'green' | 'orange' | 'grey' | 'red';

const TURN_STATE_COLORS: Record<TurnState, TurnStateColor> = {
  queued: 'grey',
  running: 'blue',
  succeeded: 'green',
  failed: 'red',
  collected: 'green',
  swept: 'grey',
};

const STATE_SET: ReadonlySet<string> = new Set(TURN_STATES);

function isTurnState(state: string): state is TurnState {
  return STATE_SET.has(state);
}

/** Badge color for a state string off the wire; grey for anything outside the vocabulary. */
export function turnStateColor(state: string): TurnStateColor {
  return isTurnState(state) ? TURN_STATE_COLORS[state] : 'grey';
}

/** How long the turn ran (or has been running): terminal_at - created_at for a finished turn,
 * now - created_at for one still in flight. Null when created_at is missing/unparseable, or when
 * the clock skews negative (a lie beats a "-3s"). */
export function turnDuration(
  createdAt: string | null | undefined,
  terminalAt: string | null | undefined,
  now: number = Date.now(),
): string | null {
  if (!createdAt) return null;
  const start = new Date(createdAt).getTime();
  if (Number.isNaN(start)) return null;
  const end = terminalAt ? new Date(terminalAt).getTime() : now;
  if (Number.isNaN(end)) return null;
  const deltaSec = Math.floor((end - start) / 1000);
  if (deltaSec < 0) return null;
  if (deltaSec < 60) return `${deltaSec}s`;
  const min = Math.floor(deltaSec / 60);
  if (min < 60) return `${min}m ${deltaSec % 60}s`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ${min % 60}m`;
  const day = Math.floor(hr / 24);
  return `${day}d ${hr % 24}h`;
}

/** First line of a failure reason, clipped to `max` chars — the table cell preview; the row
 * expansion carries the full text. Null in, null out. */
export function truncateReason(
  reason: LongText | null | undefined,
  max = 120,
): string | null {
  if (!reason?.text) return null;
  const firstLine = reason.text.split('\n', 1)[0];
  if (firstLine.length <= max && firstLine === reason.text) return reason.text;
  return `${firstLine.slice(0, max).trimEnd()}…`;
}
