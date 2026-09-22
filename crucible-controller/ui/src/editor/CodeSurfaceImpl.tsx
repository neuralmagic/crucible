import { useEffect, useRef } from 'react';
import Editor, { type OnMount } from '@monaco-editor/react';
import { monaco } from './monaco';
import { languageOf } from './language';
import { useEditorPrefs } from './useEditorPrefs';

/// One engine complaint, at the position the engine named it.
export interface CodeMarker {
  line: number;
  col: number;
  message: string;
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
  focus?: CodeFocus | null;
  onSave?: () => void;
  height?: string;
  label?: string;
  testId?: string;
}

/// The marker owner, so setting ours never clears a language service's own.
const OWNER = 'engine';

/// The one Monaco surface: the studio's editor, and every read-only code viewer. What differs
/// between them is props, never a second embedding.
export default function CodeSurface({
  path,
  value,
  onChange,
  readOnly = false,
  markers = [],
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
        endLineNumber: marker.line,
        endColumn: model.getLineMaxColumn(Math.min(marker.line, model.getLineCount())),
      }))
    );
  }, [markers, path, value]);

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
