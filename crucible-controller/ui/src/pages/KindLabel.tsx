import { Mono } from '../ui';
import type { components } from '../api/schema.d';

type InputKindDto = components['schemas']['InputKindDto'];

/// The issue's source, from the DTO tagged union, in its own table column so the title cell stays
/// free of chips. GitHub is the overwhelming majority case; scenario is an operator-adopted
/// free-text ask with no upstream GitHub item, jira an operator-adopted Jira issue, and unknown a
/// forward-rolled or corrupt kind, which renders the raw stored tag.
export function KindLabel({ kind }: { kind: InputKindDto }) {
  return (
    <Mono size="label" tone="ink-3" uppercase>
      {kind.type === 'unknown' ? kind.tag : kind.type}
    </Mono>
  );
}
