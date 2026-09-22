import { DiffEditor } from '@monaco-editor/react';
import './monaco';
import { languageOf } from './language';
import { useEditorPrefs } from './useEditorPrefs';

export interface CodeDiffProps {
  path: string;
  /// The side that overtook this writer: what is stored now.
  original: string;
  /// The side being held: the writer's own buffer.
  modified: string;
  height?: string;
  testId?: string;
}

/// The same Monaco, in diff mode. The stale-base refusal renders here so the writer reads what
/// actually differs instead of a sentence about it.
export default function CodeDiff({ path, original, modified, height = '20rem', testId }: CodeDiffProps) {
  const { theme, options } = useEditorPrefs();
  const language = languageOf(path);

  return (
    <div className="border border-rule-hard bg-paper" data-testid={testId} style={{ height }}>
      <DiffEditor
        original={original}
        modified={modified}
        language={language}
        originalModelPath={`stored/${path}`}
        modifiedModelPath={`buffer/${path}`}
        theme={theme}
        loading={<span className="px-3 py-2 font-mono text-data text-ink-3">LOADING DIFF</span>}
        options={{
          ...options,
          readOnly: true,
          renderSideBySide: true,
          renderOverviewRuler: false,
          renderGutterMenu: false,
          ignoreTrimWhitespace: false,
        }}
      />
    </div>
  );
}
