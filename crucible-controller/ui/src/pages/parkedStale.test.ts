import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema.d';
import { filterStaleClosable, isStaleClosable, staleCloseEvidence } from './parkedStale';

type EventDto = components['schemas']['EventDto'];

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

describe('isStaleClosable', () => {
  it('is true when the server flags the issue stale-closable', () => {
    expect(isStaleClosable({ stale_closable: true })).toBe(true);
  });

  it('is false when the server omits or clears the flag', () => {
    expect(isStaleClosable({ stale_closable: false })).toBe(false);
    expect(isStaleClosable({})).toBe(false);
  });
});

describe('filterStaleClosable', () => {
  const issues = [
    { key: 'a', stale_closable: true },
    { key: 'b', stale_closable: false },
    { key: 'c', stale_closable: undefined },
  ];

  it('passes every issue through unchanged when disabled', () => {
    expect(filterStaleClosable(issues, false)).toEqual(issues);
  });

  it('keeps only stale-closable issues when enabled', () => {
    expect(filterStaleClosable(issues, true).map((i) => i.key)).toEqual(['a']);
  });

  it('composes with an already-narrowed list (an empty input stays empty either way)', () => {
    expect(filterStaleClosable([], true)).toEqual([]);
    expect(filterStaleClosable([], false)).toEqual([]);
  });
});

const GITHUB: components['schemas']['InputKindDto'] = { type: 'github', owner: 'o', repo: 'r', number: 1 };

describe('staleCloseEvidence', () => {
  it('is null for a non-parked issue', () => {
    expect(staleCloseEvidence(GITHUB, 'new', true, [])).toBeNull();
  });

  it('is null for an ordinary park', () => {
    expect(staleCloseEvidence(GITHUB, 'parked', false, [])).toBeNull();
  });

  it('carries the standing rank event as evidence for a stale park', () => {
    const events = [
      event('new', 'new', 'already implemented: see server.go lines 40-52', '2026-07-01T00:00:00Z'),
    ];
    const result = staleCloseEvidence(GITHUB, 'parked', true, events);
    expect(result?.evidenceEvent?.reason?.text).toBe('already implemented: see server.go lines 40-52');
  });

  it('picks the newest evidence event when the issue was re-ranked stale more than once', () => {
    const events = [
      event('new', 'new', 'already implemented: v1 evidence', '2026-07-01T00:00:00Z'),
      event('new', 'new', 'already implemented: v2 evidence', '2026-07-03T00:00:00Z'),
    ];
    const result = staleCloseEvidence(GITHUB, 'parked', true, events);
    expect(result?.evidenceEvent?.reason?.text).toBe('already implemented: v2 evidence');
  });

  it('evidenceEvent is null when the park predates any reasoned event', () => {
    const result = staleCloseEvidence(GITHUB, 'parked', true, []);
    expect(result).toEqual({ evidenceEvent: null });
  });
});

describe('staleCloseEvidence for a playbook launch', () => {
  it('finds no evidence event: a launch never ranked, so its events are not verdicts', () => {
    const launch: components['schemas']['InputKindDto'] = {
      type: 'playbook',
      playbook: 'fences',
      launch: '01a02ffb',
    };
    const events = [event('new', 'new', 'playbook dispatch failed', '2026-08-23T00:00:00Z')];
    expect(staleCloseEvidence(launch, 'parked', true, events)).toEqual({ evidenceEvent: null });
  });
});
