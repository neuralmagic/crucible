import type { components } from '../api/schema.d';
import type { StatusTone } from '../ui';
import { relativeTime } from './journeyView';
import { refreshFailure } from './schedulesView';

export type WebhookDto = components['schemas']['WebhookDto'];
export type WebhookDeliveryDto = components['schemas']['WebhookDeliveryDto'];

export type WebhookState = 'signin' | 'paused' | 'failing' | 'live';

export interface WebhookView {
  state: WebhookState;
  headline: string;
  detail: string;
}

/// The fields the webhook view reads, so a test row does not spell out the whole DTO.
export type WebhookRow = Pick<
  WebhookDto,
  | 'enabled'
  | 'consecutive_failures'
  | 'owner_signin_required'
  | 'owner_refresh_error'
  | 'owner_refresh_at'
  | 'last_delivery_at'
>;

/// What a row says about itself. `signin` outranks the rest: a webhook whose owner has to sign in
/// again launches nothing until they do.
export function webhookView(row: WebhookRow): WebhookView {
  const failure = refreshFailure(row);
  if (row.owner_signin_required) {
    return {
      state: 'signin',
      headline: 'SIGN-IN NEEDED',
      detail:
        failure ??
        'The owner has no usable offline credential, so team membership cannot be re-checked.',
    };
  }
  if (!row.enabled) {
    const failures = row.consecutive_failures;
    return {
      state: 'paused',
      headline: 'PAUSED',
      detail:
        failures > 0
          ? `Paused after ${failures} launch${failures === 1 ? '' : 'es'} in a row failed. Its address answers not found until it is resumed.`
          : 'Paused. Its address answers not found until it is resumed.',
    };
  }
  if (failure !== null) {
    return { state: 'failing', headline: 'REFRESH FAILING', detail: `${failure}; it is tried again shortly.` };
  }
  const last = relativeTime(row.last_delivery_at);
  return {
    state: 'live',
    headline: 'ENABLED',
    detail: last ? `Last delivery ${last}.` : 'No delivery yet.',
  };
}

export const STATE_TONE: Record<WebhookState, StatusTone> = {
  signin: 'red',
  paused: 'grey',
  failing: 'amber',
  live: 'green',
};

const OUTCOME_TONE: Record<string, StatusTone> = {
  queued: 'blue',
  launched: 'green',
  filtered: 'grey',
  duplicate: 'grey',
  throttled: 'amber',
  failed: 'red',
};

/// How a delivery outcome is painted. An outcome this build does not know is grey.
export function outcomeTone(outcome: string): StatusTone {
  return OUTCOME_TONE[outcome] ?? 'grey';
}

/// A delivery's body as the log shows it: pretty JSON when it parses, the text otherwise, and a
/// note when it was not UTF-8.
export function bodyText(delivery: Pick<WebhookDeliveryDto, 'body' | 'body_base64'>): string {
  if (delivery.body === null || delivery.body === undefined) {
    return delivery.body_base64 ? `(not UTF-8; base64) ${delivery.body_base64}` : '';
  }
  try {
    return JSON.stringify(JSON.parse(delivery.body), null, 2);
  } catch {
    return delivery.body;
  }
}
