// DOM-free presentation logic for the issue journey timeline: the step -> {tone, title, detail}
// mapping, the ghost (upcoming-lifecycle) derivation, and relative/absolute time formatting. Kept
// pure so the whole mapping is unit-tested without rendering (see journeyView.test.ts); the TSX
// layers interactive chrome (live toggle, PR chip) on top of what these functions decide.

import type { components } from '../api/schema.d';

type JourneyStep = components['schemas']['JourneyStep'];
export type StepKind = JourneyStep['kind'];

/// The visual weight of a node. Drives the swatch color.
export type Tone = 'muted' | 'info' | 'success' | 'warning' | 'danger';

export interface StepPresentation {
  tone: Tone;
  title: string;
  detail: string;
}

/// True when a run's status reads as a failure (vs. a clean finish or an in-flight run).
function isFailedStatus(status: string): boolean {
  return /fail|error|kill|abort/i.test(status);
}

/// Map one journey step to its tone/title/detail. Pure: the same step always renders the same
/// way. Interactive affordances (links, the live pane) are layered by the TSX around this base.
export function stepPresentation(step: JourneyStep): StepPresentation {
  switch (step.kind) {
    case 'discovered':
      return { tone: 'info', title: 'Discovered', detail: 'entered the system' };
    case 'ranked':
      return step.disposition === 'stale'
        ? { tone: 'warning', title: 'Ranked', detail: 'superseded — grounded verdict found it already implemented' }
        : { tone: 'info', title: 'Ranked', detail: 'tier assigned' };
    case 'grounded':
      return step.disposition === 'stale'
        ? { tone: 'warning', title: 'Grounded', detail: 'already implemented in the checkout' }
        : { tone: 'info', title: 'Grounded', detail: 'code-grounded verdict recorded' };
    case 'scoped': {
      const rounds = `${step.refine_rounds} refine round${step.refine_rounds === 1 ? '' : 's'}`;
      switch (step.adversary) {
        case 'passed':
          return { tone: 'info', title: 'Scoped', detail: `${rounds} · red-team passed` };
        case 'concerns':
          return { tone: 'warning', title: 'Scoped', detail: `${rounds} · red-team concerns` };
        default:
          return { tone: 'info', title: 'Scoped', detail: rounds };
      }
    }
    case 'approval':
      return {
        tone: 'success',
        title: 'Approved',
        detail: step.approved_by ? `approved by @${step.approved_by}` : 'approval recorded',
      };
    case 'build': {
      const took = formatDuration(step.duration_secs);
      const label = `${step.name} · ${step.backend}`;
      switch (step.state) {
        case 'succeeded':
          return { tone: 'success', title: 'Build', detail: took ? `${label} · built in ${took}` : `${label} · image pinned` };
        case 'failed':
          return { tone: 'danger', title: 'Build', detail: `${label} · failed` };
        case 'timed-out':
          return { tone: 'danger', title: 'Build', detail: `${label} · timed out` };
        case 'dispatched':
          return { tone: 'info', title: 'Build', detail: `${label} · building` };
        default:
          return { tone: 'info', title: 'Build', detail: `${label} · queued` };
      }
    }
    case 'run': {
      const score = step.best_score !== null && step.best_score !== undefined ? ` · best ${step.best_score.toFixed(1)}` : '';
      if (step.live) {
        return { tone: 'info', title: 'Run', detail: `running${score}` };
      }
      if (isFailedStatus(step.status)) {
        return { tone: 'danger', title: 'Run', detail: `${step.status}${score}` };
      }
      return { tone: 'success', title: 'Run', detail: `${step.status}${score}` };
    }
    case 'pr':
      return { tone: 'success', title: 'PR opened', detail: step.repo };
    case 'parked':
      return {
        tone: 'warning',
        title: 'Parked',
        detail: step.by ? `parked by ${step.by}` : 'parked',
      };
    case 'done':
      return { tone: 'success', title: 'Done', detail: 'issue resolved' };
    case 'stale':
      return { tone: 'warning', title: 'Stale', detail: 'already implemented in the checkout' };
  }
}

/// The lifecycle stages we render as ghost (pending) nodes below the real ones, in canonical order.
/// Grounding is optional and intentionally omitted (it is not a gate every issue passes).
export const LIFECYCLE = ['discovered', 'ranked', 'scoped', 'approval', 'run', 'pr'] as const;
export type LifecycleKind = (typeof LIFECYCLE)[number];

const TERMINAL: ReadonlySet<StepKind> = new Set<StepKind>(['parked', 'done', 'stale']);

const GHOST_TITLES: Record<LifecycleKind, string> = {
  discovered: 'Discovery',
  ranked: 'Ranking',
  scoped: 'Scoping',
  approval: 'Approval',
  run: 'Run',
  pr: 'PR',
};

/// The remaining lifecycle stages to show as ghost nodes, derived from what is absent. Once a
/// terminal step (parked/done/stale) exists the journey is over, so no ghosts. Otherwise: find the
/// furthest lifecycle stage reached and return every canonical stage after it (all absent, since the
/// normal flow is monotonic). A brand-new issue (only `discovered`) previews ranked -> pr.
export function ghostSteps(steps: JourneyStep[]): LifecycleKind[] {
  if (steps.some((s) => TERMINAL.has(s.kind))) return [];
  let maxIndex = -1;
  for (const step of steps) {
    const idx = LIFECYCLE.findIndex((k) => k === step.kind);
    if (idx > maxIndex) maxIndex = idx;
  }
  return LIFECYCLE.filter((_, i) => i > maxIndex);
}

/// The ghost node's title.
export function ghostTitle(kind: LifecycleKind): string {
  return GHOST_TITLES[kind];
}

/// A compact "1m 5s" / "45s" / "2h 3m" build duration from whole seconds. Null for a missing count
/// so the caller can fall back to a duration-free phrasing.
export function formatDuration(secs: number | null | undefined): string | null {
  if (secs === null || secs === undefined || Number.isNaN(secs) || secs < 0) return null;
  if (secs < 60) return `${secs}s`;
  const min = Math.floor(secs / 60);
  if (min < 60) {
    const rem = secs % 60;
    return rem ? `${min}m ${rem}s` : `${min}m`;
  }
  const hr = Math.floor(min / 60);
  const remMin = min % 60;
  return remMin ? `${hr}h ${remMin}m` : `${hr}h`;
}

/// "3m ago" / "2h ago" style relative stamp, matching the rest of the control plane. Returns null
/// for a missing timestamp so the caller can omit the line entirely.
export function relativeTime(iso: string | null | undefined): string | null {
  if (!iso) return null;
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return null;
  const deltaSec = Math.floor((Date.now() - then) / 1000);
  if (deltaSec < 0) return 'just now';
  if (deltaSec < 60) return `${deltaSec}s ago`;
  const deltaMin = Math.floor(deltaSec / 60);
  if (deltaMin < 60) return `${deltaMin}m ago`;
  const deltaHr = Math.floor(deltaMin / 60);
  if (deltaHr < 24) return `${deltaHr}h ago`;
  const deltaDay = Math.floor(deltaHr / 24);
  return `${deltaDay}d ago`;
}

/// Full local timestamp for the hover title. Null when there is no stamp.
export function absoluteTime(iso: string | null | undefined): string | null {
  if (!iso) return null;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return null;
  return d.toLocaleString();
}
