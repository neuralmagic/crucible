import type { ReactNode } from 'react';
import { cn } from './cn';

export interface SpecItem {
  label: string;
  value: ReactNode;
  /** Small dim suffix beside the value: a unit, or a denominator. */
  note?: ReactNode;
  tone?: 'default' | 'win' | 'bad';
}

export interface SpecProps {
  items: readonly SpecItem[];
  className?: string;
}

const TONE: Record<NonNullable<SpecItem['tone']>, string> = {
  default: 'text-ink',
  win: 'text-green',
  bad: 'text-red',
};

export function Spec({ items, className }: SpecProps) {
  return (
    <dl className={cn('m-0 flex flex-none border border-rule-hard', className)}>
      {items.map((item) => (
        <div key={item.label} className="border-r border-rule px-3.5 py-1.5 text-right last:border-r-0">
          <dt className="font-mono text-micro uppercase tracking-section text-ink-3">{item.label}</dt>
          <dd className={cn('mt-0.5 mb-0 ml-0 font-mono text-figure font-semibold', TONE[item.tone ?? 'default'])}>
            {item.value}
            {item.note !== undefined && (
              <span className="ml-1 text-label font-normal text-ink-3">{item.note}</span>
            )}
          </dd>
        </div>
      ))}
    </dl>
  );
}
