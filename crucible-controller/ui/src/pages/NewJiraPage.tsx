import { useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import {
  Breadcrumb,
  Button,
  Empty,
  LoadingBlock,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
} from '../ui';
import {
  CheckField,
  FormActions,
  FormError,
  FormGrid,
  Notice,
  RepoRows,
  TextField,
  useRepoRows,
} from './formControls';

const CRUMBS = [{ label: 'Issues', to: '/issues' }, { label: 'New Jira issue' }];

/// The Jira adopt form: onboard a Jira issue by key. The controller fetches its title/body once,
/// server-side, so the loop never sees Jira creds — the fetched body then rides the same
/// non-upstream scope path as a scenario. Site is optional (derived from the configured Jira base
/// URL when blank). Submits to `POST /api/jira`; the row lands at `new` and skips the ranker's gates
/// (the human adoption is itself the tier/priority authorization).
export function NewJiraPage() {
  const navigate = useNavigate();
  const whoami = $api.useQuery('get', '/api/whoami');
  const adoptMutation = $api.useMutation('post', '/api/jira');

  const [issueKey, setIssueKey] = useState('');
  const [site, setSite] = useState('');
  const { repos: affectedRepos, trimmed: trimmedRepos, onChangeAt, onAdd, onRemove } = useRepoRows();
  const [authoritative, setAuthoritative] = useState(false);
  const [justification, setJustification] = useState('');
  const [submitError, setSubmitError] = useState<string | null>(null);

  if (whoami.isPending) {
    return <LoadingBlock />;
  }

  const isAdmin = whoami.data?.role === 'admin';

  if (!isAdmin) {
    return (
      <>
        <Breadcrumb items={CRUMBS} />
        <Empty
          title="ADMIN ACCESS REQUIRED"
          description="Adopting a Jira issue books to your login, so it needs an admin session."
        />
      </>
    );
  }

  const isValid =
    issueKey.trim().length > 0 &&
    trimmedRepos.length > 0 &&
    affectedRepos[0].trim().length > 0 &&
    justification.trim().length >= 10;

  const handleSubmit = async () => {
    setSubmitError(null);
    try {
      const ack = await adoptMutation.mutateAsync({
        body: {
          issue_key: issueKey.trim(),
          site: site.trim(),
          affected_repos: trimmedRepos,
          authoritative,
          justification: justification.trim(),
        },
      });
      void navigate(`/issues/${encodeURIComponent(ack.key)}`);
    } catch (err: unknown) {
      setSubmitError(formatError(err));
    }
  };

  return (
    <>
      <Breadcrumb items={CRUMBS} />
      <PageHeader
        eyebrow="Queue"
        title="Adopt a Jira issue"
        description="Onboard an issue by key. Its title and description are fetched server-side, so the loop never sees Jira credentials."
      />
      <Notice label="Bypass">
        This fetches the Jira issue's title and description server-side and creates a work item from
        it. It skips the usual prioritization checks. Starting it here is the approval, recorded in
        the audit log under your account ({whoami.data?.user || 'unknown'}).
      </Notice>

      <Section>
        <SectionHeader title="Jira issue" />
        <SectionBody>
          <FormGrid>
            <TextField
              id="jira-issue-key"
              label="Jira issue key"
              required
              value={issueKey}
              onChange={setIssueKey}
              placeholder="e.g. ACME-1234"
              mono
              autoFocus
              hint="The PROJECT-NUMBER key. Its title and description are fetched from the configured Jira site."
            />

            <TextField
              id="jira-site"
              label="Site"
              value={site}
              onChange={setSite}
              placeholder="Optional — derived from the Jira base URL when blank"
              mono
            />

            <CheckField
              id="jira-authoritative"
              label="Authoritative brief (inject verbatim, skip de-prescription)"
              checked={authoritative}
              onChange={setAuthoritative}
              description="For an issue that already carries a plan derived from measurement, whose prescriptions have to survive into the run. The frozen gate is what keeps it honest."
            />

            <RepoRows
              idPrefix="jira"
              repos={affectedRepos}
              onChangeAt={onChangeAt}
              onAdd={onAdd}
              onRemove={onRemove}
            />

            <TextField
              id="jira-justification"
              label="Justification"
              required
              value={justification}
              onChange={setJustification}
              placeholder="Why this is worth running now"
              hint="Why this is worth running (recorded in the audit log). At least 10 characters."
            />

            {submitError !== null && <FormError>{submitError}</FormError>}
          </FormGrid>
        </SectionBody>
      </Section>

      <FormActions>
        <Button
          variant="filled"
          onClick={() => void handleSubmit()}
          disabled={!isValid || adoptMutation.isPending}
        >
          {adoptMutation.isPending ? 'ADOPTING…' : 'ADOPT JIRA ISSUE'}
        </Button>
        <Button render={<Link to="/issues" />}>CANCEL</Button>
      </FormActions>
    </>
  );
}
