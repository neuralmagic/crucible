// Pure presentation helpers for the span-enriched flow fetch, kept side-effect-free for
// flowEnriched.test.ts.

/** Human message for a failed enrichment, keyed on the endpoint's status contract. */
export function flowEnrichedMessage(status: number, error: string | null): string {
  switch (status) {
    case 501:
      return 'engine too old for flow reports';
    case 503:
      return 'Datadog keys not configured';
    case 502:
      return 'span fetch failed';
    case 400:
      return error ?? 'trace id must be a plain alphanumeric token';
    case 404:
      return error ?? 'run has no session evidence';
  }
  return error ?? `enrichment failed (${status})`;
}

/** Client-side mirror of the endpoint's trace-id rule, so an obviously bad id never leaves an
 * in-flight request behind. */
export function validTraceId(traceId: string): boolean {
  return traceId.length > 0 && traceId.length <= 64 && /^[A-Za-z0-9]+$/.test(traceId);
}
