import { lazy, Suspense } from 'react';
import type { CodeSurfaceProps } from './CodeSurfaceImpl';

export type { CodeCompletion, CodeCompletions, CodeFocus, CodeMarker, CodeSurfaceProps } from './CodeSurfaceImpl';

const Impl = lazy(() => import('./CodeSurfaceImpl'));

/// What stands in while Monaco downloads. An editable surface is a plain textarea bound to the
/// same value, so what the author types before the editor arrives is kept.
function Loading({ value, onChange, readOnly, height = '100%', label, testId }: CodeSurfaceProps) {
  if (onChange === undefined || readOnly) {
    return (
      <div
        className="min-h-0 flex-1 border border-rule-hard bg-paper px-3 py-2 font-mono text-data text-ink-3 uppercase"
        style={{ height }}
      >
        Loading editor
      </div>
    );
  }
  return (
    <textarea
      className="min-h-0 w-full flex-1 resize-none border border-rule-hard bg-paper px-3 py-2 font-mono text-data text-ink"
      style={{ height }}
      value={value}
      spellCheck={false}
      aria-label={label}
      data-testid={testId === undefined ? undefined : `${testId}-loading`}
      onChange={(e) => {
        onChange(e.target.value);
      }}
    />
  );
}

/// Monaco is several megabytes, so it rides its own chunk the way the Explore page's engine does;
/// a session that never opens a code surface never downloads one.
export function CodeSurface(props: CodeSurfaceProps) {
  return (
    <Suspense fallback={<Loading {...props} />}>
      <Impl {...props} />
    </Suspense>
  );
}
