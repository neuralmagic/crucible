import type { components } from '../api/schema.d';
import { lastRankEvent } from './rankProvenance';

type EventDto = components['schemas']['EventDto'];
type InputKind = components['schemas']['InputKindDto'];

/** The server-computed "already-implemented, upstream-closable" signal
 * (`IssueDto.stale_closable`, derived from `ParkReason::is_stale_closable` in
 * `crucible-controller/src/model.rs`). The UI no longer needs to know the park-reason wording at
 * all — the reason text can change freely without touching this. */
export function isStaleClosable(issue: { stale_closable?: boolean }): boolean {
  return issue.stale_closable === true;
}

/** The issues list's "closable upstream" toggle: a client-side pass on top of whatever the wire
 * query already returned, so it composes with every other filter instead of replacing them. */
export function filterStaleClosable<T extends { stale_closable?: boolean }>(
  issues: T[],
  enabled: boolean,
): T[] {
  return enabled ? issues.filter((issue) => isStaleClosable(issue)) : issues;
}

/** What the issue-detail call-out needs: whether to show it at all, and the evidence event (the
 * same standing-verdict "new -> new" event `rankingRationale` reads) to source the rationale and
 * a recorded-at stamp from — `null` when the park predates the event log carrying a reason. */
export function staleCloseEvidence(
  kind: InputKind,
  status: string,
  staleClosable: boolean | undefined,
  events: EventDto[],
): { evidenceEvent: EventDto | null } | null {
  if (status !== 'parked' || staleClosable !== true) return null;
  return { evidenceEvent: lastRankEvent(kind, events) };
}
