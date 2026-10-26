import { useQueryClient } from '@tanstack/react-query';
import { Button, Status } from './ui';
import { $api } from './api/client';
import { AGENT_TOOLS_NOTICE, agentToolsNotice } from './pages/agentToolsView';

/** Announces once, per person, that the MCP servers runs get by default changed. */
export function AgentToolsBanner() {
  const queryClient = useQueryClient();
  const defaults = $api.useQuery('get', '/api/agent-tools');
  const notices = $api.useQuery('get', '/api/prefs/notices');
  const dismiss = $api.useMutation('put', '/api/prefs/notices');

  const stored = notices.data?.prefs;
  const notice = agentToolsNotice(defaults.data, stored);
  if (notice === null || defaults.data === undefined) return null;
  const revision = defaults.data.revision;

  return (
    <div className="flex flex-none items-center gap-3 border-b border-rule-hard bg-surface px-3.5 py-1.5 font-mono text-data">
      <Status status="agent tools" tone="blue" />
      <span className="text-ink-2">{notice}</span>
      <Button
        className="ml-auto uppercase"
        onClick={() => {
          dismiss.mutate(
            { body: { prefs: { ...stored, [AGENT_TOOLS_NOTICE]: revision } } },
            {
              onSettled: () => {
                void queryClient.invalidateQueries({ queryKey: ['get', '/api/prefs/notices'] });
              },
            },
          );
        }}
      >
        Dismiss
      </Button>
    </div>
  );
}
