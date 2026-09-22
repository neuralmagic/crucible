import type { components } from '../api/schema.d';

type OverrideSet = components['schemas']['OverrideSet'];
type EventDto = components['schemas']['EventDto'];
type Source = components['schemas']['Source'];

// The slice of KnobView these helpers actually read. The generated KnobView types `value` as
// utoipa's opaque object, but the runtime shape is any JSON scalar/array — `unknown` is the honest
// model, and KnobView is assignable to this, so callers pass the API rows straight through.
export interface ConfigKnob {
  name: string;
  value: unknown;
  source: Source;
}

// The overridable knobs are a closed set (mirrors the Rust `Knob` enum): every key here is a field
// of `OverrideSet`, so the record is exhaustive — add a knob to the schema and this fails to compile
// until it's classified, which is exactly the staleness guard we want.
export type KnobKey = keyof OverrideSet;
export type KnobKind = 'int' | 'float' | 'bool' | 'tiers';

export const KNOB_KINDS: Record<KnobKey, KnobKind> = {
  discovery_secs: 'int',
  per_reconcile_cost: 'float',
  max_concurrent_pods: 'int',
  max_scopes_per_day: 'int',
  daily_cost_ceiling: 'float',
  rank_cost_fallback_usd: 'float',
  grounded_rank_pod_cap: 'int',
  grounded_rank_daily_turns: 'int',
  allow_t3: 'bool',
  allowed_tiers: 'tiers',
  prescope_grounded: 'bool',
  rank_horizon_days: 'int',
  run_iterations: 'int',
  run_max_cost: 'float',
  failed_pod_keep: 'int',
  scope_gaming_rounds: 'int',
  scope_skip_gaming_review: 'bool',
  allow_draft_head_schedules: 'bool',
};

// The lowercase wire spelling `allowed_tiers` accepts (Tier::parse_lower on the Rust side).
export const ALL_TIERS = ['t0', 't1', 't2', 't3'] as const;

// allow_t3 survives only as a deprecated alias (true unions t3 into allowed_tiers); flagged so the
// table row can badge it beyond the API description.
export const DEPRECATED_KNOBS: ReadonlySet<KnobKey> = new Set<KnobKey>(['allow_t3']);

export function isKnobKey(name: string): name is KnobKey {
  return Object.prototype.hasOwnProperty.call(KNOB_KINDS, name);
}

export function formatKnobValue(value: unknown): string {
  if (typeof value === 'boolean') return value ? 'true' : 'false';
  if (typeof value === 'number') return String(value);
  if (typeof value === 'string') return value;
  if (Array.isArray(value) && value.every((v) => typeof v === 'string')) return value.join(', ');
  if (value === null || value === undefined) return '—';
  return JSON.stringify(value);
}

// Reconstruct the current desired override set from the effective config: only knobs whose source is
// `override` are actually pinned; the rest fall back to env/default and stay absent (null). This is
// the read half of the read-modify-write against the full-set-replace PUT.
export function currentOverrideSet(knobs: ConfigKnob[]): OverrideSet {
  const pinned = new Map<string, unknown>();
  for (const k of knobs) {
    if (k.source === 'override') pinned.set(k.name, k.value);
  }
  const num = (key: KnobKey): number | null => {
    const v = pinned.get(key);
    return typeof v === 'number' ? v : null;
  };
  const bool = (key: KnobKey): boolean | null => {
    const v = pinned.get(key);
    return typeof v === 'boolean' ? v : null;
  };
  const tiers = (key: KnobKey): string[] | null => {
    const v = pinned.get(key);
    return Array.isArray(v) && v.every((t) => typeof t === 'string') ? v : null;
  };
  return {
    discovery_secs: num('discovery_secs'),
    per_reconcile_cost: num('per_reconcile_cost'),
    max_concurrent_pods: num('max_concurrent_pods'),
    max_scopes_per_day: num('max_scopes_per_day'),
    daily_cost_ceiling: num('daily_cost_ceiling'),
    rank_cost_fallback_usd: num('rank_cost_fallback_usd'),
    grounded_rank_pod_cap: num('grounded_rank_pod_cap'),
    grounded_rank_daily_turns: num('grounded_rank_daily_turns'),
    allow_t3: bool('allow_t3'),
    allowed_tiers: tiers('allowed_tiers'),
    prescope_grounded: bool('prescope_grounded'),
    rank_horizon_days: num('rank_horizon_days'),
    run_iterations: num('run_iterations'),
    run_max_cost: num('run_max_cost'),
    failed_pod_keep: num('failed_pod_keep'),
    scope_gaming_rounds: num('scope_gaming_rounds'),
    scope_skip_gaming_review: bool('scope_skip_gaming_review'),
    allow_draft_head_schedules: bool('allow_draft_head_schedules'),
  };
}

export type KnobValue = number | boolean | string[] | null;

// Set one knob on a copy of the set. A closed switch (not a computed key) so each field keeps its
// concrete type and the `never` default catches any un-handled knob if the schema grows.
export function withKnobValue(base: OverrideSet, key: KnobKey, value: KnobValue): OverrideSet {
  const next: OverrideSet = { ...base };
  const asNum = typeof value === 'number' ? value : null;
  const asBool = typeof value === 'boolean' ? value : null;
  const asTiers = Array.isArray(value) ? value : null;
  switch (key) {
    case 'discovery_secs': next.discovery_secs = asNum; break;
    case 'per_reconcile_cost': next.per_reconcile_cost = asNum; break;
    case 'max_concurrent_pods': next.max_concurrent_pods = asNum; break;
    case 'max_scopes_per_day': next.max_scopes_per_day = asNum; break;
    case 'daily_cost_ceiling': next.daily_cost_ceiling = asNum; break;
    case 'rank_cost_fallback_usd': next.rank_cost_fallback_usd = asNum; break;
    case 'grounded_rank_pod_cap': next.grounded_rank_pod_cap = asNum; break;
    case 'grounded_rank_daily_turns': next.grounded_rank_daily_turns = asNum; break;
    case 'rank_horizon_days': next.rank_horizon_days = asNum; break;
    case 'run_iterations': next.run_iterations = asNum; break;
    case 'run_max_cost': next.run_max_cost = asNum; break;
    case 'failed_pod_keep': next.failed_pod_keep = asNum; break;
    case 'scope_gaming_rounds': next.scope_gaming_rounds = asNum; break;
    case 'scope_skip_gaming_review': next.scope_skip_gaming_review = asBool; break;
    case 'allow_draft_head_schedules': next.allow_draft_head_schedules = asBool; break;
    case 'allow_t3': next.allow_t3 = asBool; break;
    case 'prescope_grounded': next.prescope_grounded = asBool; break;
    case 'allowed_tiers': next.allowed_tiers = asTiers; break;
    default: {
      const exhaustive: never = key;
      return exhaustive;
    }
  }
  return next;
}

// The control-plane activity feed: events that are admin/operator actions, not machine reconcile.
// Config + autopilot ride pseudo-keys; repo add/pause/resume/unwatch are watch-state transitions;
// everything else human-initiated (scope-now, park/unpark/bump) carries an actor.
const REPO_WATCH_STATES = new Set(['watched', 'unwatched', 'watching', 'paused']);

export function isControlPlaneEvent(e: EventDto): boolean {
  if (e.key === 'config' || e.key === 'autopilot') return true;
  if (REPO_WATCH_STATES.has(e.from) || REPO_WATCH_STATES.has(e.to)) return true;
  return e.actor != null && e.actor.trim() !== '';
}
