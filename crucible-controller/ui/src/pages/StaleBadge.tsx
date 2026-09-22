import { Mono, Tooltip } from '../ui';
import type { components } from '../api/schema.d';

type LongText = components['schemas']['LongText'];

/** Compact badge for a parked issue the grounded ranker found already implemented in the
 * checkout — the signal that a human should go close it upstream. Renders nothing when the
 * server-computed `stale_closable` flag is unset, so callers can drop it in next to the status
 * label unconditionally. Tooltip carries the `parked_reason`; list responses truncate it, so the
 * tooltip says where the rest lives rather than pretending the preview is the whole reason. */
export function StaleBadge({
  staleClosable,
  parkedReason,
}: {
  staleClosable: boolean;
  parkedReason: LongText | null | undefined;
}) {
  if (!staleClosable) return null;
  const content = parkedReason?.truncated
    ? `${parkedReason.text} (open the issue for the full reason)`
    : parkedReason?.text;
  return (
    <Tooltip content={content}>
      <Mono size="micro" weight="bold" uppercase tone="amber" className="border border-amber px-1">
        stale
      </Mono>
    </Tooltip>
  );
}
