import { useState } from 'react';
import { $api } from '../api/client';
import { RichSelectField, SelectField, TextField } from './formControls';
import type { components } from '../api/schema';
import { NO_AGENT_PICK, type AgentPick } from './agentPick';
import { ProviderIcon } from './ProviderIcon';

type ProviderDto = components['schemas']['ProviderDto'];
type DispatchDefaultDto = components['schemas']['DispatchDefaultDto'];
type WorkloadClass = components['schemas']['WorkloadClass'];

const OTHER = '__other__';

/// Two providers may share a display name, so the option carries the kind mark and the registry
/// id as well.
function describe(provider: ProviderDto) {
  return (
    <>
      <ProviderIcon kind={provider.kind} />
      <span className="truncate">{provider.display_name}</span>
      <span className="truncate text-ink-3">
        {provider.kind} · {provider.id}
      </span>
    </>
  );
}

/// The default a launch of this class inherits by picking nothing. Domain defaults are keyed by a
/// domain the form does not know yet, so only the platform row can be shown here.
function platformDefault(
  defaults: readonly DispatchDefaultDto[],
  workloadClass: WorkloadClass
): DispatchDefaultDto | undefined {
  return defaults.find((d) => d.scope_kind === 'platform' && d.workload_class === workloadClass);
}

interface ProviderModelFieldProps {
  idPrefix: string;
  /// Which set of defaults this form's dispatches resolve through.
  workloadClass: WorkloadClass;
  value: AgentPick;
  onChange: (next: AgentPick) => void;
}

/// The inference provider picker. Renders nothing when the registry is empty: a deployment that
/// configured no provider dispatches on the pack manifest's own agent, and should not grow a
/// control whose only option is that.
export function ProviderModelField({
  idPrefix,
  workloadClass,
  value,
  onChange,
}: ProviderModelFieldProps) {
  const registry = $api.useQuery('get', '/api/config/providers');
  const [freeText, setFreeText] = useState(false);

  if (registry.data === undefined) return null;
  const providers = registry.data.providers;
  if (providers.length === 0) return null;

  const inherited = platformDefault(registry.data.defaults, workloadClass);
  const inheritedProvider = providers.find((p) => p.id === inherited?.provider);
  const selected = providers.find((p) => p.id === value.provider);

  const defaultModelFor = (provider: ProviderDto): string =>
    provider.id === inherited?.provider && inherited.model ? inherited.model : provider.default_model;

  const pickProvider = (id: string) => {
    setFreeText(false);
    const provider = providers.find((p) => p.id === id);
    onChange(provider === undefined ? NO_AGENT_PICK : { provider: id, model: defaultModelFor(provider) });
  };

  const pickModel = (model: string) => {
    if (model === OTHER) {
      setFreeText(true);
      onChange({ provider: value.provider, model: '' });
      return;
    }
    setFreeText(false);
    onChange({ provider: value.provider, model });
  };

  const inheritedLabel =
    inheritedProvider === undefined ? (
      'Configured default'
    ) : (
      <>
        Configured default (<ProviderIcon kind={inheritedProvider.kind} />{' '}
        {inheritedProvider.display_name}
        {inherited?.model ? ` · ${inherited.model}` : ''})
      </>
    );

  const curated = selected?.models ?? [];
  const custom = freeText || (value.model !== '' && !curated.includes(value.model));

  return (
    <>
      <RichSelectField
        id={`${idPrefix}-provider`}
        label="Inference provider"
        value={value.provider}
        onChange={pickProvider}
        options={[
          { value: '', label: <span className="truncate text-ink-2">{inheritedLabel}</span> },
          ...providers.map((p) => ({ value: p.id, label: describe(p) })),
        ]}
        hint="Which registered provider this run's agent talks to. Leave it alone to resolve through the platform and domain defaults at dispatch."
      />
      {selected !== undefined && (
        <SelectField
          id={`${idPrefix}-model`}
          label="Model"
          value={custom ? OTHER : value.model}
          onChange={pickModel}
          options={[
            ...curated.map((model) => ({ value: model, label: model })),
            { value: OTHER, label: 'Other…' },
          ]}
          hint={`Curated for ${selected.display_name}. Its own default is ${selected.default_model}.`}
        />
      )}
      {selected !== undefined && custom && (
        <TextField
          id={`${idPrefix}-model-other`}
          label="Model name"
          mono
          value={value.model}
          onChange={(model) => {
            onChange({ provider: value.provider, model });
          }}
          placeholder={selected.default_model}
          hint="Free text. The provider answers for it, not this form."
        />
      )}
    </>
  );
}
