import type { components } from '../api/schema.d';

export type FunnelStage = components['schemas']['FunnelStage'];

// The two visual bands the dashboard's funnel strip renders: the in-band pipeline (every issue
// lands in exactly one of these) and the out-of-band call-outs (a `parked` sub-population, not
// additional volume — see `crucible-controller/src/operations.rs::funnel_counts`). Keyed here
// (not just assumed from server order) so a stage the server drops or renames fails a render
// with an empty band instead of silently misplacing a box.
export const IN_BAND_KEYS = [
  'discovered',
  'ranked',
  'awaiting_approval',
  'running',
  'pr_open',
  'done',
] as const;

export const OUT_OF_BAND_KEYS = ['parked', 'stale'] as const;

export type StageTone = 'discovery' | 'progress' | 'gate' | 'success' | 'attention';

// Stage-tone accents: where each key sits in the pipeline's overall arc. `awaiting_approval` gets its
// own `gate` tone (a human decision point, distinct from machine-driven `progress`); `parked` and
// `stale` both read as `attention` (out-of-band, worth a second look).
const STAGE_TONES: Record<string, StageTone> = {
  discovered: 'discovery',
  ranked: 'discovery',
  awaiting_approval: 'gate',
  running: 'progress',
  pr_open: 'progress',
  done: 'success',
  parked: 'attention',
  stale: 'attention',
};

// Fallback for a stage key the frontend doesn't recognize yet (a server rename/addition ahead of
// a UI regen) — reads as `discovery` rather than throwing, so the strip still renders.
export function stageTone(key: string): StageTone {
  return STAGE_TONES[key] ?? 'discovery';
}

/** Order `stages` into the in-band pipeline row and the out-of-band call-out row, each in the
 * canonical key order above regardless of the order the server happened to emit them in. A key
 * missing from the response is simply absent from its band (the box just doesn't render), rather
 * than a caller-visible error. */
export function splitFunnelStages(stages: FunnelStage[]): {
  inBand: FunnelStage[];
  outOfBand: FunnelStage[];
} {
  const byKey = new Map(stages.map((s) => [s.key, s]));
  const pick = (keys: readonly string[]): FunnelStage[] =>
    keys.map((k) => byKey.get(k)).filter((s): s is FunnelStage => s !== undefined);
  return {
    inBand: pick(IN_BAND_KEYS),
    outOfBand: pick(OUT_OF_BAND_KEYS),
  };
}

export function isZeroStage(stage: FunnelStage): boolean {
  return stage.count === 0;
}
