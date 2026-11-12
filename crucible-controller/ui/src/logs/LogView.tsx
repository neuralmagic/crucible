import { LazyLog } from '@melloware/react-logviewer';

/// A whole log: virtualized, searchable, line-numbered, with its ANSI colours in the app's palette
/// (global.css styles the viewer under `.crucible-log`). `follow` keeps the newest line in view —
/// the viewer stops following once the reader scrolls up.
export function LogView({ text, testId, follow }: { text: string; testId?: string; follow?: boolean }) {
  return (
    <div data-testid={testId} className="crucible-log min-h-0 flex-1">
      <LazyLog
        text={text}
        enableSearch
        enableLineNumbers
        selectableLines
        caseInsensitive
        extraLines={1}
        follow={follow}
      />
    </div>
  );
}
