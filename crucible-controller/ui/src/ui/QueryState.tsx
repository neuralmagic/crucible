import type { ReactNode } from 'react';
import { formatError } from '../api/errors';
import { Empty } from './Empty';
import { LoadingBlock } from './LoadingBlock';

export interface QueryStateProps {
  query: { isError: boolean; isPending: boolean; error: unknown };
  /** Lowercase noun the labels derive from, e.g. "runs" → "Runs unavailable" / "Loading runs". */
  noun: string;
  children: ReactNode;
}

/** The page-level render of a query: its error, then its pending band, then the content. */
export function QueryState({ query, noun, children }: QueryStateProps) {
  if (query.isError) {
    return <Empty title={`${noun.charAt(0).toUpperCase()}${noun.slice(1)} unavailable`} description={formatError(query.error)} />;
  }
  if (query.isPending) return <LoadingBlock label={`Loading ${noun}`} />;
  return children;
}
