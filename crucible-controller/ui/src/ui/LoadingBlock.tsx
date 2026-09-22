import { cn } from './cn';
import { Spinner } from './Spinner';

export interface LoadingBlockProps {
  /** Uppercase mono caption, e.g. "LOADING ISSUES". */
  label?: string;
  className?: string;
}

/** The page-level pending state: a padded band where the content will land. */
export function LoadingBlock({ label, className }: LoadingBlockProps) {
  return (
    <div className={cn('border-b border-rule px-4.5 py-6', className)}>
      <Spinner label={label} />
    </div>
  );
}
