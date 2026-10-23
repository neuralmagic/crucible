import type { components } from '../api/schema.d';

export type AgentTool = components['schemas']['AgentTool'];
export type AgentToolDefaults = components['schemas']['Defaults'];

/** The notices document key the agent tools revision is stored under. */
export const AGENT_TOOLS_NOTICE = 'agent_tools';

export interface AgentToolRow {
  key: string;
  /** `kueue on pirate` */
  title: string;
  /** `as wseaton`, or empty when the server acts as no Kubernetes user. */
  identity: string;
  scopes: string;
  /** `namespace crucible-kueue-e2e · push_repo quay.io/...`, sorted by name. */
  limits: string;
  url: string;
}

/** One row per granted server, kueue before buildit as the API orders them. */
export function agentToolRows(tools: readonly AgentTool[]): AgentToolRow[] {
  return tools.map((t) => ({
    key: `${t.server}/${t.cluster}`,
    title: `${t.server} on ${t.cluster}`,
    identity: t.kube_user ? `as ${t.kube_user}` : '',
    scopes: t.scopes.join(' '),
    limits: Object.entries(t.limits)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([name, value]) => `${name} ${value}`)
      .join(' · '),
    url: t.url,
  }));
}

/**
 * The notice to show a person, or null: only when the controller grants something by default and
 * its revision is not the one they last dismissed.
 */
export function agentToolsNotice(
  defaults: AgentToolDefaults | undefined,
  notices: Record<string, unknown> | undefined,
): string | null {
  if (defaults === undefined || defaults.tools.length === 0) return null;
  if (notices === undefined) return null;
  return notices[AGENT_TOOLS_NOTICE] === defaults.revision ? null : defaults.changelog;
}
