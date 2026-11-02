import { useState } from 'react';
import { cn } from './cn';
import { middleTruncate } from './format';

export interface CopyTextProps {
  value: string;
  /** Characters kept from the start and from the end once the value is cut. */
  head?: number;
  tail?: number;
  className?: string;
}

/** A long id or digest, cut in the middle, that copies the whole value when clicked. */
export function CopyText({ value, head = 14, tail = 8, className }: CopyTextProps) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      title={value}
      data-copy={value}
      className={cn(
        'cursor-copy border-0 bg-transparent p-0 font-mono text-ink-3 hover:text-ink',
        className,
      )}
      onClick={() => {
        void navigator.clipboard.writeText(value).then(() => {
          setCopied(true);
          window.setTimeout(() => {
            setCopied(false);
          }, 1500);
        });
      }}
    >
      <span aria-hidden>{copied ? 'copied' : middleTruncate(value, head, tail)}</span>
      <span className="sr-only">Copy {value}</span>
    </button>
  );
}
