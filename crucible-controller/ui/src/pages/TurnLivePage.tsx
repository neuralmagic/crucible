import { useParams } from 'react-router-dom';
import { $api } from '../api/client';
import { TurnLive } from '../live/TurnLive';
import {
  Breadcrumb,
  DetailHeader,
  Empty,
  Identifier,
  Section,
  SectionBody,
  SectionHeader,
} from '../ui';

/// Minimal viewer over one turn pod's live stream — the `live` link target on the /turns ledger.
/// The same TurnLive pane the scope progress page embeds, plus a breadcrumb back to the ledger.
export function TurnLivePage() {
  const { pod: rawPod } = useParams<{ pod: string }>();
  const podName = rawPod ? decodeURIComponent(rawPod) : '';

  // The row is only for context (issue link, created_at); the stream itself needs just the name.
  const turns = $api.useQuery('get', '/api/turns');
  const row = (turns.data ?? []).find((t) => t.pod_name === podName) ?? null;

  if (!podName) {
    return <Empty title="NO POD NAME" description="The live viewer needs a work-pod name." />;
  }

  return (
    <>
      <Breadcrumb items={[{ label: 'Turns', to: '/turns' }, { label: podName }]} />
      <DetailHeader
        title={podName}
        meta={
          row?.issue_key ? (
            <Identifier variant="inline" to={`/issues/${encodeURIComponent(row.issue_key)}`}>
              {row.issue_key}
            </Identifier>
          ) : undefined
        }
      />
      <Section>
        <SectionHeader title="Live turn" />
        <SectionBody>
          <TurnLive podName={podName} createdAt={row?.created_at} />
        </SectionBody>
      </Section>
    </>
  );
}
