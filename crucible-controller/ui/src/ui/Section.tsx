import type { ReactNode } from 'react';
import { cn } from './cn';

export interface SectionProps {
  children: ReactNode;
  className?: string;
}

export function Section({ children, className }: SectionProps) {
  return <section className={cn('border-b border-rule-hard', className)}>{children}</section>;
}

export interface SectionHeaderProps {
  title: ReactNode;
  /** Dim, sentence-case aside after the title. */
  note?: ReactNode;
  /** Trailing slot, pushed to the right edge. */
  actions?: ReactNode;
  className?: string;
}

export function SectionHeader({ title, note, actions, className }: SectionHeaderProps) {
  return (
    <div
      className={cn(
        'flex items-center gap-2.5 border-b border-rule bg-sunk px-4.5 py-2 font-mono text-label font-semibold uppercase tracking-section text-ink-2',
        className,
      )}
    >
      {title}
      {note !== undefined && (
        <span className="font-normal tracking-action normal-case text-ink-3">{note}</span>
      )}
      {actions !== undefined && <span className="ml-auto flex items-center gap-2">{actions}</span>}
    </div>
  );
}

export interface SectionBodyProps {
  children: ReactNode;
  /** Off for flush content such as a table. */
  padded?: boolean;
  className?: string;
}

export function SectionBody({ children, padded = true, className }: SectionBodyProps) {
  return (
    <div className={cn('bg-surface', padded && 'px-4.5 py-3.5', className)}>{children}</div>
  );
}
