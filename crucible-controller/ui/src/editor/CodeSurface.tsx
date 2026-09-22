import { lazy, Suspense } from 'react';
import type { CodeSurfaceProps } from './CodeSurfaceImpl';

export type { CodeFocus, CodeMarker, CodeSurfaceProps } from './CodeSurfaceImpl';

const Impl = lazy(() => import('./CodeSurfaceImpl'));

function Loading({ height }: { height: string }) {
  return (
    <div
      className="min-h-0 flex-1 border border-rule-hard bg-paper px-3 py-2 font-mono text-data text-ink-3"
      style={{ height }}
    >
      LOADING EDITOR
    </div>
  );
}

/// Monaco is several megabytes, so it rides its own chunk the way the Explore page's engine does;
/// a session that never opens a code surface never downloads one.
export function CodeSurface(props: CodeSurfaceProps) {
  return (
    <Suspense fallback={<Loading height={props.height ?? '100%'} />}>
      <Impl {...props} />
    </Suspense>
  );
}
