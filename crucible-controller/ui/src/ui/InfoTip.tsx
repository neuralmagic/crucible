import type { ReactNode } from 'react';
import { Tooltip } from './Tooltip';

export interface InfoTipProps {
  /** What the icon explains, for screen readers: "About the dedupe key". */
  label: string;
  children: ReactNode;
}

/// An ⓘ that shows its text on hover or focus, for the one fact a field cannot go without.
export function InfoTip({ label, children }: InfoTipProps) {
  return (
    <Tooltip content={children} delay={100}>
      <button
        type="button"
        aria-label={label}
        className="cursor-help px-1 font-mono text-data text-ink-3 hover:text-ink focus-visible:text-ink"
      >
        ⓘ
      </button>
    </Tooltip>
  );
}
