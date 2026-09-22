import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  refreshFailure,
  scheduleView,
  signInHref,
  signinNeededCount,
  type ScheduleRow,
} from './schedulesView';

function row(over: Partial<ScheduleRow> = {}): ScheduleRow {
  return {
    enabled: true,
    next_due_at: '2026-08-25T06:00:00Z',
    consecutive_failures: 0,
    owner_signin_required: false,
    owner_refresh_error: null,
    owner_refresh_at: null,
    ...over,
  };
}

describe('refreshFailure', () => {
  afterEach(() => vi.useRealTimers());

  it('is null while the last refresh landed', () => {
    expect(refreshFailure(row())).toBeNull();
    expect(refreshFailure(row({ owner_refresh_at: '2026-08-24T11:00:00Z' }))).toBeNull();
  });

  it('carries the refusal and when it was tried', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-08-24T12:00:00Z'));
    const failure = refreshFailure(
      row({
        owner_refresh_error: 'the refresh was refused: invalid_grant',
        owner_refresh_at: '2026-08-24T11:30:00Z',
      }),
    );
    expect(failure).toBe('the refresh was refused: invalid_grant (30m ago)');
  });

  it('keeps the refusal when there is no usable stamp', () => {
    expect(refreshFailure(row({ owner_refresh_error: 'issuer unreachable' }))).toBe(
      'issuer unreachable',
    );
    expect(
      refreshFailure(row({ owner_refresh_error: 'issuer unreachable', owner_refresh_at: 'nope' })),
    ).toBe('issuer unreachable');
  });
});

describe('scheduleView', () => {
  it('offers sign-in, quoting the refusal, when the owner has to sign in again', () => {
    const view = scheduleView(
      row({ owner_signin_required: true, owner_refresh_error: 'the refresh was refused' }),
    );
    expect(view.state).toBe('signin');
    expect(view.headline).toBe('SIGN-IN NEEDED');
    expect(view.detail).toContain('the refresh was refused');
    expect(view.signInHref).toBe('/auth/login?rd=%2Fschedules');
  });

  it('still offers sign-in when the sweep recorded no refusal, e.g. after a revoke', () => {
    const view = scheduleView(row({ owner_signin_required: true }));
    expect(view.state).toBe('signin');
    expect(view.detail).toMatch(/offline credential/);
  });

  it('outranks disabled: signing in is the move, and the row still says it is off', () => {
    const view = scheduleView(row({ owner_signin_required: true, enabled: false }));
    expect(view.state).toBe('signin');
    expect(view.detail).toContain('; it is disabled as well.');
  });

  it('counts the firings that auto-disabled a schedule, and pluralizes them', () => {
    expect(scheduleView(row({ enabled: false, consecutive_failures: 1 })).detail).toContain(
      '1 firing in a row',
    );
    expect(scheduleView(row({ enabled: false, consecutive_failures: 3 })).detail).toContain(
      '3 firings in a row',
    );
  });

  it('says a hand-disabled schedule is simply off', () => {
    const view = scheduleView(row({ enabled: false }));
    expect(view.state).toBe('disabled');
    expect(view.detail).toBe('Disabled. It fires again once it is enabled.');
    expect(view.signInHref).toBeNull();
  });

  it('reports a transient refresh failure without claiming sign-in is needed', () => {
    const view = scheduleView(row({ owner_refresh_error: 'issuer unreachable' }));
    expect(view.state).toBe('failing');
    expect(view.headline).toBe('REFRESH FAILING');
    expect(view.detail).toBe('issuer unreachable; the next sweep tries again.');
    expect(view.signInHref).toBeNull();
  });

  it('shows the next firing when nothing is wrong', () => {
    const view = scheduleView(row());
    expect(view.state).toBe('live');
    expect(view.detail).toContain('2026-08-25T06:00:00Z');
  });

  it('says so when an enabled schedule has no occurrence ahead', () => {
    expect(scheduleView(row({ next_due_at: null })).detail).toBe('No further occurrence ahead.');
  });
});

describe('signInHref', () => {
  it('returns to the schedules view after sign-in', () => {
    expect(signInHref()).toBe('/auth/login?rd=%2Fschedules');
  });
});

describe('signinNeededCount', () => {
  it('counts only the schedules waiting on a sign-in', () => {
    expect(
      signinNeededCount([row(), row({ owner_signin_required: true }), row({ enabled: false })]),
    ).toBe(1);
    expect(signinNeededCount([])).toBe(0);
  });
});
