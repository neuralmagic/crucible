import { describe, expect, it } from 'vitest';
import {
  TURN_STATES,
  truncateReason,
  turnDuration,
  turnStateColor,
} from './turns';

describe('turnStateColor', () => {
  it('maps every known state', () => {
    expect(turnStateColor('queued')).toBe('grey');
    expect(turnStateColor('running')).toBe('blue');
    expect(turnStateColor('succeeded')).toBe('green');
    expect(turnStateColor('failed')).toBe('red');
    expect(turnStateColor('collected')).toBe('green');
    expect(turnStateColor('swept')).toBe('grey');
  });

  it('covers the whole vocabulary (a new state must get a color)', () => {
    for (const state of TURN_STATES) {
      expect(typeof turnStateColor(state)).toBe('string');
    }
  });

  it('falls back to grey for an unknown state rather than throwing', () => {
    expect(turnStateColor('exploded')).toBe('grey');
  });
});

describe('turnDuration', () => {
  const t0 = '2026-07-04T12:00:00Z';

  it('uses terminal_at when the turn finished', () => {
    expect(turnDuration(t0, '2026-07-04T12:00:42Z')).toBe('42s');
    expect(turnDuration(t0, '2026-07-04T12:05:30Z')).toBe('5m 30s');
    expect(turnDuration(t0, '2026-07-04T14:15:00Z')).toBe('2h 15m');
    expect(turnDuration(t0, '2026-07-06T13:00:00Z')).toBe('2d 1h');
  });

  it('falls back to now for an in-flight turn', () => {
    const now = new Date('2026-07-04T12:01:10Z').getTime();
    expect(turnDuration(t0, null, now)).toBe('1m 10s');
    expect(turnDuration(t0, undefined, now)).toBe('1m 10s');
  });

  it('returns null on missing/garbage stamps and negative deltas', () => {
    expect(turnDuration(null, null)).toBeNull();
    expect(turnDuration(undefined, null)).toBeNull();
    expect(turnDuration('not-a-date', null)).toBeNull();
    expect(turnDuration(t0, 'not-a-date')).toBeNull();
    expect(turnDuration(t0, '2026-07-04T11:59:59Z')).toBeNull();
  });
});

/** A reason as the detail endpoint serves it: whole. */
const whole = (text: string) => ({ text, truncated: false });

describe('truncateReason', () => {
  it('passes short single-line reasons through untouched', () => {
    expect(truncateReason(whole('pod timed out'))).toBe('pod timed out');
  });

  it('clips long reasons with an ellipsis', () => {
    expect(truncateReason(whole('x'.repeat(200)), 120)).toBe(`${'x'.repeat(120)}…`);
  });

  it('keeps only the first line of a multiline reason', () => {
    const multi = 'first line\nsecond line with the stack trace';
    expect(truncateReason(whole(multi))).toBe('first line…');
  });

  it('trims trailing whitespace before the ellipsis', () => {
    const padded = `${'y'.repeat(118)}  and more`;
    expect(truncateReason(whole(padded), 120)).toBe(`${'y'.repeat(118)}…`);
  });

  it('is null-safe', () => {
    expect(truncateReason(null)).toBeNull();
    expect(truncateReason(undefined)).toBeNull();
    expect(truncateReason(whole(''))).toBeNull();
  });
});
