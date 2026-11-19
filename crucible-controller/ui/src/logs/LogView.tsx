import { useMemo, useState } from 'react';
import { LazyLog } from '@melloware/react-logviewer';
import { CopyButton } from '../ui/CopyButton';
import { Button } from '../ui/Button';
import { ansiPlain } from './ansi';
import { filterByPod, podPrefixes, type PodCount } from './pods';

interface LogViewProps {
  text: string;
  testId?: string;
  /// Keep the newest line in view — the viewer stops following once the reader scrolls up.
  follow?: boolean;
  /// Pods named elsewhere; parsed from `text` when absent.
  pods?: readonly PodCount[];
  /// The pod the declared failure came from, pinned until the reader picks another.
  failingPod?: string | null;
}

/// A whole log: virtualized, searchable, line-numbered, with its ANSI colours in the app's palette
/// (global.css styles the viewer under `.crucible-log`). A body that names several pods gets a
/// filter, with the failing pod pinned when the body still carries its lines.
export function LogView({ text, testId, follow, pods, failingPod }: LogViewProps) {
  const [choice, setChoice] = useState<string | null | undefined>(undefined);
  const counts = useMemo<readonly PodCount[]>(
    () => pods ?? podPrefixes(text),
    [pods, text]
  );
  const named =
    failingPod !== null &&
    failingPod !== undefined &&
    counts.some((count) => count.pod === failingPod);
  const pinned = named && failingPod !== null && failingPod !== undefined ? failingPod : null;
  const chosen = choice === undefined ? pinned : choice;
  const filtered = chosen !== null && counts.some((count) => count.pod === chosen) ? chosen : null;
  const shown = filterByPod(text, filtered);
  const showFilter = counts.length >= 2 || (counts.length === 1 && named);

  return (
    <div data-testid={testId} className="flex min-h-0 flex-1 flex-col">
      {showFilter && (
        <div
          data-testid="log-pods"
          className="flex flex-wrap items-center gap-1 border-b border-rule px-1.5 py-1"
        >
          <span className="mr-1 text-micro tracking-label text-ink-3 uppercase">pod</span>
          <Button selected={filtered === null} onClick={() => setChoice(null)}>
            all
          </Button>
          {counts.map((count) => (
            <Button
              key={count.pod}
              selected={filtered === count.pod}
              title={`${count.pod} · ${count.lines} lines`}
              onClick={() => setChoice(count.pod)}
            >
              <span className="max-w-72 truncate">{count.pod}</span>
              <span className="ml-1 text-ink-3">{count.lines}</span>
            </Button>
          ))}
        </div>
      )}
      <div className="crucible-log relative min-h-0 flex-1">
        <LazyLog
          text={shown}
          enableSearch
          enableLineNumbers
          selectableLines
          caseInsensitive
          extraLines={1}
          follow={follow}
        />
        <CopyButton value={() => ansiPlain(text)} label="log" className="absolute top-0.5 right-1.5" />
      </div>
    </div>
  );
}
