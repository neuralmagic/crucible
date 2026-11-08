import { describe, expect, it } from 'vitest';
import { scheduleView, type ScheduleRow } from './schedulesView';

function row(over: Partial<ScheduleRow> = {}): ScheduleRow {
  return {
    enabled: true,
    next_due_at: '2026-08-25T06:00:00Z',
    consecutive_failures: 0,
    ...over,
  };
}

describe('scheduleView', () => {
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
    expect(view.headline).toBe('DISABLED');
    expect(view.detail).toBe('Disabled. It fires again once it is enabled.');
  });

  it('shows the next firing when nothing is wrong', () => {
    const view = scheduleView(row());
    expect(view.state).toBe('live');
    expect(view.headline).toBe('ENABLED');
    expect(view.detail).toContain('2026-08-25T06:00:00Z');
  });

  it('says so when an enabled schedule has no occurrence ahead', () => {
    expect(scheduleView(row({ next_due_at: null })).detail).toBe('No further occurrence ahead.');
  });
});
