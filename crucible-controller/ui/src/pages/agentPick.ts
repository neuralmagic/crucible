/// What a launch form holds for the provider picker. Both blank is "resolve through the configured
/// defaults at dispatch", which is what the body omits both fields for.
export interface AgentPick {
  provider: string;
  model: string;
}

export const NO_AGENT_PICK: AgentPick = { provider: '', model: '' };

/// The provider/model half of a launch body. `model` never travels without a `provider`: it names
/// nothing on its own and the endpoint refuses it.
export function agentPickFields(agent: AgentPick | undefined): { provider?: string; model?: string } {
  const provider = (agent?.provider ?? '').trim();
  if (provider.length === 0) return {};
  const model = (agent?.model ?? '').trim();
  return { provider, ...(model.length > 0 ? { model } : {}) };
}
