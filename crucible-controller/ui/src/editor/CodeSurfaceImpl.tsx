import { useEffect, useRef } from 'react';
import Editor, { type OnMount } from '@monaco-editor/react';
import { monaco } from './monaco';
import { languageOf } from './language';
import { useEditorPrefs } from './useEditorPrefs';

/// One engine complaint, at the position the engine named it. Without an end it runs to the end of
/// its line.
export interface CodeMarker {
  line: number;
  col: number;
  endLine?: number;
  endCol?: number;
  message: string;
}

export interface CodeCompletion {
  label: string;
  kind: 'keyword' | 'type' | 'value';
}

/// What to offer at the caret, given the line up to it. `from` is the 0-based index in that prefix
/// where the replaced text starts.
export interface CodeCompletions {
  triggers: readonly string[];
  complete: (linePrefix: string) => { from: number; items: readonly CodeCompletion[] } | null;
}

/// A position to put the caret on. The nonce is what makes clicking the same diagnostic twice
/// reveal it twice.
export interface CodeFocus {
  line: number;
  col: number;
  nonce: number;
}

export interface CodeSurfaceProps {
  /// The pack-relative path, which names the model and picks the grammar.
  path: string;
  value: string;
  /// Absent means read-only: a viewer, not an editor.
  onChange?: (next: string) => void;
  readOnly?: boolean;
  markers?: readonly CodeMarker[];
  completions?: CodeCompletions;
  focus?: CodeFocus | null;
  onSave?: () => void;
  height?: string;
  label?: string;
  testId?: string;
}

/// The marker owner, so setting ours never clears a language service's own.
const OWNER = 'engine';

const COMPLETION_KIND: Record<CodeCompletion['kind'], monaco.languages.CompletionItemKind> = {
  keyword: monaco.languages.CompletionItemKind.Keyword,
  type: monaco.languages.CompletionItemKind.Class,
  value: monaco.languages.CompletionItemKind.Value,
};

/// The one Monaco surface: the studio's editor, and every read-only code viewer. What differs
/// between them is props, never a second embedding.
export default function CodeSurface({
  path,
  value,
  onChange,
  readOnly = false,
  markers = [],
  completions,
  focus = null,
  onSave,
  height = '100%',
  label,
  testId,
}: CodeSurfaceProps) {
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);
  const saveRef = useRef(onSave);
  saveRef.current = onSave;
  const { theme, options } = useEditorPrefs();
  const language = languageOf(path);
  const editable = onChange !== undefined && !readOnly;

  const handleMount: OnMount = (editor) => {
    editorRef.current = editor;
    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => {
      saveRef.current?.();
    });
  };

  useEffect(() => {
    const model = editorRef.current?.getModel() ?? null;
    if (model === null) return;
    monaco.editor.setModelMarkers(
      model,
      OWNER,
      markers.map((marker) => ({
        severity: monaco.MarkerSeverity.Error,
        message: marker.message,
        startLineNumber: marker.line,
        startColumn: marker.col,
        endLineNumber: marker.endLine ?? marker.line,
        endColumn: marker.endCol ?? model.getLineMaxColumn(Math.min(marker.line, model.getLineCount())),
      }))
    );
  }, [markers, path, value]);

  useEffect(() => {
    if (completions === undefined) return;
    const provider = monaco.languages.registerCompletionItemProvider(language, {
      triggerCharacters: [...completions.triggers],
      provideCompletionItems(model, position) {
        if (model !== editorRef.current?.getModel()) return { suggestions: [] };
        const prefix = model.getLineContent(position.lineNumber).slice(0, position.column - 1);
        const found = completions.complete(prefix);
        if (found === null) return { suggestions: [] };
        const range = {
          startLineNumber: position.lineNumber,
          startColumn: found.from + 1,
          endLineNumber: position.lineNumber,
          endColumn: position.column,
        };
        return {
          suggestions: found.items.map((item) => ({
            label: item.label,
            insertText: item.label,
            kind: COMPLETION_KIND[item.kind],
            range,
          })),
        };
      },
    });
    return () => {
      provider.dispose();
    };
  }, [completions, language]);

  useEffect(() => {
    const editor = editorRef.current;
    if (focus === null || editor === null) return;
    editor.revealLineInCenter(focus.line);
    editor.setPosition({ lineNumber: focus.line, column: focus.col });
    editor.focus();
    // The nonce is the point: the same line focused again has to reveal again.
  }, [focus]);

  return (
    <div
      className="min-h-0 flex-1 border border-rule-hard bg-paper"
      data-testid={testId}
      aria-label={label}
      style={{ height }}
    >
      <Editor
        path={path}
        language={language}
        value={value}
        theme={theme}
        onChange={(next) => {
          onChange?.(next ?? '');
        }}
        onMount={handleMount}
        loading={<span className="px-3 py-2 font-mono text-data text-ink-3">LOADING EDITOR</span>}
        options={{
          ...options,
          readOnly: !editable,
          domReadOnly: !editable,
          tabSize: 4,
          renderLineHighlight: editable ? 'line' : 'none',
        }}
      />
    </div>
  );
}
