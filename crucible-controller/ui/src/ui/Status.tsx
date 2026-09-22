import { cn } from './cn';
import styles from './ui.module.css';

export type StatusTone = 'green' | 'amber' | 'blue' | 'red' | 'grey';

const TEXT: Record<StatusTone, string> = {
  green: 'text-green',
  amber: 'text-amber',
  blue: 'text-blue',
  red: 'text-red',
  grey: 'text-ink-3',
};

const SWATCH: Record<StatusTone, string> = {
  green: 'bg-green',
  amber: 'bg-amber',
  blue: 'bg-blue',
  red: 'bg-red',
  grey: 'bg-ink-3',
};

const LIVE_STATUSES: ReadonlySet<string> = new Set(['running', 'building']);

/** True for the statuses whose swatch pulses: work is in flight right now. */
export function isLiveStatus(status: string): boolean {
  return LIVE_STATUSES.has(status);
}

/** Map a wire label color (`issueStatusColor`, run colors) onto a swatch tone. */
export function statusTone(color: string): StatusTone {
  switch (color) {
    case 'green':
      return 'green';
    case 'orange':
    case 'amber':
    case 'yellow':
      return 'amber';
    case 'blue':
    case 'cyan':
      return 'blue';
    case 'red':
      return 'red';
    default:
      return 'grey';
  }
}

export interface StatusProps {
  /** The wire status string, rendered uppercase as-is. */
  status: string;
  tone: StatusTone;
  /** Defaults to `isLiveStatus(status)`. */
  pulse?: boolean;
  className?: string;
}

export function Status({ status, tone, pulse, className }: StatusProps) {
  const live = pulse ?? isLiveStatus(status);
  return (
    <span
      className={cn(
        'inline-flex items-center gap-1.5 font-mono text-label font-semibold uppercase tracking-label',
        TEXT[tone],
        className,
      )}
    >
      <span
        className={cn(
          'size-2 shrink-0 border border-black/30 dark:border-white/30',
          SWATCH[tone],
          live && styles.pulse,
        )}
      />
      {status}
    </span>
  );
}
