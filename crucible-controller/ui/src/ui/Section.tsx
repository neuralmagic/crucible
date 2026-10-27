import type { ReactNode } from 'react';
import { cn } from './cn';

export interface SectionProps {
  children: ReactNode;
  className?: string;
}

export function Section({ children, className }: SectionProps) {
  return (
    <section data-ui="section" className={cn('border-b border-rule-hard', className)}>
      {children}
    </section>
  );
}

export interface SectionHeaderProps {
  title: ReactNode;
  /** Dim, sentence-case aside after the title. */
  note?: ReactNode;
  /** Trailing slot, pushed to the right edge. */
  actions?: ReactNode;
  className?: string;
}

const HEADER =
  'flex items-center gap-2.5 border-b border-rule bg-sunk px-4.5 py-2 font-mono text-label font-semibold uppercase tracking-section text-ink-2';

function HeaderNote({ note }: { note: ReactNode }) {
  return <span className="font-normal tracking-action normal-case text-ink-3">{note}</span>;
}

export function SectionHeader({ title, note, actions, className }: SectionHeaderProps) {
  return (
    <div data-ui="section-header" className={cn(HEADER, className)}>
      {title}
      {note !== undefined && <HeaderNote note={note} />}
      {actions !== undefined && <span className="ml-auto flex items-center gap-2">{actions}</span>}
    </div>
  );
}

export interface FoldSectionProps {
  title: ReactNode;
  note?: ReactNode;
  children: ReactNode;
  className?: string;
}

/** A section that starts closed: its header is the toggle. */
export function FoldSection({ title, note, children, className }: FoldSectionProps) {
  return (
    <details data-ui="section" className={cn('group border-b border-rule-hard', className)}>
      <summary
        data-ui="section-header"
        className={cn(HEADER, 'cursor-pointer list-none select-none [&::-webkit-details-marker]:hidden')}
      >
        <span aria-hidden className="text-ink-3 transition-transform group-open:rotate-90">
          {'\u25b8'}
        </span>
        {title}
        {note !== undefined && <HeaderNote note={note} />}
      </summary>
      {children}
    </details>
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
    <div data-ui="section-body" className={cn('bg-surface', padded && 'px-4.5 py-3.5', className)}>
      {children}
    </div>
  );
}
