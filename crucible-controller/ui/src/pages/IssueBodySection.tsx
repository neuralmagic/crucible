import { Mono, Section, SectionBody, SectionHeader } from '../ui';
import { MarkdownView } from './MarkdownView';

/// The upstream issue's own content: label chips + the body rendered as GitHub-flavored markdown.
/// A null body (not yet backfilled by discovery, or genuinely empty upstream) gets a quiet
/// placeholder so the section reads intentionally rather than looking broken.
export function IssueBodySection({ body, labels }: { body: string | null | undefined; labels: string[] }) {
  return (
    <Section>
      <SectionHeader title="Description" />
      <SectionBody>
        {labels.length > 0 && (
          <div className="mb-3 flex flex-wrap gap-1">
            {labels.map((label) => (
              <Mono key={label} size="micro" className="border border-rule-hard px-1 py-px">
                {label}
              </Mono>
            ))}
          </div>
        )}
        {body ? (
          <MarkdownView markdown={body} />
        ) : (
          <Mono size="label" tone="ink-3" uppercase>
            No description provided
          </Mono>
        )}
      </SectionBody>
    </Section>
  );
}
