import { Section, SectionBody, SectionHeader } from '../ui';
import { agentToolRows, type AgentTool } from './agentToolsView';

/** The MCP servers the run's agent was granted at dispatch; nothing when it was granted none. */
export function RunAgentTools({ tools }: { tools: readonly AgentTool[] }) {
  const rows = agentToolRows(tools);
  if (rows.length === 0) return null;
  return (
    <Section>
      <SectionHeader title="Agent tools" note="MCP servers this run's agent could use" />
      <SectionBody>
        <ul className="grid gap-2 font-mono text-data">
          {rows.map((row) => (
            <li key={row.key} className="grid gap-0.5">
              <span>
                <span className="text-ink">{row.title}</span>
                {row.identity ? <span className="text-ink-2"> {row.identity}</span> : null}
                <span className="text-ink-3"> · {row.scopes}</span>
              </span>
              {row.limits ? <span className="text-ink-3">{row.limits}</span> : null}
              <span className="text-ink-3">{row.url}</span>
            </li>
          ))}
        </ul>
      </SectionBody>
    </Section>
  );
}
