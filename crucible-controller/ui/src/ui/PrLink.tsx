import { cn } from './cn';
import { parsePrNumber } from './prNumber';

export type PrState = 'open' | 'merged' | 'closed';

const TONE: Record<PrState, string> = {
  open: 'text-blue',
  merged: 'text-green',
  closed: 'text-ink-3',
};

export interface PrLinkProps {
  /** The pull-request url; the number is parsed out of it. */
  url: string;
  /** Paints the link when the wire carries the PR's state. Unset renders the neutral tone. */
  state?: PrState;
  /** Table-row form (`#123`); the default is the detail-header form (`PR #123`). */
  compact?: boolean;
  className?: string;
}

export function PrLink({ url, state, compact = false, className }: PrLinkProps) {
  const number = parsePrNumber(url);
  const label = number === null ? 'PR' : `#${number}`;
  return (
    <a
      href={url}
      target="_blank"
      rel="noopener noreferrer"
      title={url}
      onClick={(event) => {
        event.stopPropagation();
      }}
      className={cn(
        'border-b border-rule-hard font-mono text-data hover:border-ink',
        state === undefined ? 'text-ink-2 hover:text-ink' : TONE[state],
        className,
      )}
    >
      {compact || number === null ? label : `PR ${label}`}
    </a>
  );
}
