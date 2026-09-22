import type { ReactNode } from 'react';
import { formatError } from '../api/errors';
import { Empty } from './Empty';
import { LoadingBlock } from './LoadingBlock';

export interface QueryStateProps {
  query: { isError: boolean; isPending: boolean; error: unknown };
  /** Uppercase noun the labels derive from, e.g. "RUNS" → "RUNS UNAVAILABLE" / "LOADING RUNS". */
  noun: string;
  children: ReactNode;
}

/** The page-level render of a query: its error, then its pending band, then the content. */
export function QueryState({ query, noun, children }: QueryStateProps) {
  if (query.isError) return <Empty title={`${noun} UNAVAILABLE`} description={formatError(query.error)} />;
  if (query.isPending) return <LoadingBlock label={`LOADING ${noun}`} />;
  return children;
}
