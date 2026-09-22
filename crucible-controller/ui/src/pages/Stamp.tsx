import { Mono } from '../ui';
import { absoluteTime, relativeTime } from './journeyView';

/// Relative upstream-activity stamp with the full timestamp on hover.
export function Stamp({ iso }: { iso: string | null | undefined }) {
  const rel = relativeTime(iso);
  if (!rel) return <Mono tone="ink-3">—</Mono>;
  return <Mono title={absoluteTime(iso) ?? undefined}>{rel}</Mono>;
}
