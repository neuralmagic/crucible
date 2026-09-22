import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema.d';
import {
  ALL_TIERS,
  DEPRECATED_KNOBS,
  KNOB_KINDS,
  currentOverrideSet,
  formatKnobValue,
  isControlPlaneEvent,
  isKnobKey,
  withKnobValue,
  type ConfigKnob,
  type KnobKey,
} from './adminConfig';

type OverrideSet = components['schemas']['OverrideSet'];
type EventDto = components['schemas']['EventDto'];

const knob = (name: string, value: unknown, source: ConfigKnob['source']): ConfigKnob => ({
  name,
  value,
  source,
});

const EMPTY: OverrideSet = {
  allow_draft_head_schedules: null,
  discovery_secs: null,
  per_reconcile_cost: null,
  max_concurrent_pods: null,
  max_scopes_per_day: null,
  daily_cost_ceiling: null,
  rank_cost_fallback_usd: null,
  grounded_rank_pod_cap: null,
  grounded_rank_daily_turns: null,
  allow_t3: null,
  allowed_tiers: null,
  prescope_grounded: null,
  rank_horizon_days: null,
  run_iterations: null,
  run_max_cost: null,
  failed_pod_keep: null,
  scope_gaming_rounds: null,
  scope_skip_gaming_review: null,
};

describe('KNOB_KINDS', () => {
  it('classifies every OverrideSet knob (the record is the exhaustiveness guard)', () => {
    // Compile-time exhaustiveness is the real check; at runtime just pin the closed set so a
    // schema regen that adds a knob shows up here too.
    expect(Object.keys(KNOB_KINDS).sort()).toEqual(Object.keys(EMPTY).sort());
  });

  it('classifies allowed_tiers as the tier checkbox set and allow_t3 as the deprecated bool alias', () => {
    expect(KNOB_KINDS.allowed_tiers).toBe('tiers');
    expect(KNOB_KINDS.allow_t3).toBe('bool');
    expect(DEPRECATED_KNOBS.has('allow_t3')).toBe(true);
    expect(DEPRECATED_KNOBS.has('allowed_tiers')).toBe(false);
  });

  it('isKnobKey narrows known names and rejects unknown ones', () => {
    expect(isKnobKey('allowed_tiers')).toBe(true);
    expect(isKnobKey('discovery_secs')).toBe(true);
    expect(isKnobKey('not_a_knob')).toBe(false);
  });
});

describe('formatKnobValue', () => {
  it('renders scalars, tier arrays, and null', () => {
    expect(formatKnobValue(true)).toBe('true');
    expect(formatKnobValue(3.5)).toBe('3.5');
    expect(formatKnobValue(['t0', 't1'])).toBe('t0, t1');
    expect(formatKnobValue(null)).toBe('—');
  });
});

describe('currentOverrideSet', () => {
  it('pins only source=override knobs; env/default stay null', () => {
    const set = currentOverrideSet([
      knob('max_concurrent_pods', 4, 'override'),
      knob('daily_cost_ceiling', 100, 'env'),
      knob('allow_t3', true, 'default'),
      knob('allowed_tiers', ['t0', 't3'], 'override'),
    ]);
    expect(set.max_concurrent_pods).toBe(4);
    expect(set.allowed_tiers).toEqual(['t0', 't3']);
    expect(set.daily_cost_ceiling).toBeNull();
    expect(set.allow_t3).toBeNull();
  });

  it('drops an override whose value has the wrong runtime shape', () => {
    const set = currentOverrideSet([
      knob('allowed_tiers', 'not-an-array', 'override'),
      knob('max_scopes_per_day', 'ten', 'override'),
    ]);
    expect(set.allowed_tiers).toBeNull();
    expect(set.max_scopes_per_day).toBeNull();
  });
});

describe('withKnobValue', () => {
  it('sets exactly the one knob and leaves the rest untouched', () => {
    const next = withKnobValue({ ...EMPTY, discovery_secs: 60 }, 'allowed_tiers', ['t0', 't1', 't2']);
    expect(next.allowed_tiers).toEqual(['t0', 't1', 't2']);
    expect(next.discovery_secs).toBe(60);
    expect(next.allow_t3).toBeNull();
  });

  it('nulls a knob handed a mismatched value shape instead of writing garbage', () => {
    expect(withKnobValue(EMPTY, 'allow_t3', 7).allow_t3).toBeNull();
    expect(withKnobValue(EMPTY, 'allowed_tiers', true).allowed_tiers).toBeNull();
    expect(withKnobValue(EMPTY, 'max_concurrent_pods', ['t0']).max_concurrent_pods).toBeNull();
  });

  it('round-trips every knob kind through its natural value', () => {
    const cases: Array<[KnobKey, number | boolean | string[]]> = [
      ['discovery_secs', 30],
      ['per_reconcile_cost', 0.25],
      ['allow_t3', true],
      ['prescope_grounded', false],
      ['allowed_tiers', [...ALL_TIERS]],
      ['rank_horizon_days', 7],
      ['scope_gaming_rounds', 3],
      ['scope_skip_gaming_review', true],
    ];
    for (const [key, value] of cases) {
      expect(withKnobValue(EMPTY, key, value)[key]).toEqual(value);
    }
  });
});

describe('isControlPlaneEvent', () => {
  const event = (over: Partial<EventDto>): EventDto => ({
    ts: '2026-07-04T00:00:00Z',
    key: 'o/r#1',
    from: 'new',
    to: 'scoped',
    reason: null,
    actor: null,
    evidence: null,
    ...over,
  });

  it('keeps config and autopilot pseudo-key events', () => {
    expect(isControlPlaneEvent(event({ key: 'config', from: 'config', to: 'config' }))).toBe(true);
    expect(isControlPlaneEvent(event({ key: 'autopilot', from: 'enabled', to: 'disabled' }))).toBe(true);
  });

  it('keeps repo watch-set transitions (add/pause/resume/unwatch)', () => {
    expect(isControlPlaneEvent(event({ key: 'o/r', from: 'unwatched', to: 'watched' }))).toBe(true);
    expect(isControlPlaneEvent(event({ key: 'o/r', from: 'watching', to: 'paused' }))).toBe(true);
    expect(isControlPlaneEvent(event({ key: 'o/r', from: 'paused', to: 'watching' }))).toBe(true);
    expect(isControlPlaneEvent(event({ key: 'o/r', from: 'watched', to: 'unwatched' }))).toBe(true);
  });

  it('keeps actor-tagged (human) issue events, drops machine reconcile', () => {
    expect(isControlPlaneEvent(event({ actor: 'wren' }))).toBe(true);
    expect(isControlPlaneEvent(event({}))).toBe(false);
    expect(isControlPlaneEvent(event({ actor: '  ' }))).toBe(false);
  });
});
