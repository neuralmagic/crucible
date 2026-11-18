import { useState } from 'react';
import { cn } from './cn';

export interface CopyButtonProps {
  /** The text to copy, or a function that builds it when the button is pressed. */
  value: string | (() => string);
  /** What the value is, for the accessible name: "Copy <label>". */
  label: string;
  className?: string;
}

/** A clipboard icon that copies `value` and shows a check for a moment after. */
export function CopyButton({ value, label, className }: CopyButtonProps) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      title={copied ? 'Copied' : `Copy ${label}`}
      aria-label={`Copy ${label}`}
      className={cn(
        'inline-flex cursor-pointer items-center justify-center border-0 bg-transparent p-1 text-ink-3 hover:text-ink',
        className,
      )}
      onClick={() => {
        void navigator.clipboard.writeText(typeof value === 'function' ? value() : value).then(() => {
          setCopied(true);
          window.setTimeout(() => {
            setCopied(false);
          }, 1500);
        });
      }}
    >
      <svg
        aria-hidden
        viewBox="0 0 24 24"
        width="14"
        height="14"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        {copied ? (
          <path d="M20 6 9 17l-5-5" />
        ) : (
          <>
            <rect x="9" y="9" width="12" height="12" rx="2" />
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
          </>
        )}
      </svg>
    </button>
  );
}
