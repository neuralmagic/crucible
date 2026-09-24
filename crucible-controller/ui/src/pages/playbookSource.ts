import type { components } from '../api/schema';

type PlaybookSourceDto = components['schemas']['PlaybookSourceDto'];

/// Where a registered pack came from, as one line: `repo/path`, or the draft version it was
/// published from.
export function sourceLabel(source: PlaybookSourceDto): string {
  switch (source.kind) {
    case 'git':
      return source.path === '' ? source.repo : `${source.repo}/${source.path}`;
    case 'draft':
      return `draft ${source.draft} v${source.version}`;
  }
}
