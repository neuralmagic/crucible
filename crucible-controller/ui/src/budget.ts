import type { components } from './api/schema';

type PlaybookRunDto = components['schemas']['PlaybookRunDto'];

/// Today's deployment-wide spend and the daily ceiling every launch is admitted against.
export interface SharedBudget {
  spent: number;
  /// Null when the deployment runs without a ceiling.
  ceiling: number | null | undefined;
}

export type BudgetState = 'uncapped' | 'ok' | 'near' | 'spent';

/// The share of the ceiling past which the budget reads as running low.
export const NEAR_SHARE = 0.8;

export function budgetState(budget: SharedBudget): BudgetState {
  const { spent, ceiling } = budget;
  if (ceiling === null || ceiling === undefined) return 'uncapped';
  if (spent >= ceiling) return 'spent';
  return spent >= ceiling * NEAR_SHARE ? 'near' : 'ok';
}

/// Whole percent of the ceiling used, or null when uncapped. A zero ceiling reads as full.
export function budgetPercent(budget: SharedBudget): number | null {
  const { spent, ceiling } = budget;
  if (ceiling === null || ceiling === undefined) return null;
  return ceiling === 0 ? 100 : Math.round((spent / ceiling) * 100);
}

export function usd(amount: number): string {
  return `$${amount.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

/// When the ledger day rolls over and a spent budget admits launches again: the next UTC midnight,
/// relative to `now`.
export function resetsIn(now: Date): string {
  const next = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + 1);
  const min = Math.max(1, Math.ceil((next - now.getTime()) / 60_000));
  if (min < 60) return `${min}m`;
  const hr = Math.floor(min / 60);
  const rest = min % 60;
  return rest === 0 ? `${hr}h` : `${hr}h ${rest}m`;
}

/// The launch form's line on the shared budget. Null when uncapped.
export function budgetNotice(budget: SharedBudget, now: Date): string | null {
  const state = budgetState(budget);
  if (state === 'uncapped') return null;
  if (state === 'spent') return `Spent. Launches resume in ${resetsIn(now)}.`;
  return `${usd(budget.spent)} of ${usd(budget.ceiling ?? 0)} used today.`;
}

/// Cost of the runs `login` launched on `now`'s UTC day. A run's cost counts on the day it was
/// launched, so a run that crosses midnight lands on the earlier day.
export function yourSpendToday(
  runs: readonly Pick<PlaybookRunDto, 'created_by' | 'created_at' | 'cost_usd'>[],
  login: string,
  now: Date,
): number {
  const me = login.trim().toLowerCase();
  const today = now.toISOString().slice(0, 10);
  return runs
    .filter((run) => (run.created_by ?? '').toLowerCase() === me && run.created_at.slice(0, 10) === today)
    .reduce((total, run) => total + (run.cost_usd ?? 0), 0);
}
