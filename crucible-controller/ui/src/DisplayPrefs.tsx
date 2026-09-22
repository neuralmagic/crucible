import { useEffect, useState } from 'react';
import { Popover } from '@base-ui-components/react/popover';
import { usePrefs } from './api/prefs';
import { CO_DRAFT_HINT_KEY } from './pages/CoDraftHint';
import { useDeviceFlag } from './useDeviceFlag';
import { FONT_SIZES, type HighlightTheme } from './editor/editorPrefs';
import { useEditorPrefs } from './editor/useEditorPrefs';
import { cn } from './ui';

type Theme = 'dark' | 'light';

// index.html applies the attribute pre-paint from the same key; this only keeps state, storage and
// the <html> attribute in sync after hydration.
function initialTheme(): Theme {
  return localStorage.getItem('theme') === 'light' ? 'light' : 'dark';
}

interface ChoiceProps<T extends string | number | boolean> {
  label: string;
  hint?: string;
  options: readonly { value: T; label: string }[];
  value: T;
  onChange: (value: T) => void;
}

function Choice<T extends string | number | boolean>({
  label,
  hint,
  options,
  value,
  onChange,
}: ChoiceProps<T>) {
  return (
    <div className="border-b border-rule px-3 py-2.5 last:border-b-0">
      <div className="font-mono text-label font-semibold tracking-[0.1em] text-ink-3 uppercase">{label}</div>
      <div className="mt-1.5 flex">
        {options.map((option) => (
          <button
            key={String(option.value)}
            type="button"
            aria-pressed={option.value === value}
            onClick={() => {
              onChange(option.value);
            }}
            className={cn(
              'border border-rule-hard px-2.5 py-1 font-mono text-data text-ink-2',
              'border-r-0 last:border-r hover:bg-hi hover:text-ink',
              option.value === value && 'bg-ink font-semibold text-surface hover:bg-ink hover:text-surface',
            )}
          >
            {option.label}
          </button>
        ))}
      </div>
      {hint === undefined ? null : <p className="mt-1.5 max-w-[34ch] text-data text-ink-3">{hint}</p>}
    </div>
  );
}

export function DisplayPrefs() {
  const [theme, setTheme] = useState<Theme>(initialTheme);
  const { prefs, setPref } = usePrefs();
  const editor = useEditorPrefs();
  const [coDraftDismissed, setCoDraftDismissed] = useDeviceFlag(CO_DRAFT_HINT_KEY, false);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem('theme', theme);
  }, [theme]);

  return (
    <Popover.Root>
      <Popover.Trigger className="flex items-center border-l border-rule px-3 font-mono text-data text-ink-2 hover:bg-hi hover:text-ink">
        {theme === 'dark' ? '◐' : '◑'} DISPLAY
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Positioner sideOffset={1} align="end">
          <Popover.Popup className="max-h-[80vh] overflow-auto border border-rule-hard bg-surface">
            <Choice<Theme>
              label="Theme"
              options={[
                { value: 'dark', label: 'dark' },
                { value: 'light', label: 'light' },
              ]}
              value={theme}
              onChange={setTheme}
            />
            <Choice<HighlightTheme>
              label="Editor theme"
              hint="Curated pairings for every code surface. Each one follows the theme above."
              options={[
                { value: 'paper', label: 'paper' },
                { value: 'classic', label: 'classic' },
                { value: 'contrast', label: 'contrast' },
              ]}
              value={editor.prefs.theme}
              onChange={(value) => {
                editor.setPref('theme', value);
              }}
            />
            <Choice<number>
              label="Editor text"
              options={FONT_SIZES.map((size) => ({ value: size, label: `${size}` }))}
              value={editor.prefs.fontSize}
              onChange={(value) => {
                editor.setPref('fontSize', value);
              }}
            />
            <Choice<boolean>
              label="Word wrap"
              options={[
                { value: true, label: 'on' },
                { value: false, label: 'off' },
              ]}
              value={editor.prefs.wordWrap}
              onChange={(value) => {
                editor.setPref('wordWrap', value);
              }}
            />
            <Choice<boolean>
              label="Minimap"
              options={[
                { value: true, label: 'on' },
                { value: false, label: 'off' },
              ]}
              value={editor.prefs.minimap}
              onChange={(value) => {
                editor.setPref('minimap', value);
              }}
            />
            <Choice<boolean>
              label="Whitespace"
              hint="Renders spaces and tabs in every editor and viewer."
              options={[
                { value: true, label: 'on' },
                { value: false, label: 'off' },
              ]}
              value={editor.prefs.whitespace}
              onChange={(value) => {
                editor.setPref('whitespace', value);
              }}
            />
            <Choice<boolean>
              label="Chart phosphor"
              hint="Post-processes the charts as a CRT. Dark theme only, and off automatically under reduced motion."
              options={[
                { value: true, label: 'on' },
                { value: false, label: 'off' },
              ]}
              value={prefs.chartCrt}
              onChange={(value) => {
                setPref('chartCrt', value);
              }}
            />
            <Choice<boolean>
              label="Co-draft hint"
              hint="The walkthrough above the draft create form for pointing a local agent at this controller."
              options={[
                { value: true, label: 'shown' },
                { value: false, label: 'hidden' },
              ]}
              value={!coDraftDismissed}
              onChange={(value) => {
                setCoDraftDismissed(!value);
              }}
            />
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  );
}
