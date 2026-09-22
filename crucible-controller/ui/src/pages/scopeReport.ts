// Pure shaping helpers for the scope progress page's structured report view
// (GET /api/issues/{key}/scope-report), kept side-effect-free so scopeReport.test.ts can
// exercise them without any DOM. Round-level presentation reuses approvalEvidence.ts.
import type { components } from '../api/schema';
import type { Tone } from './approvalEvidence';

export type ScopeReport = components['schemas']['ScopeReportDto'];
export type ScopeStage = components['schemas']['ScopeStage'];

/** The stage the pipeline stopped on, or null when every recorded stage passed. */
export function firstFailedStage(stages: ScopeStage[]): ScopeStage | null {
  return stages.find((s) => !s.passed) ?? null;
}

/** The tone a stage chip renders with. */
export function stageTone(stage: ScopeStage): Tone {
  return stage.passed ? 'green' : 'red';
}

/** One-line headline for the report card: the failing stage, or the frozen digest. */
export function reportHeadline(report: ScopeReport): string {
  if (report.survived) {
    return report.digest ? `pack frozen — ${report.digest}` : 'pack frozen';
  }
  const failed = firstFailedStage(report.stages);
  if (failed) {
    return `${failed.name} failed — ${failed.detail}`;
  }
  return 'scope turn failed (no stage detail recorded)';
}

/** `$0.4200`-style USD, or null when the report carried no cost to show. */
export function formatUsd(cost: number | null | undefined): string | null {
  if (cost === null || cost === undefined) return null;
  return `$${cost.toFixed(4)}`;
}

/**
 * Whether the report has any structured breakdown worth rendering. An older row (or one whose
 * stored JSON drifted past the mirror) decodes to empty stages + rounds; the page falls back to
 * the prose park reason for those.
 */
export function hasBreakdown(report: ScopeReport): boolean {
  return report.stages.length > 0 || report.rounds.length > 0;
}
