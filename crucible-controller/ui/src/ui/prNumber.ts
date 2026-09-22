/** The PR number out of a GitHub pull-request url (`…/pull/123`, with or without trailing
 * segments/anchors), or null when the url doesn't carry one — the chip then falls back to a
 * numberless "PR" label rather than rendering garbage. */
export function parsePrNumber(url: string): number | null {
  const m = /\/pull\/(\d+)(?:[/?#]|$)/.exec(url);
  if (!m) return null;
  const n = Number(m[1]);
  return Number.isSafeInteger(n) ? n : null;
}
