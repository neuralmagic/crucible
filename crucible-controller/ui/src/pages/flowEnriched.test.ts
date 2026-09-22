import { describe, expect, it } from 'vitest';
import { flowEnrichedMessage, validTraceId } from './flowEnriched';

describe('flowEnrichedMessage', () => {
  it('maps the contract statuses to fixed human messages', () => {
    expect(flowEnrichedMessage(501, null)).toBe('engine too old for flow reports');
    expect(flowEnrichedMessage(503, 'DD_API_KEY is not configured on this deploy')).toBe(
      'Datadog keys not configured',
    );
    expect(flowEnrichedMessage(502, 'flow render failed')).toBe('span fetch failed');
  });

  it('prefers the server error body for 400/404', () => {
    expect(flowEnrichedMessage(400, 'trace_id must be a plain alphanumeric token')).toBe(
      'trace_id must be a plain alphanumeric token',
    );
    expect(flowEnrichedMessage(404, 'run not found: run-1')).toBe('run not found: run-1');
    expect(flowEnrichedMessage(400, null)).toBe('trace id must be a plain alphanumeric token');
    expect(flowEnrichedMessage(404, null)).toBe('run has no session evidence');
  });

  it('falls back to a status-tagged message for anything else', () => {
    expect(flowEnrichedMessage(500, null)).toBe('enrichment failed (500)');
    expect(flowEnrichedMessage(500, 'boom')).toBe('boom');
  });
});

describe('validTraceId', () => {
  it('accepts plain alphanumeric tokens', () => {
    expect(validTraceId('deadbeef123')).toBe(true);
    expect(validTraceId('0')).toBe(true);
  });

  it('rejects empty, oversized, and non-alphanumeric ids', () => {
    expect(validTraceId('')).toBe(false);
    expect(validTraceId('x'.repeat(65))).toBe(false);
    for (const bad of ['abc def', 'abc/def', 'abc-def', 'trace:1', '../x']) {
      expect(validTraceId(bad)).toBe(false);
    }
  });
});
