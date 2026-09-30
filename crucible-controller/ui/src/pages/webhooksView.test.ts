import { afterEach, describe, expect, it, vi } from 'vitest';
import { bodyText, outcomeTone, webhookView, type WebhookRow } from './webhooksView';

function row(over: Partial<WebhookRow> = {}): WebhookRow {
  return {
    enabled: true,
    consecutive_failures: 0,
    owner_signin_required: false,
    owner_refresh_error: null,
    owner_refresh_at: null,
    last_delivery_at: null,
    ...over,
  };
}

describe('webhookView', () => {
  afterEach(() => vi.useRealTimers());

  it('says when it last took a delivery', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-09-29T12:10:00Z'));
    expect(webhookView(row())).toMatchObject({ state: 'live', detail: 'No delivery yet.' });
    expect(webhookView(row({ last_delivery_at: '2026-09-29T12:00:00Z' }))).toMatchObject({
      state: 'live',
      detail: 'Last delivery 10m ago.',
    });
  });

  it('puts a needed sign-in ahead of being disabled', () => {
    expect(webhookView(row({ owner_signin_required: true, enabled: false })).state).toBe('signin');
  });

  it('says why it is disabled and what its address does meanwhile', () => {
    const view = webhookView(row({ enabled: false, consecutive_failures: 3 }));
    expect(view.state).toBe('disabled');
    expect(view.detail).toContain('3 firings in a row');
    expect(view.detail).toContain('not found');
  });

  it('surfaces a failing owner refresh', () => {
    const view = webhookView(row({ owner_refresh_error: 'issuer said no', owner_refresh_at: null }));
    expect(view).toMatchObject({ state: 'failing', headline: 'REFRESH FAILING' });
    expect(view.detail).toContain('issuer said no');
  });
});

describe('outcomeTone', () => {
  it('paints each outcome, and an unknown one grey', () => {
    expect(outcomeTone('launched')).toBe('green');
    expect(outcomeTone('pending')).toBe('blue');
    expect(outcomeTone('throttled')).toBe('amber');
    expect(outcomeTone('failed')).toBe('red');
    expect(outcomeTone('filtered')).toBe('grey');
    expect(outcomeTone('duplicate')).toBe('grey');
    expect(outcomeTone('something-new')).toBe('grey');
  });
});

describe('bodyText', () => {
  it('pretty-prints JSON, keeps other text, and labels bytes that are not UTF-8', () => {
    expect(bodyText({ body: '{"a":1}', body_base64: null })).toBe('{\n  "a": 1\n}');
    expect(bodyText({ body: 'plain', body_base64: null })).toBe('plain');
    expect(bodyText({ body: null, body_base64: '/w==' })).toBe('(not UTF-8; base64) /w==');
  });
});
