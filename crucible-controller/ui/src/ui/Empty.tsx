import type { ReactNode } from 'react';
import { cn } from './cn';

export interface EmptyProps {
  /** Uppercase mono headline, e.g. "NO ISSUES". */
  title: string;
  description?: ReactNode;
  action?: ReactNode;
  className?: string;
}

export function Empty({ title, description, action, className }: EmptyProps) {
  return (
    <div
      className={cn(
        'flex flex-col items-center gap-2 border-b border-rule bg-surface px-4.5 py-12 text-center',
        className,
      )}
    >
      <span className="font-mono text-label font-semibold uppercase tracking-section text-ink-3">
        {title}
      </span>
      {description !== undefined && <p className="m-0 max-w-[52ch] text-ink-2">{description}</p>}
      {action !== undefined && <div className="mt-1 flex gap-2">{action}</div>}
    </div>
  );
}
