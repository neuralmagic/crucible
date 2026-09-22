import { describe, expect, it } from 'vitest';
import {
  firstFailedStage,
  formatUsd,
  hasBreakdown,
  reportHeadline,
  stageTone,
  type ScopeReport,
  type ScopeStage,
} from './scopeReport';

const stage = (name: string, passed: boolean, detail = ''): ScopeStage => ({ name, passed, detail });

const report = (over: Partial<ScopeReport>): ScopeReport => ({
  issue_key: 'o/r#1',
  pod_name: null,
  survived: false,
  created_at: '2026-07-05T00:00:00Z',
  stages: [],
  digest: null,
  cost: null,
  rounds: [],
  ...over,
});

describe('firstFailedStage', () => {
  it('picks the first failing stage in pipeline order', () => {
    const failed = firstFailedStage([
      stage('ingest', true),
      stage('propose', false, 'refine exhausted'),
      stage('validate', false, 'never reached'),
    ]);
    expect(failed?.name).toBe('propose');
    expect(failed?.detail).toBe('refine exhausted');
  });

  it('is null when every stage passed', () => {
    expect(firstFailedStage([stage('ingest', true), stage('freeze', true)])).toBeNull();
  });

  it('is null for an empty stage list', () => {
    expect(firstFailedStage([])).toBeNull();
  });
});

describe('stageTone', () => {
  it('maps pass/fail to green/red', () => {
    expect(stageTone(stage('ingest', true))).toBe('green');
    expect(stageTone(stage('propose', false))).toBe('red');
  });
});

describe('reportHeadline', () => {
  it('names the failing stage with its detail', () => {
    const r = report({
      stages: [stage('ingest', true), stage('propose', false, 'no crucible.toml was written')],
    });
    expect(reportHeadline(r)).toBe('propose failed — no crucible.toml was written');
  });

  it('shows the digest on survival', () => {
    const r = report({ survived: true, digest: 'v1:abc123', stages: [stage('freeze', true)] });
    expect(reportHeadline(r)).toBe('pack frozen — v1:abc123');
  });

  it('survival without a digest still reads as frozen', () => {
    expect(reportHeadline(report({ survived: true }))).toBe('pack frozen');
  });

  it('failure with no recorded stages says so', () => {
    expect(reportHeadline(report({}))).toBe('scope turn failed (no stage detail recorded)');
  });
});

describe('formatUsd', () => {
  it('renders four decimal places', () => {
    expect(formatUsd(0.42)).toBe('$0.4200');
    expect(formatUsd(0)).toBe('$0.0000');
  });

  it('is null when the report carried no cost', () => {
    expect(formatUsd(null)).toBeNull();
    expect(formatUsd(undefined)).toBeNull();
  });
});

describe('hasBreakdown', () => {
  it('is false for a drifted/empty decode', () => {
    expect(hasBreakdown(report({}))).toBe(false);
  });

  it('is true with stages or rounds', () => {
    expect(hasBreakdown(report({ stages: [stage('ingest', true)] }))).toBe(true);
    expect(
      hasBreakdown(
        report({
          rounds: [
            { round: 1, kind: 'propose', judge_block: '', cost: 0, outcome: { result: 'passed' } },
          ],
        }),
      ),
    ).toBe(true);
  });
});
