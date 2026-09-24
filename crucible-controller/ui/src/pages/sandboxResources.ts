import type { components } from '../api/schema';

type SandboxResourcesDto = components['schemas']['SandboxResourcesDto'];

/// What a pack's `[agent.resources]` asks the sandbox scheduler for, as one line; null when it asks
/// for nothing.
export function resourcesLabel(resources: SandboxResourcesDto): string | null {
  const parts = [
    resources.gpus > 0 ? `${resources.gpus} GPU${resources.gpus === 1 ? '' : 's'}` : null,
    resources.cpu === null || resources.cpu === undefined ? null : `cpu ${resources.cpu}`,
    resources.memory === null || resources.memory === undefined
      ? null
      : `memory ${resources.memory}`,
    ...Object.entries(resources.node_selector).map(([label, value]) => `on ${label}=${value}`),
  ].filter((part): part is string => part !== null);
  return parts.length === 0 ? null : parts.join(' · ');
}
