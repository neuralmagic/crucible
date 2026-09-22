import { describe, expect, it } from 'vitest';
import {
  adversaryAttacks,
  adversaryPassed,
  contractStderrTail,
  lastAdversaryRound,
  outcomeSummary,
  roundOutcomeTone,
  selftestEvidence,
  type ControlEvidence,
  type RoundRecord,
} from './approvalEvidence';

const control = (cmd: string, mean: number): ControlEvidence => ({
  cmd,
  mean,
  all_valid: true,
  readings: [{ valid: true, score: mean, note: '' }],
});

const round = (n: number, kind: RoundRecord['kind'], outcome: RoundRecord['outcome']): RoundRecord => ({
  round: n,
  kind,
  judge_block: '[judge]\nmeasure_cmd = "./m.sh"',
  cost: 0,
  outcome,
});

const selftestFail: RoundRecord['outcome'] = {
  result: 'failed',
  evidence: {
    stage: 'selftest',
    direction: 'higher',
    runs: 3,
    good: control('stage-good', 10),
    bad: control('stage-bad', 100),
  },
};

const contractFail: RoundRecord['outcome'] = {
  result: 'failed',
  evidence: {
    stage: 'contract',
    findings: ['measure_cmd exited nonzero'],
    stderr_tail: ['line1', 'boom: not found'],
  },
};

const adversaryFail: RoundRecord['outcome'] = {
  result: 'failed',
  evidence: {
    stage: 'adversary',
    attacks: [
      { kind: 'uncounted-path', narrative: 'work moves to setup_cmd', suggestion: 'time setup_cmd too' },
    ],
  },
};

// The pre-adversary compat shape: only propose/refine kinds, only passed/failed outcomes — the
// trail a pack frozen before adversarial-verify rounds were added carries. The helpers must treat
// it as trivially adversary-free, not error on the missing round kind.
const legacyTrail: RoundRecord[] = [
  round(1, 'propose', contractFail),
  round(2, 'refine', { result: 'passed' }),
];

describe('roundOutcomeTone', () => {
  it('maps each outcome to its tone', () => {
    expect(roundOutcomeTone({ result: 'passed' })).toEqual({ label: 'passed', tone: 'green' });
    expect(roundOutcomeTone(selftestFail)).toEqual({ label: 'failed', tone: 'orange' });
    expect(roundOutcomeTone({ result: 'error', detail: 'no verdict' })).toEqual({
      label: 'error',
      tone: 'red',
    });
  });
});

describe('adversary verdict derivation', () => {
  it('passes when the last adversary round passed', () => {
    const rounds = [round(1, 'propose', { result: 'passed' }), round(2, 'adversary', { result: 'passed' })];
    expect(adversaryPassed(rounds)).toBe(true);
    expect(adversaryAttacks(rounds)).toEqual([]);
  });

  it('carries the attacks when the adversary found concerns', () => {
    const rounds = [round(1, 'propose', { result: 'passed' }), round(2, 'adversary', adversaryFail)];
    expect(adversaryPassed(rounds)).toBe(false);
    const attacks = adversaryAttacks(rounds);
    expect(attacks).toHaveLength(1);
    expect(attacks[0].kind).toBe('uncounted-path');
  });

  it('uses the LAST adversary round when a refine round follows the first review', () => {
    const rounds = [
      round(1, 'propose', { result: 'passed' }),
      round(2, 'adversary', adversaryFail),
      round(3, 'refine', { result: 'passed' }),
      round(4, 'adversary', { result: 'passed' }),
    ];
    expect(lastAdversaryRound(rounds)?.round).toBe(4);
    expect(adversaryPassed(rounds)).toBe(true);
    expect(adversaryAttacks(rounds)).toEqual([]);
  });

  it('is neither passed nor attacked on a pre-adversary compat trail', () => {
    expect(lastAdversaryRound(legacyTrail)).toBeNull();
    expect(adversaryPassed(legacyTrail)).toBe(false);
    expect(adversaryAttacks(legacyTrail)).toEqual([]);
  });

  it('an errored adversary round is not a pass and carries no attacks', () => {
    const rounds = [round(1, 'adversary', { result: 'error', detail: 'malformed verdict' })];
    expect(adversaryPassed(rounds)).toBe(false);
    expect(adversaryAttacks(rounds)).toEqual([]);
  });
});

describe('selftestEvidence', () => {
  it('extracts the controls from a selftest failure', () => {
    const ev = selftestEvidence(selftestFail);
    expect(ev).not.toBeNull();
    expect(ev?.direction).toBe('higher');
    expect(ev?.runs).toBe(3);
    expect(ev?.good.cmd).toBe('stage-good');
    expect(ev?.bad.mean).toBe(100);
  });

  it('is null for every other outcome shape', () => {
    expect(selftestEvidence({ result: 'passed' })).toBeNull();
    expect(selftestEvidence(contractFail)).toBeNull();
    expect(selftestEvidence(adversaryFail)).toBeNull();
    expect(selftestEvidence({ result: 'error', detail: 'x' })).toBeNull();
  });
});

describe('contractStderrTail', () => {
  it('extracts the stderr tail from a contract failure', () => {
    expect(contractStderrTail(contractFail)).toEqual(['line1', 'boom: not found']);
  });

  it('is null for every other outcome shape', () => {
    expect(contractStderrTail({ result: 'passed' })).toBeNull();
    expect(contractStderrTail(selftestFail)).toBeNull();
    expect(contractStderrTail({ result: 'error', detail: 'x' })).toBeNull();
  });
});

describe('outcomeSummary', () => {
  it('summarizes each outcome shape in one line', () => {
    expect(outcomeSummary({ result: 'passed' })).toBe('validation passed');
    expect(outcomeSummary({ result: 'error', detail: 'no verdict' })).toBe('turn error: no verdict');
    expect(outcomeSummary(contractFail)).toBe('contract — measure_cmd exited nonzero');
    expect(outcomeSummary(selftestFail)).toBe('selftest — higher wins, 3 run(s)');
    expect(outcomeSummary(adversaryFail)).toBe('adversary — 1 concern(s)');
    expect(
      outcomeSummary({ result: 'failed', evidence: { stage: 'structure', detail: 'no manifest' } }),
    ).toBe('structure — no manifest');
  });
});
