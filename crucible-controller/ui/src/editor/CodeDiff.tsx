import { lazy, Suspense } from 'react';
import type { CodeDiffProps } from './CodeDiffImpl';

export type { CodeDiffProps } from './CodeDiffImpl';

const Impl = lazy(() => import('./CodeDiffImpl'));

export function CodeDiff(props: CodeDiffProps) {
  return (
    <Suspense
      fallback={
        <div
          className="border border-rule-hard bg-paper px-3 py-2 font-mono text-data text-ink-3"
          style={{ height: props.height ?? '20rem' }}
        >
          LOADING DIFF
        </div>
      }
    >
      <Impl {...props} />
    </Suspense>
  );
}
