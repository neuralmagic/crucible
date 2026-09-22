import type { ReactNode } from 'react';
import { cn } from './cn';

export interface PageHeaderProps {
  /** Uppercase mono kicker above the title, e.g. "Queue". */
  eyebrow?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  /** Trailing slot, pushed to the right edge and top-aligned. */
  actions?: ReactNode;
  className?: string;
}

export function PageHeader({ eyebrow, title, description, actions, className }: PageHeaderProps) {
  return (
    <div
      className={cn('flex items-start gap-6 border-b border-rule px-4.5 pt-4 pb-3', className)}
    >
      <div className="min-w-0 flex-1">
        {eyebrow !== undefined && (
          <div className="font-mono text-label uppercase tracking-eyebrow text-ink-3">{eyebrow}</div>
        )}
        <h1 className="mt-0.5 mb-0 text-title font-bold tracking-title">{title}</h1>
        {description !== undefined && (
          <p className="mt-[5px] mb-0 max-w-[66ch] text-ink-2">{description}</p>
        )}
      </div>
      {actions !== undefined && <div className="flex flex-none items-center gap-2">{actions}</div>}
    </div>
  );
}
