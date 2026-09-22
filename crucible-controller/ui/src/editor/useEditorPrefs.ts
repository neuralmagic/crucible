import { useCallback } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import {
  type EditorPrefs,
  monacoThemeName,
  parseEditorPrefs,
  type SurfaceOptions,
  surfaceOptions,
} from './editorPrefs';
import { useAppTheme } from '../appTheme';

const QUERY_KEY = ['get', '/api/prefs/editor'];

export interface EditorPrefsHandle {
  prefs: EditorPrefs;
  /// The resolved Monaco theme name for the current app theme.
  theme: string;
  options: SurfaceOptions;
  setPref: <K extends keyof EditorPrefs>(key: K, value: EditorPrefs[K]) => void;
}

/// The editor-preferences document, read at load and written on change. Every Monaco surface calls
/// this, so one write reaches all of them.
export function useEditorPrefs(): EditorPrefsHandle {
  const queryClient = useQueryClient();
  const query = $api.useQuery('get', '/api/prefs/editor');
  const mutation = $api.useMutation('put', '/api/prefs/editor');
  const app = useAppTheme();

  const stored: Record<string, unknown> | undefined = query.data?.prefs;
  const prefs = parseEditorPrefs(stored);

  const setPref = useCallback(
    <K extends keyof EditorPrefs>(key: K, value: EditorPrefs[K]) => {
      const next: Record<string, unknown> = { ...stored, [key]: value };
      mutation.mutate(
        { body: { prefs: next } },
        {
          onSettled: () => {
            void queryClient.invalidateQueries({ queryKey: QUERY_KEY });
          },
        }
      );
    },
    [stored, mutation, queryClient]
  );

  return {
    prefs,
    theme: monacoThemeName(prefs.theme, app),
    options: surfaceOptions(prefs),
    setPref,
  };
}
