import type { ReactNode } from 'react';
import { cn } from './cn';

export interface DetailHeaderProps {
  /** The record's identifier, set in mono at display size. */
  title: ReactNode;
  /** Inline slot beside the title, typically a `<Status />`. */
  badge?: ReactNode;
  /** Prose subtitle: the issue summary, the scenario goal. */
  description?: ReactNode;
  /** Mono facts row under the title: identifiers, repo, timestamps. */
  meta?: ReactNode;
  /** Right-hand slot, typically a `<Spec />` strip. Stacks below the title under 1100px. */
  aside?: ReactNode;
  className?: string;
}

export function DetailHeader({
  title,
  badge,
  description,
  meta,
  aside,
  className,
}: DetailHeaderProps) {
  return (
    <div
      className={cn(
        'flex items-start gap-6 border-b border-rule-hard bg-surface px-4.5 py-4 max-wide:flex-col',
        className,
      )}
    >
      <div className="min-w-0 flex-1">
        <h1 className="m-0 mr-2.5 inline-block font-mono text-display font-bold tracking-display break-all">
          {title}
        </h1>
        {badge}
        {description !== undefined && (
          <p className="mt-1 mb-0 max-w-[70ch] text-lede font-medium">{description}</p>
        )}
        {meta !== undefined && (
          <div className="mt-1.5 flex flex-wrap items-center gap-3.5 font-mono text-data text-ink-3">
            {meta}
          </div>
        )}
      </div>
      {aside}
    </div>
  );
}
