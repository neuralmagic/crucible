import { describe, expect, it, vi, afterEach } from 'vitest';
import type { components } from '../api/schema.d';
import {
  LIFECYCLE,
  absoluteTime,
  formatDuration,
  ghostTitle,
  ghostSteps,
  relativeTime,
  stepPresentation,
} from './journeyView';

type JourneyStep = components['schemas']['JourneyStep'];

describe('stepPresentation', () => {
  it('maps discovered to an info node', () => {
    const view = stepPresentation({ kind: 'discovered', at: null });
    expect(view).toMatchObject({ tone: 'info', title: 'Discovered' });
  });

  it('separates a normal ranked tier from a stale supersession', () => {
    const tiered = stepPresentation({ kind: 'ranked', at: null, tier: 'T1', disposition: 'tier' });
    expect(tiered).toMatchObject({ tone: 'info' });
    const stale = stepPresentation({ kind: 'ranked', at: null, tier: null, disposition: 'stale' });
    expect(stale).toMatchObject({ tone: 'warning' });
  });

  it('turns red-team concerns into a warning node', () => {
    const passed = stepPresentation({ kind: 'scoped', at: null, refine_rounds: 2, adversary: 'passed' });
    expect(passed).toMatchObject({ tone: 'info', detail: '2 refine rounds · red-team passed' });
    const concerns = stepPresentation({ kind: 'scoped', at: null, refine_rounds: 1, adversary: 'concerns' });
    expect(concerns).toMatchObject({ tone: 'warning', detail: '1 refine round · red-team concerns' });
    const none = stepPresentation({ kind: 'scoped', at: null, refine_rounds: 3, adversary: 'none' });
    expect(none.detail).toBe('3 refine rounds');
  });

  it('names the approver on the approval node', () => {
    const view = stepPresentation({ kind: 'approval', at: null, approved_by: 'wren' });
    expect(view).toMatchObject({ tone: 'success', detail: 'approved by @wren' });
    expect(stepPresentation({ kind: 'approval', at: null, approved_by: null }).detail).toBe('approval recorded');
  });

  it('tones a run by live / finished / failed and shows the best score', () => {
    const live = stepPresentation({ kind: 'run', at: null, run_id: 'r', status: 'running', best_score: 12.3, live: true });
    expect(live).toMatchObject({ tone: 'info', detail: 'running · best 12.3' });
    const done = stepPresentation({ kind: 'run', at: null, run_id: 'r', status: 'finished', best_score: 240, live: false });
    expect(done).toMatchObject({ tone: 'success', detail: 'finished · best 240.0' });
    const failed = stepPresentation({ kind: 'run', at: null, run_id: 'r', status: 'failed', best_score: null, live: false });
    expect(failed).toMatchObject({ tone: 'danger', detail: 'failed' });
  });

  it('tones a build by its state and shows backend + duration', () => {
    const base = { kind: 'build' as const, at: null, name: 'loop', backend: 'cluster', image: 'quay.io/x/loop', digest: null, evidence: null, duration_secs: null, live: false };
    const ok = stepPresentation({ ...base, state: 'succeeded', digest: 'quay.io/x/loop@sha256:dead', duration_secs: 65 });
    expect(ok).toMatchObject({ tone: 'success', detail: 'loop · cluster · built in 1m 5s' });
    // Succeeded without a duration falls back to "image pinned".
    expect(stepPresentation({ ...base, state: 'succeeded' }).detail).toBe('loop · cluster · image pinned');
    const failed = stepPresentation({ ...base, state: 'failed', evidence: 'https://x/runs/9', live: false });
    expect(failed).toMatchObject({ tone: 'danger', detail: 'loop · cluster · failed' });
    expect(stepPresentation({ ...base, state: 'timed-out' })).toMatchObject({ tone: 'danger', detail: 'loop · cluster · timed out' });
    expect(stepPresentation({ ...base, state: 'dispatched', live: true })).toMatchObject({ tone: 'info', detail: 'loop · cluster · building' });
    expect(stepPresentation({ ...base, state: 'pending', live: true })).toMatchObject({ tone: 'info', detail: 'loop · cluster · queued' });
  });

  it('makes the pr node a success and carries the repo', () => {
    const view = stepPresentation({ kind: 'pr', at: null, url: 'https://x/pull/1', repo: 'o/r' });
    expect(view).toMatchObject({ tone: 'success', title: 'PR opened', detail: 'o/r' });
  });

  it('renders the three terminals', () => {
    expect(stepPresentation({ kind: 'parked', at: null, by: 'wren', reason: 'x' })).toMatchObject({ tone: 'warning', detail: 'parked by wren' });
    expect(stepPresentation({ kind: 'parked', at: null, by: null, reason: null }).detail).toBe('parked');
    expect(stepPresentation({ kind: 'done', at: null })).toMatchObject({ tone: 'success' });
    expect(stepPresentation({ kind: 'stale', at: null, evidence: 'e' })).toMatchObject({ tone: 'warning' });
  });
});

