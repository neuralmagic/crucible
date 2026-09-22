// Pure parsing/presentation helpers for the approvals "Measurement evidence" panel, kept side-effect-free
// so approvalEvidence.test.ts can exercise them without any DOM.
import type { components } from '../api/schema';

export type RoundRecord = components['schemas']['RoundRecord'];
export type RoundOutcome = components['schemas']['RoundOutcome'];
export type FailureEvidence = components['schemas']['FailureEvidence'];
export type Attack = components['schemas']['Attack'];
export type ControlEvidence = components['schemas']['ControlEvidence'];

export type Tone = 'green' | 'red' | 'orange' | 'grey' | 'blue';

/** The tone + label a round's outcome should render with. */
export function roundOutcomeTone(outcome: RoundOutcome): { label: string; tone: Tone } {
  switch (outcome.result) {
    case 'passed':
      return { label: 'passed', tone: 'green' };
    case 'failed':
      return { label: 'failed', tone: 'orange' };
    case 'error':
      return { label: 'error', tone: 'red' };
  }
}

/** The last `Adversary`-kind round in the trail, if any — the gaming-review verdict. */
export function lastAdversaryRound(rounds: RoundRecord[]): RoundRecord | null {
  for (let i = rounds.length - 1; i >= 0; i--) {
    if (rounds[i].kind === 'adversary') return rounds[i];
  }
  return null;
}

/** Whether the trail's adversary round (if any) passed clean — the "red-team passed" banner. */
export function adversaryPassed(rounds: RoundRecord[]): boolean {
  const round = lastAdversaryRound(rounds);
  return round !== null && round.outcome.result === 'passed';
}

/** The attacks an adversary round found, or `[]` if it passed clean / there was no adversary round. */
export function adversaryAttacks(rounds: RoundRecord[]): Attack[] {
  const round = lastAdversaryRound(rounds);
  if (!round || round.outcome.result !== 'failed') return [];
  const evidence = round.outcome.evidence;
  return evidence.stage === 'adversary' ? evidence.attacks : [];
}

/** The contract stderr tail for a `Failed`/`Contract` round, or `null` for any other shape. */
export function contractStderrTail(outcome: RoundOutcome): string[] | null {
  if (outcome.result !== 'failed') return null;
  return outcome.evidence.stage === 'contract' ? outcome.evidence.stderr_tail : null;
}

/** The self-test evidence for a `Failed`/`Selftest` round, or `null` for any other shape. */
export function selftestEvidence(
  outcome: RoundOutcome,
): { direction: string; runs: number; good: ControlEvidence; bad: ControlEvidence } | null {
  if (outcome.result !== 'failed') return null;
  return outcome.evidence.stage === 'selftest' ? outcome.evidence : null;
}

/** A one-line human summary of a round's outcome, for a collapsed row. */
export function outcomeSummary(outcome: RoundOutcome): string {
  switch (outcome.result) {
    case 'passed':
      return 'validation passed';
    case 'error':
      return `turn error: ${outcome.detail}`;
    case 'failed':
      switch (outcome.evidence.stage) {
        case 'structure':
          return `structure — ${outcome.evidence.detail}`;
        case 'contract':
          return `contract — ${outcome.evidence.findings.join('; ')}`;
        case 'selftest':
          return `selftest — ${outcome.evidence.direction} wins, ${outcome.evidence.runs} run(s)`;
        case 'adversary':
          return `adversary — ${outcome.evidence.attacks.length} concern(s)`;
      }
  }
}
