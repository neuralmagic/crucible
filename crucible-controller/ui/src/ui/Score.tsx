import type { ReactNode } from 'react';
import { cn } from './cn';

export interface ScoreProps {
  value: ReactNode;
  /** Unit suffix, rendered small and dim next to the number. */
  unit?: string;
  /** Paints the value green and appends the BEST tag. */
  best?: boolean;
  className?: string;
}

export function Score({ value, unit, best = false, className }: ScoreProps) {
  return (
    <span className={cn('font-mono text-data-lg font-semibold', best ? 'text-green' : 'text-ink', className)}>
      {value}
      {unit !== undefined && <span className="text-label font-normal text-ink-3"> {unit}</span>}
      {best && (
        <span className="ml-1.5 bg-green px-1 py-px font-mono text-micro font-bold tracking-group text-surface">
          BEST
        </span>
      )}
    </span>
  );
}
