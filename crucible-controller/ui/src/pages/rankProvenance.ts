import type { components } from '../api/schema.d';

type EventDto = components['schemas']['EventDto'];
type JourneyStep = components['schemas']['JourneyStep'];
type InputKind = components['schemas']['InputKindDto'];

// Both the text-tier rank and a grounded verdict log a `new -> new` event carrying the rationale
// (see crucible-controller/src/reconcile.rs); the newest such event is the verdict currently
// standing, so it doubles as the "last ranked" stamp and the rationale source. Exported (not just
// `rankingRationale`/`lastRankedAt`) so a stale-park call-out can show the full event, timestamp
// included, without re-implementing the search.
export function lastRankEvent(kind: InputKind, events: EventDto[]): EventDto | null {
  if (!ranks(kind)) return null;
  for (let i = events.length - 1; i >= 0; i--) {
    const event = events[i];
    if (event.from === 'new' && event.to === 'new' && event.reason) {
      return event;
    }
  }
  return null;
}

/** True for the kinds the ranker sees. A playbook launch is authorized, not ranked: its `new -> new`
 * events are the launch and its dispatch attempts, and reading one as a verdict is how a dispatch
 * failure used to render as ranking rationale. */
export function ranks(kind: InputKind): boolean {
  return kind.type !== 'playbook';
}

/** The rationale of the standing rank verdict, or null before the issue has ever ranked. */
export function rankingRationale(kind: InputKind, events: EventDto[]): string | null {
  return lastRankEvent(kind, events)?.reason?.text ?? null;
}

/** The RFC3339 stamp of the standing rank verdict, or null before the issue has ever ranked. */
export function lastRankedAt(kind: InputKind, events: EventDto[]): string | null {
  return lastRankEvent(kind, events)?.ts ?? null;
}

export type VerdictSource = 'grounded' | 'text';

/** Where the standing tier verdict came from. The journey carries a `grounded` step exactly when a
 * code-grounded verdict is on record for the issue (`issues.grounded_content_hash`, the
 * low-confidence escalation or the pre-scope confirmation gate); without one the verdict is the
 * text-tier ranker's alone. Null when the issue has never ranked at all. */
export function verdictSource(steps: JourneyStep[]): VerdictSource | null {
  if (!steps.some((s) => s.kind === 'ranked')) return null;
  return steps.some((s) => s.kind === 'grounded') ? 'grounded' : 'text';
}
