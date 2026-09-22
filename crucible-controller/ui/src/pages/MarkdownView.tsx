import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import styles from './issueContent.module.css';

/// GitHub-flavored markdown, sanitized by default: react-markdown never injects raw HTML unless
/// rehype-raw is added — and it deliberately is not. Shared by the issue body and each comment.
export function MarkdownView({ markdown }: { markdown: string }) {
  return (
    <div className={styles.markdown}>
      <ReactMarkdown remarkPlugins={[remarkGfm]}>{markdown}</ReactMarkdown>
    </div>
  );
}
