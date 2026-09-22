import { $api } from '../api/client';
import type { components } from '../api/schema';
import { cn } from '../ui';

type ProviderKind = components['schemas']['ProviderKind'];

interface Mark {
  title: string;
  viewBox: string;
  d: string;
}

// Brand marks, drawn with currentColor so they follow the surrounding text.
// anthropic + vertex traced from simple-icons (CC0); openai from its Wikimedia lockup.
const MARKS: Record<ProviderKind, Mark> = {
  custom: {
    title: 'Custom endpoint',
    viewBox: '0 0 24 24',
    // A server rack: the operator's own service, wherever it runs.
    d: 'M3 4a1 1 0 0 1 1-1h16a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V4Zm2 1v2h14V5H5Zm-2 9a1 1 0 0 1 1-1h16a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1v-4Zm2 1v2h14v-2H5Zm1-9.5h2v1H6v-1Zm0 10h2v1H6v-1Z',
  },
  anthropic: {
    title: 'Anthropic',
    viewBox: '0 0 24 24',
    d: 'M17.3041 3.541h-3.6718l6.696 16.918H24Zm-10.6082 0L0 20.459h3.7442l1.3693-3.5527h7.0052l1.3693 3.5528h3.7442L10.5363 3.5409Zm-.3712 10.2232 2.2914-5.9456 2.2914 5.9456Z',
  },
  vertex: {
    title: 'Google Vertex AI',
    viewBox: '0 0 24 24',
    d: 'M12.19 2.38a9.344 9.344 0 0 0-9.234 6.893c.053-.02-.055.013 0 0-3.875 2.551-3.922 8.11-.247 10.941l.006-.007-.007.03a6.717 6.717 0 0 0 4.077 1.356h5.173l.03.03h5.192c6.687.053 9.376-8.605 3.835-12.35a9.365 9.365 0 0 0-2.821-4.552l-.043.043.006-.05A9.344 9.344 0 0 0 12.19 2.38zm-.358 4.146c1.244-.04 2.518.368 3.486 1.15a5.186 5.186 0 0 1 1.862 4.078v.518c3.53-.07 3.53 5.262 0 5.193h-5.193l-.008.009v-.04H6.785a2.59 2.59 0 0 1-1.067-.23h.001a2.597 2.597 0 1 1 3.437-3.437l3.013-3.012A6.747 6.747 0 0 0 8.11 8.24c.018-.01.04-.026.054-.023a5.186 5.186 0 0 1 3.67-1.69z',
  },
  openai: {
    title: 'OpenAI',
    viewBox: '0 0 320 320',
    d: 'm297.06 130.97c7.26-21.79 4.76-45.66-6.85-65.48-17.46-30.4-52.56-46.04-86.84-38.68-15.25-17.18-37.16-26.95-60.13-26.81-35.04-.08-66.13 22.48-76.91 55.82-22.51 4.61-41.94 18.7-53.31 38.67-17.59 30.32-13.58 68.54 9.92 94.54-7.26 21.79-4.76 45.66 6.85 65.48 17.46 30.4 52.56 46.04 86.84 38.68 15.24 17.18 37.16 26.95 60.13 26.8 35.06.09 66.16-22.49 76.94-55.86 22.51-4.61 41.94-18.7 53.31-38.67 17.57-30.32 13.55-68.51-9.94-94.51zm-120.28 168.11c-14.03.02-27.62-4.89-38.39-13.88.49-.26 1.34-.73 1.89-1.07l63.72-36.8c3.26-1.85 5.26-5.32 5.24-9.07v-89.83l26.93 15.55c.29.14.48.42.52.74v74.39c-.04 33.08-26.83 59.9-59.91 59.97zm-128.84-55.03c-7.03-12.14-9.56-26.37-7.15-40.18.47.28 1.3.79 1.89 1.13l63.72 36.8c3.23 1.89 7.23 1.89 10.47 0l77.79-44.92v31.1c.02.32-.13.63-.38.83l-64.41 37.19c-28.69 16.52-65.33 6.7-81.92-21.95zm-16.77-139.09c7-12.16 18.05-21.46 31.21-26.29 0 .55-.03 1.52-.03 2.2v73.61c-.02 3.74 1.98 7.21 5.23 9.06l77.79 44.91-26.93 15.55c-.27.18-.61.21-.91.08l-64.42-37.22c-28.63-16.58-38.45-53.21-21.95-81.89zm221.26 51.49-77.79-44.92 26.93-15.54c.27-.18.61-.21.91-.08l64.42 37.19c28.68 16.57 38.51 53.26 21.94 81.94-7.01 12.14-18.05 21.44-31.2 26.28v-75.81c.03-3.74-1.96-7.2-5.2-9.06zm26.8-40.34c-.47-.29-1.3-.79-1.89-1.13l-63.72-36.8c-3.23-1.89-7.23-1.89-10.47 0l-77.79 44.92v-31.1c-.02-.32.13-.63.38-.83l64.41-37.16c28.69-16.55 65.37-6.7 81.91 22 6.99 12.12 9.52 26.31 7.15 40.1zm-168.51 55.43-26.94-15.55c-.29-.14-.48-.42-.52-.74v-74.39c.02-33.12 26.89-59.96 60.01-59.94 14.01 0 27.57 4.92 38.34 13.88-.49.26-1.33.73-1.89 1.07l-63.72 36.8c-3.26 1.85-5.26 5.31-5.24 9.06l-.04 89.79zm14.63-31.54 34.65-20.01 34.65 20v40.01l-34.65 20-34.65-20z',
  },
};

interface ProviderIconProps {
  kind: ProviderKind;
  className?: string;
}

export function ProviderIcon({ kind, className }: ProviderIconProps) {
  const mark = MARKS[kind];
  return (
    <svg
      viewBox={mark.viewBox}
      role="img"
      aria-label={mark.title}
      fill="currentColor"
      className={cn('inline-block h-[0.875em] w-[0.875em] shrink-0 align-[-0.08em]', className)}
    >
      <title>{mark.title}</title>
      <path d={mark.d} />
    </svg>
  );
}

interface AgentProviderTagProps {
  provider: string;
  model?: string | null;
}

/// "provider · model" with the kind's mark in front, for issue and run detail. The mark is
/// looked up from the registry and quietly absent for a provider deregistered since.
export function AgentProviderTag({ provider, model }: AgentProviderTagProps) {
  const kind = useProviderKind(provider);
  return (
    <>
      {kind !== undefined && (
        <>
          <ProviderIcon kind={kind} />{' '}
        </>
      )}
      {model ? `${provider} · ${model}` : provider}
    </>
  );
}

/// The kind behind a recorded provider id, for surfaces that only have the id (issue and run
/// detail). Undefined while loading or when the provider has since been deregistered.
export function useProviderKind(id: string | null | undefined): ProviderKind | undefined {
  const registry = $api.useQuery(
    'get',
    '/api/config/providers',
    {},
    { enabled: typeof id === 'string' && id !== '' }
  );
  if (typeof id !== 'string' || id === '') return undefined;
  return registry.data?.providers.find((p) => p.id === id)?.kind;
}
