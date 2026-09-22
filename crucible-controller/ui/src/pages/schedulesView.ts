import type { components } from '../api/schema.d';
import { relativeTime } from './journeyView';

export type ScheduleDto = components['schemas']['ScheduleDto'];

export type ScheduleState = 'signin' | 'disabled' | 'failing' | 'live';

export interface ScheduleView {
  state: ScheduleState;
  headline: string;
  /// What the row says under the headline: why it will not fire, or when it fires next.
  detail: string;
  /// Where the sign-in control sends the browser, null unless this schedule needs one.
  signInHref: string | null;
}

/// The fields the schedules view reads. Anything that carries them renders, so a test row does not
/// have to spell out the whole DTO.
export type ScheduleRow = Pick<
  ScheduleDto,
  | 'enabled'
  | 'next_due_at'
  | 'consecutive_failures'
  | 'owner_signin_required'
  | 'owner_refresh_error'
  | 'owner_refresh_at'
>;

/// The sign-in URL, landing the browser back on the schedules view. `/auth/login` answers in both
/// auth modes, same as `redirectToSignIn`.
export function signInHref(): string {
  return `/auth/login?rd=${encodeURIComponent('/schedules')}`;
}

/// The last fire-time group refresh that did not land: the refusal the sweep recorded, and when.
export function refreshFailure(row: ScheduleRow): string | null {
  if (!row.owner_refresh_error) return null;
  const when = relativeTime(row.owner_refresh_at);
  return when ? `${row.owner_refresh_error} (${when})` : row.owner_refresh_error;
}

/// What a row says about itself. `signin` outranks the rest: a schedule whose owner has to sign in
/// again fires nothing until they do, disabled or not.
export function scheduleView(row: ScheduleRow): ScheduleView {
  const failure = refreshFailure(row);
  if (row.owner_signin_required) {
    const why =
      failure ??
      'The owner has no usable offline credential, so team membership cannot be re-checked.';
    return {
      state: 'signin',
      headline: 'SIGN-IN NEEDED',
      detail: row.enabled ? why : `${why}; it is disabled as well.`,
      signInHref: signInHref(),
    };
  }
  if (!row.enabled) {
    const failures = row.consecutive_failures;
    return {
      state: 'disabled',
      headline: 'DISABLED',
      detail:
        failures > 0
          ? `Disabled after ${failures} firing${failures === 1 ? '' : 's'} in a row that did not launch. Save it again to start it.`
          : 'Disabled. It fires again once it is enabled.',
      signInHref: null,
    };
  }
  if (failure !== null) {
    return {
      state: 'failing',
      headline: 'REFRESH FAILING',
      detail: `${failure}; the next sweep tries again.`,
      signInHref: null,
    };
  }
  return {
    state: 'live',
    headline: 'ENABLED',
    detail: row.next_due_at ? `Next firing ${row.next_due_at}.` : 'No further occurrence ahead.',
    signInHref: null,
  };
}

/// The footer count: how many of these schedules are waiting on their owner to sign in.
export function signinNeededCount(rows: ScheduleRow[]): number {
  return rows.filter((row) => row.owner_signin_required).length;
}
