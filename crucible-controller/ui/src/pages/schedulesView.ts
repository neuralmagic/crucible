import type { components } from '../api/schema.d';

export type ScheduleDto = components['schemas']['ScheduleDto'];

export type ScheduleState = 'disabled' | 'live';

export interface ScheduleView {
  state: ScheduleState;
  headline: string;
  /// What the row says under the headline: why it will not fire, or when it fires next.
  detail: string;
}

/// The fields the schedules view reads. Anything that carries them renders, so a test row does not
/// have to spell out the whole DTO.
export type ScheduleRow = Pick<ScheduleDto, 'enabled' | 'next_due_at' | 'consecutive_failures'>;

/// What a row says about itself.
export function scheduleView(row: ScheduleRow): ScheduleView {
  if (!row.enabled) {
    const failures = row.consecutive_failures;
    return {
      state: 'disabled',
      headline: 'DISABLED',
      detail:
        failures > 0
          ? `Disabled after ${failures} firing${failures === 1 ? '' : 's'} in a row that did not launch. Save it again to start it.`
          : 'Disabled. It fires again once it is enabled.',
    };
  }
  return {
    state: 'live',
    headline: 'ENABLED',
    detail: row.next_due_at ? `Next firing ${row.next_due_at}.` : 'No further occurrence ahead.',
  };
}
