// The `[pod/<pod>/<container>]` prefix kubectl writes under `logs --prefix`, read off a log body
// so a reader can narrow a run's interleaved output to one pod.

import { ansiPlain } from './ansi';

export interface PodPrefix {
  pod: string;
  container: string | null;
}

export interface PodCount {
  pod: string;
  lines: number;
}

const PREFIX = /^\[pod\/([^\]/]+)(?:\/([^\]]+))?\]/;

/// The pod and container a line was prefixed with, when it carries one.
export function podPrefixOf(line: string): PodPrefix | null {
  const match = PREFIX.exec(ansiPlain(line));
  if (match === null) return null;
  const [, pod, container] = match;
  if (pod === undefined || pod === '') return null;
  return { pod, container: container ?? null };
}

/// The pods `text` names, in first-seen order, with how many lines each prefixed. A pod that only
/// continues a message is not counted as absent: kubectl prefixes every line it forwards.
export function podPrefixes(text: string): PodCount[] {
  const counts: PodCount[] = [];
  for (const line of text.split('\n')) {
    const prefix = podPrefixOf(line);
    if (prefix === null) continue;
    const found = counts.find((count) => count.pod === prefix.pod);
    if (found === undefined) counts.push({ pod: prefix.pod, lines: 1 });
    else found.lines += 1;
  }
  return counts;
}

/// `text` narrowed to the lines `pod` prefixed; null keeps the whole body.
export function filterByPod(text: string, pod: string | null): string {
  if (pod === null) return text;
  return text
    .split('\n')
    .filter((line) => podPrefixOf(line)?.pod === pod)
    .join('\n');
}
