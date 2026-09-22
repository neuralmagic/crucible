import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema.d';
import { lastRankedAt, ranks, rankingRationale, verdictSource } from './rankProvenance';

type EventDto = components['schemas']['EventDto'];
type JourneyStep = components['schemas']['JourneyStep'];

const event = (
  from: string,
  to: string,
  reason: string | null,
  ts = '2026-07-01T00:00:00Z',
): EventDto => ({
  ts,
  key: 'o/r#1',
  from,
  to,
  // The feed serves `reason` as a LongText; these fixtures are all short enough to ride whole.
  reason: reason === null ? null : { text: reason, truncated: false },
  evidence: null,
  actor: null,
});

const GITHUB: components['schemas']['InputKindDto'] = { type: 'github', owner: 'o', repo: 'r', number: 1 };
const LAUNCH: components['schemas']['InputKindDto'] = {
  type: 'playbook',
  playbook: 'fences',
  launch: '01a02ffb',
};

describe('lastRankedAt / rankingRationale', () => {
  it('returns null when no rank verdict has ever landed', () => {
    expect(lastRankedAt(GITHUB, [])).toBeNull();
    expect(rankingRationale(GITHUB, [])).toBeNull();
    const noRank = [event('new', 'scoped', 'pack passed'), event('scoped', 'running', null)];
    expect(lastRankedAt(GITHUB, noRank)).toBeNull();
    expect(rankingRationale(GITHUB, noRank)).toBeNull();
  });

  it('picks the newest new->new event with a reason (a re-rank supersedes)', () => {
    const events = [
      event('new', 'new', 'ranked T2, first pass', '2026-07-01T00:00:00Z'),
      event('new', 'scoped', 'pack passed', '2026-07-02T00:00:00Z'),
      event('new', 'new', 'ranked T1 after content change', '2026-07-03T00:00:00Z'),
    ];
    expect(lastRankedAt(GITHUB, events)).toBe('2026-07-03T00:00:00Z');
    expect(rankingRationale(GITHUB, events)).toBe('ranked T1 after content change');
  });

  it('skips reason-less new->new transitions', () => {
    const events = [
      event('new', 'new', 'ranked T2', '2026-07-01T00:00:00Z'),
      event('new', 'new', null, '2026-07-04T00:00:00Z'),
    ];
    expect(lastRankedAt(GITHUB, events)).toBe('2026-07-01T00:00:00Z');
    expect(rankingRationale(GITHUB, events)).toBe('ranked T2');
  });
});

describe('verdictSource', () => {
  const ranked: JourneyStep = { kind: 'ranked', at: null, tier: 'T1', disposition: 'tier' };
  const grounded: JourneyStep = { kind: 'grounded', at: null, disposition: 'tier' };
  const discovered: JourneyStep = { kind: 'discovered', at: null };

  it('is null before any rank verdict exists', () => {
    expect(verdictSource([])).toBeNull();
    expect(verdictSource([discovered])).toBeNull();
  });

  it('reads text when only the API-tier verdict stands', () => {
    expect(verdictSource([discovered, ranked])).toBe('text');
  });

  it('reads grounded when a code-grounded verdict is on record', () => {
    expect(verdictSource([discovered, ranked, grounded])).toBe('grounded');
  });
});

describe('ranks', () => {
  /// A launch's `new -> new` events are its authorization and its dispatch attempts; reading the
  /// newest as a verdict is what put "playbook dispatch failed" under RANKING RATIONALE.
  it('excludes a playbook launch, so no dispatch failure reads as a verdict', () => {
    expect(ranks(LAUNCH)).toBe(false);
    expect(ranks(GITHUB)).toBe(true);
    const events = [event('new', 'new', 'playbook dispatch failed', '2026-08-23T00:00:00Z')];
    expect(rankingRationale(LAUNCH, events)).toBeNull();
    expect(lastRankedAt(LAUNCH, events)).toBeNull();
    expect(rankingRationale(GITHUB, events)).toBe('playbook dispatch failed');
  });
});
