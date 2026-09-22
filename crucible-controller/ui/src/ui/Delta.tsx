import { cn } from './cn';

export interface DeltaProps {
  /** Signed percentage change, e.g. -34.6. Null renders as a flat dash. */
  percent: number | null;
  /** Which direction counts as an improvement. */
  betterWhen?: 'lower' | 'higher';
  digits?: number;
  className?: string;
}

export function Delta({ percent, betterWhen = 'lower', digits = 1, className }: DeltaProps) {
  const base = 'font-mono text-data font-semibold tabular-nums';
  if (percent === null || Number.isNaN(percent)) {
    return <span className={cn(base, 'text-ink-3', className)}>—</span>;
  }
  const improved = betterWhen === 'lower' ? percent < 0 : percent > 0;
  const tone = percent === 0 ? 'text-ink-3' : improved ? 'text-green' : 'text-red';
  const sign = percent > 0 ? '+' : '';
  return (
    <span className={cn(base, tone, className)}>
      {sign}
      {percent.toFixed(digits)}%
    </span>
  );
}
