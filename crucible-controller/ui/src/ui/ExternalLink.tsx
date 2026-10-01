import { cn } from './cn';

/// One external result a task reported, as the API serves it: the url plus what the engine read
/// off its host and path.
export interface ExternalLinkRef {
  url: string;
  provider: string;
  kind: string;
  label: string;
}

interface Mark {
  title: string;
  viewBox: string;
  d: string;
}

const OTHER: Mark = {
  title: 'External link',
  viewBox: '0 0 24 24',
  d: 'M5 4h6v2H6v12h12v-5h2v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zm9 0h6v6h-2V7.41l-7.3 7.3-1.41-1.42L16.59 6H14V4z',
};

// Brand marks, drawn with currentColor so they follow the surrounding text.
// github + gitlab + jira traced from simple-icons (CC0).
const MARKS: Record<string, Mark | undefined> = {
  github: {
    title: 'GitHub',
    viewBox: '0 0 24 24',
    d: 'M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12',
  },
  gitlab: {
    title: 'GitLab',
    viewBox: '0 0 24 24',
    d: 'm23.6004 9.5927-.0337-.0862L20.3.9814a.851.851 0 0 0-.3362-.405.8748.8748 0 0 0-.9997.0539.8748.8748 0 0 0-.29.4399l-2.2055 6.748H7.5375l-2.2057-6.748a.8573.8573 0 0 0-.29-.4412.8748.8748 0 0 0-.9997-.0539.8585.8585 0 0 0-.3362.405L.4332 9.5015l-.0325.0862a6.0657 6.0657 0 0 0 2.0119 7.0105l.0113.0087.03.0213 4.976 3.7264 2.462 1.8633 1.4995 1.1321a1.0085 1.0085 0 0 0 1.2197 0l1.4995-1.1321 2.4619-1.8633 5.0062-3.7489.0125-.01a6.0682 6.0682 0 0 0 2.0094-7.003z',
  },
  jira: {
    title: 'Jira',
    viewBox: '0 0 24 24',
    d: 'M11.571 11.513H0a5.218 5.218 0 0 0 5.232 5.215h2.13v2.057A5.215 5.215 0 0 0 12.575 24V12.518a1.005 1.005 0 0 0-1.005-1.005zm5.723-5.756H5.736a5.215 5.215 0 0 0 5.215 5.214h2.129v2.058a5.218 5.218 0 0 0 5.215 5.214V6.758a1.001 1.001 0 0 0-1.001-1.001zM23.013 0H11.455a5.215 5.215 0 0 0 5.215 5.215h2.129v2.057A5.215 5.215 0 0 0 24 12.483V1.005A1.001 1.001 0 0 0 23.013 0z',
  },
};

function markOf(provider: string): Mark {
  return MARKS[provider] ?? OTHER;
}

export interface ExternalLinkMarkProps {
  provider: string;
  className?: string;
}

export function ExternalLinkMark({ provider, className }: ExternalLinkMarkProps) {
  const mark = markOf(provider);
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

export interface ExternalLinkChipProps {
  link: ExternalLinkRef;
  className?: string;
}

/// The provider's mark and the engine's label, linking out. The url is rendered as an href and a
/// title and nowhere else; the API only ever serves http(s).
export function ExternalLinkChip({ link, className }: ExternalLinkChipProps) {
  return (
    <a
      href={link.url}
      target="_blank"
      rel="noopener noreferrer"
      title={link.url}
      onClick={(event) => {
        event.stopPropagation();
      }}
      className={cn(
        'inline-flex items-center gap-1.5 border-b border-rule-hard font-mono text-data text-ink-2 hover:border-ink hover:text-ink',
        className,
      )}
    >
      <ExternalLinkMark provider={link.provider} />
      <span className="min-w-0 truncate">{link.label}</span>
    </a>
  );
}

/// The first occurrence of each http(s) url, in the order they came in. The API validates every
/// url it serves; this refuses anything else a second time, because React does not.
export function shownLinks(links: ExternalLinkRef[]): ExternalLinkRef[] {
  const seen = new Set<string>();
  return links.filter((link) => {
    if (!/^https?:\/\//i.test(link.url) || seen.has(link.url)) return false;
    seen.add(link.url);
    return true;
  });
}

export interface ExternalLinksProps {
  links: ExternalLinkRef[];
  className?: string;
}

export function ExternalLinks({ links, className }: ExternalLinksProps) {
  const shown = shownLinks(links);
  if (shown.length === 0) return null;
  return (
    <div className={cn('flex flex-wrap items-center gap-x-4 gap-y-1', className)}>
      {shown.map((link) => (
        <ExternalLinkChip key={link.url} link={link} />
      ))}
    </div>
  );
}