describe('formatDuration', () => {
  it('formats seconds/minutes/hours and rejects bad input', () => {
    expect(formatDuration(45)).toBe('45s');
    expect(formatDuration(65)).toBe('1m 5s');
    expect(formatDuration(120)).toBe('2m');
    expect(formatDuration(3723)).toBe('1h 2m');
    expect(formatDuration(7200)).toBe('2h');
    expect(formatDuration(null)).toBeNull();
    expect(formatDuration(-1)).toBeNull();
  });
});

describe('ghostSteps', () => {
  const step = (kind: JourneyStep['kind']): JourneyStep => {
    switch (kind) {
      case 'ranked': return { kind, at: null, tier: 'T1', disposition: 'tier' };
      case 'scoped': return { kind, at: null, refine_rounds: 1, adversary: 'passed' };
      case 'approval': return { kind, at: null, approved_by: 'x' };
      case 'run': return { kind, at: null, run_id: 'r', status: 'finished', best_score: null, live: false };
      case 'pr': return { kind, at: null, url: 'u', repo: 'o/r' };
      case 'parked': return { kind, at: null, by: null, reason: null };
      case 'stale': return { kind, at: null, evidence: null };
      case 'grounded': return { kind, at: null, disposition: 'tier' };
      default: return { kind: 'discovered', at: null };
    }
  };

  it('previews the whole lifecycle for a brand-new issue', () => {
    expect(ghostSteps([step('discovered')])).toEqual(['ranked', 'scoped', 'approval', 'run', 'pr']);
  });

  it('only previews stages after the furthest one reached', () => {
    expect(ghostSteps([step('discovered'), step('ranked'), step('scoped')])).toEqual(['approval', 'run', 'pr']);
    expect(ghostSteps([step('discovered'), step('ranked'), step('approval'), step('run')])).toEqual(['pr']);
  });

  it('ignores off-lifecycle stages like grounded when computing the frontier', () => {
    expect(ghostSteps([step('discovered'), step('ranked'), step('grounded')])).toEqual(['scoped', 'approval', 'run', 'pr']);
  });

  it('treats an in-flight build as off-lifecycle, still previewing run + pr', () => {
    const build: JourneyStep = { kind: 'build', at: null, name: 'loop', backend: 'cluster', state: 'dispatched', image: 'i', digest: null, evidence: null, duration_secs: null, live: true };
    expect(ghostSteps([step('discovered'), step('scoped'), step('approval'), build])).toEqual(['run', 'pr']);
  });

  it('shows no ghosts once a run has opened a PR', () => {
    expect(ghostSteps([step('discovered'), step('run'), step('pr')])).toEqual([]);
  });

  it('shows no ghosts for any terminal', () => {
    expect(ghostSteps([step('discovered'), step('parked')])).toEqual([]);
    expect(ghostSteps([step('discovered'), step('ranked'), step('stale')])).toEqual([]);
  });
});

describe('ghostTitle', () => {
  it('labels every lifecycle stage', () => {
    for (const kind of LIFECYCLE) {
      expect(ghostTitle(kind).length).toBeGreaterThan(0);
    }
    expect(ghostTitle('approval')).toBe('Approval');
  });
});

describe('relativeTime / absoluteTime', () => {
  afterEach(() => vi.useRealTimers());

  it('returns null for a missing or garbage stamp', () => {
    expect(relativeTime(null)).toBeNull();
    expect(relativeTime(undefined)).toBeNull();
    expect(relativeTime('not-a-date')).toBeNull();
    expect(absoluteTime(null)).toBeNull();
    expect(absoluteTime('nope')).toBeNull();
  });

  it('buckets deltas into s/m/h/d', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-07-04T12:00:00Z'));
    expect(relativeTime('2026-07-04T11:59:30Z')).toBe('30s ago');
    expect(relativeTime('2026-07-04T11:30:00Z')).toBe('30m ago');
    expect(relativeTime('2026-07-04T09:00:00Z')).toBe('3h ago');
    expect(relativeTime('2026-07-01T12:00:00Z')).toBe('3d ago');
    expect(relativeTime('2026-07-04T12:00:05Z')).toBe('just now');
  });

  it('absoluteTime produces a non-empty local string for a valid stamp', () => {
    expect(absoluteTime('2026-07-04T12:00:00Z')).not.toBeNull();
  });
});
