import { useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
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
import type { BreadcrumbItem } from '../ui';
import {
  FormActions,
  FormError,
  FormGrid,
  Notice,
  NumberInputField,
  TextAreaField,
} from './formControls';

export function ScopeFormPage() {
  const { key: rawKey } = useParams<{ key: string }>();
  const issueKey = rawKey ? decodeURIComponent(rawKey) : '';
  const navigate = useNavigate();

  const whoami = $api.useQuery('get', '/api/whoami');
  const detail = $api.useQuery('get', '/api/issues/{key}', {
    params: { path: { key: issueKey } },
  });

  const scopeMutation = $api.useMutation('post', '/api/issues/{key}/scope');

  const [justification, setJustification] = useState('');
  const [maxCost, setMaxCost] = useState(5.0);
  const [submitError, setSubmitError] = useState<string | null>(null);

  const crumbs: BreadcrumbItem[] = [
    { label: 'Issues', to: '/issues' },
    { label: issueKey, to: `/issues/${encodeURIComponent(issueKey)}` },
    { label: 'Scope' },
  ];

  if (!issueKey) {
    return <Empty title="NO ISSUE KEY" description="This route needs an issue key." />;
  }

  if (detail.isError) {
    return <Empty title="ISSUE UNAVAILABLE" description={formatError(detail.error)} />;
  }

  if (detail.isPending || whoami.isPending) {
    return <LoadingBlock label="LOADING ISSUE" />;
  }

  const isAdmin = whoami.data?.role === 'admin';

  if (!isAdmin) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty
          title="ADMIN ACCESS REQUIRED"
          description="A scope override bypasses budget caps, so it needs an admin session."
        />
      </>
    );
  }

  const isValid = justification.trim().length >= 10 && maxCost > 0;

  const handleSubmit = async () => {
    setSubmitError(null);
    try {
      await scopeMutation.mutateAsync({
        params: { path: { key: issueKey } },
        body: { justification: justification.trim(), max_cost: maxCost },
      });
      void navigate(`/issues/${encodeURIComponent(issueKey)}/scope/progress`);
    } catch (err: unknown) {
      const msg = formatError(err);
      if (msg.toLowerCase().includes('admin') || msg.toLowerCase().includes('403')) {
        setSubmitError('Admin access required. Your session may have expired.');
      } else {
        setSubmitError(msg);
      }
    }
  };

  return (
    <>
      <Breadcrumb items={crumbs} />
      <PageHeader
        eyebrow="Override"
        title={`Scope: ${detail.data.issue.title || issueKey}`}
        description="Run a scope turn on this issue now, ahead of the queue."
      />
      <Notice label="Bypass">
        This triggers a ScopeNow override that <strong>bypasses all budget caps</strong>. Cost books
        to the ledger under your login ({whoami.data?.user || 'unknown'}). The issue is unparked
        automatically if it is currently parked.
      </Notice>

      <Section>
        <SectionHeader title="Scope override" />
        <SectionBody>
          <FormGrid>
            <TextAreaField
              id="scope-justification"
              label="Justification"
              required
              value={justification}
              onChange={setJustification}
              rows={4}
              placeholder="Explain why this issue needs immediate scoping…"
              autoFocus
              hint="Why this issue needs a scope run now (min 10 characters)."
            />

            <NumberInputField
              id="scope-max-cost"
              label="Max cost (USD)"
              value={maxCost}
              onChange={setMaxCost}
              min={0.5}
              step={0.5}
              hint="Maximum spend for this scope turn."
            />

            {submitError !== null && <FormError>{submitError}</FormError>}
          </FormGrid>
        </SectionBody>
      </Section>

      <FormActions>
        <Button
          variant="filled"
          onClick={() => void handleSubmit()}
          disabled={!isValid || scopeMutation.isPending}
        >
          {scopeMutation.isPending ? 'SUBMITTING…' : 'SUBMIT SCOPE OVERRIDE'}
        </Button>
        <Button render={<Link to={`/issues/${encodeURIComponent(issueKey)}`} />}>CANCEL</Button>
      </FormActions>
    </>
  );
}
