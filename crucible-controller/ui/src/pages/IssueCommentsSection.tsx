import { useState } from 'react';
import { Button, Mono, Section, SectionBody, SectionHeader } from '../ui';
import type { components } from '../api/schema';
import { absoluteTime, relativeTime } from './journeyView';
import { MarkdownView } from './MarkdownView';

type IssueCommentDto = components['schemas']['IssueCommentDto'];

/// The mirrored upstream comment thread, collapsed by default (the RunDetail toggle pattern).
/// Renders nothing when the issue has no comments — an empty thread needs no chrome.
export function IssueCommentsSection({ comments }: { comments: IssueCommentDto[] }) {
  const [open, setOpen] = useState(false);

  if (comments.length === 0) return null;

  return (
    <Section>
      <SectionHeader
        title="Comments"
        note={`${comments.length} mirrored`}
        actions={
          <Button
            onClick={() => {
              setOpen((v) => !v);
            }}
          >
            {open ? 'HIDE' : 'SHOW'}
          </Button>
        }
      />
      {open && (
        <SectionBody padded={false}>
          {comments.map((comment) => (
            <Comment key={comment.id} comment={comment} />
          ))}
        </SectionBody>
      )}
    </Section>
  );
}

function Comment({ comment }: { comment: IssueCommentDto }) {
  const rel = relativeTime(comment.created_at);
  const abs = absoluteTime(comment.created_at);

  return (
    <article className="border-b border-rule px-4.5 py-3 last:border-b-0">
      <div className="mb-1.5 flex items-baseline gap-3">
        <Mono weight="semibold" tone="ink">
          {comment.author ?? 'ghost'}
        </Mono>
        {rel && (
          <Mono size="label" tone="ink-3" title={abs ?? undefined}>
            {rel}
          </Mono>
        )}
      </div>
      <MarkdownView markdown={comment.body} />
    </article>
  );
}
