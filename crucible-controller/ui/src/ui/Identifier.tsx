import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { cn } from './cn';

export type IdentifierVariant = 'cell' | 'inline';

const BASE = 'inline-block border border-rule-hard font-mono text-data font-semibold tracking-data';

const VARIANT: Record<IdentifierVariant, string> = {
  cell: 'bg-paper px-1.5 py-0.5 text-ink group-hover/row:border-ink group-hover/row:bg-surface',
  inline: 'px-1.5 py-px text-ink-2 hover:border-ink hover:text-ink',
};

interface IdentifierBase {
  children: ReactNode;
  variant?: IdentifierVariant;
  title?: string;
  className?: string;
}

export type IdentifierProps = IdentifierBase &
  ({ to: string; href?: never } | { href: string; to?: never } | { to?: never; href?: never });

export function Identifier({
  children,
  variant = 'cell',
  title,
  className,
  ...target
}: IdentifierProps) {
  const cls = cn(BASE, VARIANT[variant], className);
  if (target.to !== undefined) {
    return (
      <Link to={target.to} title={title} className={cls}>
        {children}
      </Link>
    );
  }
  if (target.href !== undefined) {
    return (
      <a href={target.href} title={title} target="_blank" rel="noreferrer" className={cls}>
        {children}
      </a>
    );
  }
  return (
    <span title={title} className={cls}>
      {children}
    </span>
  );
}
