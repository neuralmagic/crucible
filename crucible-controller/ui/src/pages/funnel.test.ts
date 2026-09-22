import { describe, expect, it } from 'vitest';
import {
  IN_BAND_KEYS,
  OUT_OF_BAND_KEYS,
  isZeroStage,
  splitFunnelStages,
  stageTone,
  type FunnelStage,
} from './funnel';

const stage = (key: string, count: number): FunnelStage => ({
  key,
  label: key,
  count,
  href_hint: `/issues?status=${key}`,
});

describe('stageTone', () => {
  it('classifies every known stage key', () => {
    expect(stageTone('discovered')).toBe('discovery');
    expect(stageTone('ranked')).toBe('discovery');
    expect(stageTone('awaiting_approval')).toBe('gate');
    expect(stageTone('running')).toBe('progress');
    expect(stageTone('pr_open')).toBe('progress');
    expect(stageTone('done')).toBe('success');
    expect(stageTone('parked')).toBe('attention');
    expect(stageTone('stale')).toBe('attention');
  });

  it('falls back to discovery for an unrecognized key rather than throwing', () => {
    expect(stageTone('some_new_stage')).toBe('discovery');
  });
});

describe('splitFunnelStages', () => {
  it('orders in-band and out-of-band stages canonically regardless of input order', () => {
    const stages = [
      stage('stale', 1),
      stage('done', 4),
      stage('discovered', 2),
      stage('parked', 3),
      stage('running', 5),
      stage('ranked', 0),
      stage('awaiting_approval', 0),
      stage('pr_open', 0),
    ];

    const { inBand, outOfBand } = splitFunnelStages(stages);
    expect(inBand.map((s) => s.key)).toEqual([...IN_BAND_KEYS]);
    expect(outOfBand.map((s) => s.key)).toEqual([...OUT_OF_BAND_KEYS]);
  });

  it('drops a band from the split entirely when the server response omits its keys', () => {
    const { inBand, outOfBand } = splitFunnelStages([stage('discovered', 1)]);
    expect(inBand.map((s) => s.key)).toEqual(['discovered']);
    expect(outOfBand).toEqual([]);
  });
});

describe('isZeroStage', () => {
  it('is true only at exactly zero', () => {
    expect(isZeroStage(stage('done', 0))).toBe(true);
    expect(isZeroStage(stage('done', 1))).toBe(false);
  });
});
