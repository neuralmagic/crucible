import { useEffect, useRef, useState, type ReactNode } from 'react';
import { $api } from '../api/client';
import { CodeSurface, type CodeCompletions } from '../editor/CodeSurface';
import { Mono } from '../ui';
import { markersOf, type CelDiagnosticDto } from './celTooling';

const CHECK_DELAY_MS = 350;

/// Every refusal a save would answer for this transform, checked on the server as the author types.
/// A response that arrives after a newer edit is dropped.
export function useCelCheck(
  filter: string,
  dedupe: string,
  derive: Readonly<Record<string, string>>
): readonly CelDiagnosticDto[] {
  const { mutateAsync } = $api.useMutation('post', '/api/webhooks/check');
  const [diagnostics, setDiagnostics] = useState<readonly CelDiagnosticDto[]>([]);
  const latest = useRef(0);
  useEffect(() => {
    const ticket = latest.current + 1;
    latest.current = ticket;
    const timer = setTimeout(() => {
      mutateAsync({ body: { filter, dedupe, derive: { ...derive } } })
        .then((checked) => {
          if (latest.current === ticket) setDiagnostics(checked.diagnostics);
        })
        .catch(() => {
          if (latest.current === ticket) setDiagnostics([]);
        });
    }, CHECK_DELAY_MS);
    return () => {
      clearTimeout(timer);
    };
  }, [filter, dedupe, derive, mutateAsync]);
  return diagnostics;
}

export interface CelEditorProps {
  field: string;
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  diagnostics: readonly CelDiagnosticDto[];
  completions: CodeCompletions | undefined;
  lines?: number;
}

/// One CEL expression: highlighted, completed, and marked where the server's check refused it.
export function CelEditor({
  field,
  label,
  value,
  onChange,
  diagnostics,
  completions,
  lines = 1,
}: CelEditorProps) {
  return (
    <div className="grid gap-1">
      <Mono size="label" weight="semibold" uppercase tone="ink-2" className="tracking-section">
        {label}
      </Mono>
      <CodeSurface
        path={`${field}.cel`}
        value={value}
        onChange={onChange}
        markers={markersOf(diagnostics, value)}
        completions={completions}
        height={`${lines * 1.4 + 1}rem`}
        label={field}
        testId={`cel-${field}`}
      />
      {diagnostics.map((d) => (
        <Mono key={`${d.line ?? 0}:${d.column ?? 0}:${d.message}`} size="data" tone="red">
          {d.line != null && d.column != null ? `${d.line}:${d.column}: ` : ''}
          {d.message}
        </Mono>
      ))}
    </div>
  );
}
