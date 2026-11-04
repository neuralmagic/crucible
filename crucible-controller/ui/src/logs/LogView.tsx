import { LazyLog } from '@melloware/react-logviewer';

/// A whole log: virtualized, searchable, line-numbered, with its ANSI colours in the app's palette
/// (global.css styles the viewer under `.crucible-log`).
export function LogView({ text, testId }: { text: string; testId?: string }) {
  return (
    <div data-testid={testId} className="crucible-log min-h-0 flex-1">
      <LazyLog text={text} enableSearch enableLineNumbers selectableLines caseInsensitive extraLines={1} />
    </div>
  );
}
