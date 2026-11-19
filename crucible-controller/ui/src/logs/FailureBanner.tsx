import { Mono } from '../ui';

/// The failure a pack declared, and what emitted it.
export interface FailureLine {
  line: string;
  source?: string | null;
  task?: string | null;
}

/// `task · source`, empty when neither is known.
export function failureAttribution(failure: FailureLine): string {
  return [failure.task ?? null, failure.source ?? null]
    .filter((part): part is string => part !== null)
    .join(' · ');
}

/// The failure a pack declared for the attempt, above its log, so the line that stopped it is
/// readable before the reader scrolls.
export function FailureBanner({
  failure,
  label = 'declared failure',
}: {
  failure: FailureLine;
  label?: string;
}) {
  const where = failureAttribution(failure);
  return (
    <div data-testid="failure-line" className="border-b border-red/40 bg-red/5 px-2 py-1.5">
      <Mono size="micro" tone="ink-3" uppercase className="tracking-section">
        {where === '' ? label : `${label} · ${where}`}
      </Mono>
      <p className="mt-1 mb-0 font-mono text-data whitespace-pre-wrap text-red">{failure.line}</p>
    </div>
  );
}
