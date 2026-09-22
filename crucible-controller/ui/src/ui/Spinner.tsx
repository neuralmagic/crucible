import { cn } from './cn';
import styles from './ui.module.css';

export interface SpinnerProps {
  /** Uppercase mono caption beside the pulsing swatch. */
  label?: string;
  className?: string;
}

export function Spinner({ label = 'LOADING', className }: SpinnerProps) {
  return (
    <span
      role="status"
      className={cn(
        'inline-flex items-center gap-1.5 font-mono text-label font-semibold uppercase tracking-label text-ink-3',
        className,
      )}
    >
      <span className={cn('size-2 shrink-0 border border-black/30 bg-ink-3 dark:border-white/30', styles.pulse)} />
      {label}
    </span>
  );
}
