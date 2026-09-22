import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { cn } from './cn';

export interface BreadcrumbItem {
  label: ReactNode;
  /** Omit on the final item: the current page renders as bold ink, not a link. */
  to?: string;
}

export interface BreadcrumbProps {
  items: readonly BreadcrumbItem[];
  className?: string;
}

export function Breadcrumb({ items, className }: BreadcrumbProps) {
  return (
    <nav
      aria-label="Breadcrumb"
      className={cn(
        'border-b border-rule bg-surface px-4.5 py-2 font-mono text-data text-ink-3',
        className,
      )}
    >
      {items.map((item, index) => (
        <span key={index}>
          {index > 0 && <span className="px-1">/</span>}
          {item.to === undefined ? (
            <b className="font-semibold text-ink">{item.label}</b>
          ) : (
            <Link to={item.to} className="hover:text-ink">
              {item.label}
            </Link>
          )}
        </span>
      ))}
    </nav>
  );
}
