import { Mono, Status, statusTone } from '../ui';
import { issueStatusColor } from './issueStatus';
import { launchPath } from './launchView';

/// The event log's transition cell. Events the controller records about things with no status
/// vocabulary — a draft generated, a playbook registered — carry `from === to` and render as one
/// chip, not a self-pointing arrow.
export function EventTransition({ from, to }: { from: string; to: string }) {
  if (from === to) {
    return <Status status={to} tone={statusTone(issueStatusColor(to))} />;
  }
  return (
    <span className="flex items-center gap-2">
      <Status status={from} tone={statusTone(issueStatusColor(from))} />
      <Mono tone="ink-3">→</Mono>
      <Status status={to} tone={statusTone(issueStatusColor(to))} />
    </span>
  );
}

const LAUNCH_KEY = /^playbook:(.+):([0-9a-f-]+)$/i;
const ISSUE_KEY = /^[^/]+\/[^/]+#\d+$/;

/// Where an event's key leads, or null when the thing it names has no page of its own. A key
/// without a surface renders as text, never as a link to a page that is not about it.
export function eventKeyPath(key: string): string | null {
  if (LAUNCH_KEY.test(key)) {
    return launchPath(key);
  }
  const scoped = /^([a-z]+):([^:]+)$/.exec(key);
  if (scoped !== null) {
    const id = encodeURIComponent(scoped[2]);
    switch (scoped[1]) {
      case 'draft':
        return `/playbooks/drafts/${id}`;
      case 'playbook':
        return `/playbooks/${id}`;
      case 'import':
        return `/playbooks/import/${id}`;
      case 'run':
        return `/runs/${id}`;
    }
  }
  if (ISSUE_KEY.test(key)) {
    return `/issues/${encodeURIComponent(key)}`;
  }
  return null;
}
