import { Section, SectionBody, SectionHeader } from '../ui';
import { MarkdownView } from './MarkdownView';
import type { components } from '../api/schema.d';

type ScenarioDetailDto = components['schemas']['ScenarioDetailDto'];

/// A scenario's free-text goal, per-kind hiding's counterpart to [`IssueBodySection`]: no GitHub
/// labels/links, just the problem framing plus who adopted it and its affected-repos hint list
/// (the first entry is the actual clone target, the rest are framing only).
export function ScenarioDetailSection({ scenario }: { scenario: ScenarioDetailDto }) {
  return (
    <Section>
      <SectionHeader
        title="Scenario"
        note={scenario.authoritative ? 'authoritative brief' : undefined}
      />
      <SectionBody>
        <MarkdownView markdown={scenario.body} />
        <div className="mt-3.5 font-mono text-data text-ink-3">
          adopted by {scenario.created_by} for{' '}
          {scenario.affected_repos.map((repo, i) => (
            <span key={repo}>
              {i > 0 && ', '}
              <span className="text-ink-2">{repo}</span>
            </span>
          ))}
        </div>
      </SectionBody>
    </Section>
  );
}
