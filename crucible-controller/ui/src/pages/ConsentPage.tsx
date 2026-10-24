import { useSearchParams } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { Button, Empty, LoadingBlock, PageHeader, Section, SectionBody, SectionHeader } from '../ui';
import { ClusterIdentityRow } from './ClusterIdentityRow';
import { linkOutcome } from './clusterIdentitiesView';
import { consentPath, consentView, type ConsentDto } from './consentView';

function Consent({ consent }: { consent: ConsentDto }) {
  const decide = $api.useMutation('post', '/api/oauth/consent');
  const [search] = useSearchParams();
  const outcome = linkOutcome(search);
  const view = consentView(consent);
  const answer = (decision: 'approve' | 'deny') => {
    decide.mutate(
      { body: { id: consent.id, decision } },
      { onSuccess: (result) => window.location.assign(result.redirect_url) },
    );
  };

  return (
    <>
      <PageHeader
        eyebrow="Sign-in request"
        title={view.title}
        description="A tool on your machine asks for a token to call this MCP server as you."
      />
      <Section>
        <SectionHeader title="Request" />
        <SectionBody>
          <dl data-testid="consent-request" className="m-0 grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1.5 text-body">
            <dt className="text-ink-3">Client</dt>
            <dd className="m-0 text-ink-1">
              <span data-testid="consent-client">{view.client}</span>
              <span className="block font-mono text-data text-ink-3">{consent.client_id}</span>
              {view.publisher !== null && (
                <span data-testid="consent-publisher" className="block text-ink-2">
                  {view.publisher}
                </span>
              )}
            </dd>
            <dt className="text-ink-3">Returns to</dt>
            <dd data-testid="consent-redirect" className="m-0 font-mono text-data text-ink-1">
              {consent.redirect_origin}
            </dd>
            <dt className="text-ink-3">Server</dt>
            <dd className="m-0 font-mono text-data text-ink-1">{consent.resource}</dd>
            <dt className="text-ink-3">Scopes</dt>
            <dd className="m-0 font-mono text-data text-ink-1">{consent.scopes.join(' ')}</dd>
            <dt className="text-ink-3">As</dt>
            <dd className="m-0 font-mono text-data text-ink-1">{consent.login}</dd>
          </dl>
        </SectionBody>
      </Section>
      <Section>
        <SectionHeader
          title="On the cluster"
          note="the server checks every call against this username's Kubernetes permissions"
        />
        <SectionBody>
          {outcome !== null && (
            <p
              data-testid="cluster-link-outcome"
              className={outcome.ok ? 'text-body text-ink-2' : 'text-body text-red'}
            >
              {outcome.ok ? `Linked ${outcome.cluster}.` : outcome.message}
            </p>
          )}
          <ClusterIdentityRow
            identity={consent.identity}
            readOnly={false}
            returnTo={consentPath(consent.id)}
          />
        </SectionBody>
      </Section>
      {view.blocked !== null && (
        <p data-testid="consent-blocked" className="mt-3 text-body text-ink-2">
          {view.blocked}
        </p>
      )}
      {decide.isError && <p className="mt-3 text-body text-red">{formatError(decide.error)}</p>}
      <div className="mt-4 flex gap-2">
        <Button
          className="uppercase"
          variant="filled"
          disabled={!view.canApprove || decide.isPending}
          onClick={() => answer('approve')}
        >
          {decide.isPending ? 'Allowing…' : 'Allow'}
        </Button>
        <Button className="uppercase" variant="quiet" disabled={decide.isPending} onClick={() => answer('deny')}>
          Deny
        </Button>
      </div>
    </>
  );
}

export function ConsentPage() {
  const [search] = useSearchParams();
  const id = search.get('id') ?? '';
  const consent = $api.useQuery(
    'get',
    '/api/oauth/consent',
    { params: { query: { id } } },
    { retry: false },
  );
  if (consent.isError) {
    return <Empty title="No sign-in request" description={formatError(consent.error)} />;
  }
  if (consent.isPending) return <LoadingBlock label="Loading the sign-in request" />;
  return <Consent consent={consent.data} />;
}
