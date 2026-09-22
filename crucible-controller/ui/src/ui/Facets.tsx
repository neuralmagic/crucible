import { useState } from 'react';
import { cn } from './cn';

export interface FacetOption {
  value: string;
  label: string;
  /** Rows this option would yield. Undefined renders no count; 0 renders the option inert. */
  count?: number;
  /** Hover text when the label is an abbreviation of the value. */
  title?: string;
}

export interface FacetRow {
  label: string;
  options: readonly FacetOption[];
  value: string;
  onChange: (value: string) => void;
  /** Options past this many collapse behind a "+N" toggle. Unset shows every option. */
  maxVisible?: number;
}

export interface FacetsProps {
  rows: readonly FacetRow[];
  /** Accessible name for the filter list. */
  label?: string;
  className?: string;
}

const OPTION =
  'flex items-baseline gap-1.5 border-r border-rule px-2 py-px font-mono text-data text-ink-2 ' +
  'hover:bg-hi hover:text-ink focus-visible:outline-2 focus-visible:outline-blue';

function FacetLine({ row }: { row: FacetRow }) {
  const [expanded, setExpanded] = useState(false);

  // The cap counts real options; "All" is always shown, and a selected option never hides behind the
  // toggle, because you must be able to see what is narrowing you.
  const cap = row.maxVisible;
  const real = row.options.filter((o) => o.value !== '');
  const overflowing = cap !== undefined && real.length > cap && !expanded;
  const keep = new Set(overflowing ? real.slice(0, cap).map((o) => o.value) : real.map((o) => o.value));
  const visible = row.options.filter((o) => o.value === '' || keep.has(o.value) || o.value === row.value);
  const hidden = row.options.length - visible.length;

  return (
    <div className="flex min-h-[21px] items-stretch border-b border-rule last:border-b-0">
      <dt className="flex w-[64px] flex-none items-center border-r border-rule bg-sunk px-2 font-mono text-micro font-semibold tracking-[0.08em] text-ink-3 uppercase">
        {row.label}
      </dt>
      <dd className="m-0 flex min-w-0 flex-1 flex-wrap items-center">
        {visible.map((option) => {
          const selected = option.value === row.value;
          const isDefault = option.value === '';
          const empty = option.count === 0 && !selected;
          return (
            <button
              key={option.value || 'all'}
              type="button"
              title={option.title}
              aria-pressed={selected}
              disabled={empty}
              onClick={() => {
                row.onChange(option.value);
              }}
              className={cn(
                OPTION,
                selected && !isDefault && 'bg-ink font-semibold text-surface hover:bg-ink hover:text-surface',
                selected && isDefault && 'font-semibold text-ink',
                empty && 'pointer-events-none opacity-40',
              )}
            >
              {option.label}
              {option.count === undefined ? null : (
                <span
                  className={cn(
                    'font-mono text-micro tabular-nums',
                    selected && !isDefault ? 'text-surface opacity-70' : 'text-ink-3',
                  )}
                >
                  {option.count}
                </span>
              )}
            </button>
          );
        })}
        {hidden > 0 || expanded ? (
          <button
            type="button"
            onClick={() => {
              setExpanded(!expanded);
            }}
            className={cn(OPTION, 'text-ink-3 hover:text-ink')}
          >
            {expanded ? 'less' : `+${hidden}`}
          </button>
        ) : null}
      </dd>
    </div>
  );
}

export function Facets({ rows, label = 'Filters', className }: FacetsProps) {
  return (
    <dl aria-label={label} className={cn('m-0 border-b border-rule-hard bg-surface', className)}>
      {rows.map((row) => (
        <FacetLine key={row.label} row={row} />
      ))}
    </dl>
  );
}

export interface AppliedFilter {
  label: string;
  onClear: () => void;
}

export interface AppliedProps {
  shown: number;
  total: number;
  noun: string;
  filters: readonly AppliedFilter[];
  onClearAll: () => void;
}

export function Applied({ shown, total, noun, filters, onClearAll }: AppliedProps) {
  if (filters.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-2 border-b border-rule-hard bg-hi px-2 py-0.5 font-mono text-micro text-ink-2">
      <span>
        <b className="font-semibold text-ink">{shown}</b> of {total} {noun}
      </span>
      {filters.map((filter) => (
        <button
          key={filter.label}
          type="button"
          onClick={filter.onClear}
          className="group flex items-center gap-1 border border-rule-hard bg-surface px-1.5 text-ink"
        >
          {filter.label}
          <span aria-hidden className="text-ink-3 group-hover:text-red">
            &#215;
          </span>
          <span className="sr-only">clear</span>
        </button>
      ))}
      <button
        type="button"
        onClick={onClearAll}
        className="ml-auto border-b border-rule-hard text-ink-3 hover:border-ink hover:text-ink"
      >
        Clear all
      </button>
    </div>
  );
}
