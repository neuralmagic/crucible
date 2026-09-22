import { Link } from 'react-router-dom';
import { $api } from './api/client';
import { Section, SectionBody, SectionHeader, Spinner, Status } from './ui';

// The Dashboard's read-only autopilot glance: the kill-switch state plus a link to /admin where
// the toggle now lives. Visible to everyone; the mutating control is admin-gated on the admin page.
export function AutopilotStatusChip() {
  const autopilot = $api.useQuery('get', '/api/autopilot');

  return (
    <Section>
      <SectionHeader title="Autopilot" />
      <SectionBody>
        {autopilot.isPending ? (
          <Spinner />
        ) : autopilot.isError ? (
          <Status status="unknown" tone="grey" />
        ) : (
          <div className="flex items-center gap-4">
            <Status
              status={autopilot.data.enabled ? 'enabled' : 'disabled'}
              tone={autopilot.data.enabled ? 'green' : 'amber'}
            />
            <Link
              to="/admin"
              className="font-mono text-data text-ink-2 underline-offset-2 hover:text-ink hover:underline"
            >
              MANAGE
            </Link>
          </div>
        )}
      </SectionBody>
    </Section>
  );
}
