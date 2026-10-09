import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { viaLabel } from '../ownerContext';
import { Link, useSearchParams } from 'react-router-dom';
import {
  Button,
  Empty,
  LoadingBlock,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
  Status,
} from '../ui';
import type { StatusTone } from '../ui';
import {
  credentialView,
  keyView,
  mcpInstall,
  revokedMessage,
  type ApiKeyDto,
  type CredentialState,
  type KeyState,
} from './settingsView';
import { TextField, NumberInputField, CheckField, FormActions, BareTextInput } from './formControls';
import {
  identityView,
  linkOutcome,
  sortIdentities,
  type ClusterIdentityDto,
  type IdentityState,
} from './clusterIdentitiesView';

const TONE: Record<CredentialState, StatusTone> = {
  absent: 'grey',
  failing: 'amber',
  live: 'green',
};

const KEY_TONE: Record<KeyState, StatusTone> = {
  live: 'green',
  expired: 'amber',
  revoked: 'grey',
};

function OfflineCredential() {
  const qc = useQueryClient();
  const credential = $api.useQuery('get', '/api/credentials/me');
  const revoke = $api.useMutation('delete', '/api/credentials/me');
  const [outcome, setOutcome] = useState<string | null>(null);

  if (credential.isError) {
    return <Empty title="Credential unavailable" description={formatError(credential.error)} />;
  }
  if (credential.isPending) {
    return <LoadingBlock label="Loading credential" />;
  }
  const view = credentialView(credential.data);

  return (
    <Section>
      <SectionHeader
        title="Offline credential"
        note="what your scheduled launches re-check team membership with"
        actions={<Status status={view.headline} tone={TONE[view.state]} pulse={false} />}
      />
      <SectionBody>
        <p className="text-body text-ink-2">{view.detail}</p>
        <p className="mt-2 text-body text-ink-3">
          Revoking stops every schedule of yours whose playbook binds a secret. They start again
          when you sign in.
        </p>
        {outcome !== null && <p className="mt-2 text-body text-ink-2">{outcome}</p>}
        {revoke.isError && (
          <p className="mt-2 text-body text-red">{formatError(revoke.error)}</p>
        )}
        <div className="mt-3">
          <Button
            className="uppercase"
            variant="filled"
            disabled={!view.revocable || revoke.isPending}
            onClick={() => {
              revoke.mutate(
                {},
                {
                  onSuccess: (result) => {
                    setOutcome(revokedMessage(result.revoked, result.schedules_parked));
                    void qc.invalidateQueries();
                  },
                },
              );
            }}
          >
            {revoke.isPending ? 'Revoking…' : 'Revoke'}
          </Button>
        </div>
      </SectionBody>
    </Section>
  );
}

/// One key's row: what it is doing, and the control that stops it.
function KeyRow({ apiKey }: { apiKey: ApiKeyDto }) {
  const qc = useQueryClient();
  const revoke = $api.useMutation('delete', '/api/keys/{id}');
  const view = keyView(apiKey);
  return (
    <div className="flex items-start justify-between gap-4 border-t border-line py-3">
      <div>
        <div className="flex items-center gap-2">
          <span className="text-body text-ink-1">{apiKey.name}</span>
          <Status status={view.headline} tone={KEY_TONE[view.state]} pulse={false} />
        </div>
        <p className="mt-1 text-body text-ink-3">{view.detail}</p>
        {revoke.isError && <p className="mt-1 text-body text-red">{formatError(revoke.error)}</p>}
      </div>
      <Button
        className="uppercase"
        variant="quiet"
        disabled={!view.revocable || revoke.isPending}
        onClick={() => {
          revoke.mutate(
            { params: { path: { id: apiKey.id } } },
            { onSuccess: () => void qc.invalidateQueries() },
          );
        }}
      >
        {revoke.isPending ? 'Revoking…' : 'Revoke'}
      </Button>
    </div>
  );
}

/// Mint a key, and show it once. The secret is never returned again, so the panel keeps it on
/// screen until the page is left rather than tucking it behind a toast that can be missed.
function ApiKeys() {
  const qc = useQueryClient();
  const keys = $api.useQuery('get', '/api/keys');
  const mint = $api.useMutation('post', '/api/keys');
  const [name, setName] = useState('');
  const [days, setDays] = useState(90);
  const [forever, setForever] = useState(false);
  const [minted, setMinted] = useState<{ secret: string; url: string | null } | null>(null);

  if (keys.isError) {
    return <Empty title="API keys unavailable" description={formatError(keys.error)} />;
  }
  if (keys.isPending) {
    return <LoadingBlock label="Loading API keys" />;
  }

  return (
    <Section>
      <SectionHeader
        title="API keys"
        note="what an agent authenticates as when it talks to this controller"
      />
      <SectionBody>
        <p className="text-body text-ink-2">
          A key acts as you: your groups, your roles, everything you can reach in this page. It is
          shown once, when it is minted. Revoking one stops whatever is holding it, immediately.
        </p>

        {minted !== null && (
          <div className="mt-3 border border-line p-3">
            <p className="text-body text-ink-1">Copy this now. It is not shown again.</p>
            <pre className="mt-2 overflow-x-auto text-body text-ink-2">{minted.secret}</pre>
            {minted.url !== null && (
              <>
                <p className="mt-3 text-body text-ink-3">Point an MCP client at it:</p>
                <pre className="mt-1 overflow-x-auto text-body text-ink-2">
                  {mcpInstall(minted.secret, minted.url)}
                </pre>
              </>
            )}
          </div>
        )}

        <div className="mt-4">
          <TextField
            id="api-key-name"
            label="Name"
            value={name}
            onChange={setName}
            hint="What this key is for, so you can tell two of them apart later."
          />
          <CheckField
            id="api-key-forever"
            label="Never expires"
            checked={forever}
            onChange={setForever}
            description="Off by default. A key that never expires is one you have to remember to revoke."
          />
          {!forever && (
            <NumberInputField
              id="api-key-days"
              label="Expires in (days)"
              value={days}
              onChange={setDays}
            />
          )}
          {mint.isError && <p className="mt-2 text-body text-red">{formatError(mint.error)}</p>}
          <FormActions>
            <Button
              className="uppercase"
              variant="filled"
              disabled={name.trim() === '' || mint.isPending}
              onClick={() => {
                mint.mutate(
                  { body: { name: name.trim(), expires_in_days: forever ? null : days } },
                  {
                    onSuccess: (result) => {
                      setMinted({ secret: result.secret, url: result.mcp_url ?? null });
                      setName('');
                      void qc.invalidateQueries();
                    },
                  },
                );
              }}
            >
              {mint.isPending ? 'Minting…' : 'Mint key'}
            </Button>
          </FormActions>
        </div>

        {keys.data.length === 0 ? (
          <p className="mt-4 text-body text-ink-3">You hold no API keys.</p>
        ) : (
          <div className="mt-4">
            {keys.data.map((apiKey) => (
              <KeyRow key={apiKey.id} apiKey={apiKey} />
            ))}
          </div>
        )}
      </SectionBody>
    </Section>
  );
}

const IDENTITY_TONE: Record<IdentityState, StatusTone> = {
  resolved: 'green',
  linked: 'green',
  unlinked: 'amber',
  override: 'grey',
};

/// One cluster: who the person is there, and, on a link cluster, the controls that prove or forget
/// it. A pasted token lives only in this row's state until it is sent, and is cleared either way.
function IdentityRow({ identity, readOnly }: { identity: ClusterIdentityDto; readOnly: boolean }) {
  const qc = useQueryClient();
  const start = $api.useMutation('post', '/api/me/cluster-identities/{cluster}/link');
  const paste = $api.useMutation('put', '/api/me/cluster-identities/{cluster}');
  const unlink = $api.useMutation('delete', '/api/me/cluster-identities/{cluster}');
  const [pasting, setPasting] = useState(false);
  const [token, setToken] = useState('');
  const view = identityView(identity);
  const path = { params: { path: { cluster: identity.cluster } } };
  const failure = start.error ?? paste.error ?? unlink.error;
  const busy = start.isPending || paste.isPending || unlink.isPending;

  return (
    <div
      data-testid={`cluster-identity-${identity.cluster}`}
      className="flex items-start justify-between gap-4 border-t border-line py-3"
    >
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-mono text-data text-ink-1">{identity.cluster}</span>
          <Status status={view.headline} tone={IDENTITY_TONE[view.state]} pulse={false} />
          <span className="text-body text-ink-3">{view.rule}</span>
        </div>
        <p className="mt-1 font-mono text-data text-ink-2" data-testid="kube-user">
          {view.username}
        </p>
        <p className="mt-1 text-body text-ink-3">{view.detail}</p>
        {pasting && (
          <form
            className="mt-2 flex items-center gap-2"
            onSubmit={(event) => {
              event.preventDefault();
              const sent = token.trim();
              setToken('');
              paste.mutate(
                { ...path, body: { cluster_token: sent } },
                {
                  onSuccess: () => {
                    setPasting(false);
                    void qc.invalidateQueries();
                  },
                },
              );
            }}
          >
            <BareTextInput
              id={`cluster-token-${identity.cluster}`}
              type="password"
              mono
              value={token}
              onChange={setToken}
              placeholder="oc whoami -t, or an OIDC ID token"
              aria-label={`Token for ${identity.cluster}`}
            />
            <Button type="submit" variant="filled" className="uppercase" disabled={token.trim() === '' || busy}>
              {paste.isPending ? 'Checking…' : 'Link'}
            </Button>
          </form>
        )}
        {failure && <p className="mt-1 text-body text-red">{formatError(failure)}</p>}
      </div>
      {!readOnly && (
        <div className="flex shrink-0 gap-2">
          {view.link?.method === 'oauth' && (
            <Button
              className="uppercase"
              variant="filled"
              disabled={busy}
              onClick={() => {
                start.mutate(path, {
                  onSuccess: (result) => {
                    window.location.assign(result.authorize_url);
                  },
                });
              }}
            >
              {start.isPending ? 'Opening…' : `${view.link.label} ${identity.cluster}`}
            </Button>
          )}
          {view.link?.method === 'paste' && !pasting && (
            <Button className="uppercase" variant="filled" disabled={busy} onClick={() => setPasting(true)}>
              {`${view.link.label} ${identity.cluster}`}
            </Button>
          )}
          {view.unlinkable && (
            <Button
              className="uppercase"
              variant="quiet"
              disabled={busy}
              onClick={() => {
                unlink.mutate(path, { onSuccess: () => void qc.invalidateQueries() });
              }}
            >
              {unlink.isPending ? 'Unlinking…' : 'Unlink'}
            </Button>
          )}
        </div>
      )}
    </div>
  );
}

function ClusterIdentities() {
  const identities = $api.useQuery('get', '/api/me/cluster-identities');
  const whoami = $api.useQuery('get', '/api/whoami');
  const [search] = useSearchParams();
  const outcome = linkOutcome(search);

  if (identities.isError) {
    return <Empty title="Cluster identities unavailable" description={formatError(identities.error)} />;
  }
  if (identities.isPending) {
    return <LoadingBlock label="Loading cluster identities" />;
  }
  const readOnly = Boolean(whoami.data?.impersonation);
  return (
    <Section>
      <SectionHeader
        title="Cluster identities"
        note="who you are on each cluster, for tools that check your Kubernetes permissions there"
      />
      <SectionBody>
        <p className="text-body text-ink-2">
          MCP servers act on a cluster as your username there, limited by its RBAC. Most clusters
          take it from your SSO sign-in; the rest you link once by signing in to the cluster.
        </p>
        {outcome !== null && (
          <p
            data-testid="cluster-link-outcome"
            className={outcome.ok ? 'mt-2 text-body text-ink-2' : 'mt-2 text-body text-red'}
          >
            {outcome.ok ? `Linked ${outcome.cluster}.` : outcome.message}
          </p>
        )}
        <div className="mt-4">
          {sortIdentities(identities.data).map((identity) => (
            <IdentityRow key={identity.cluster} identity={identity} readOnly={readOnly} />
          ))}
        </div>
      </SectionBody>
    </Section>
  );
}

function Memberships() {
  const whoami = $api.useQuery('get', '/api/whoami');
  if (whoami.isError) {
    return <Empty title="Memberships unavailable" description={formatError(whoami.error)} />;
  }
  if (whoami.isPending) {
    return <LoadingBlock label="Loading memberships" />;
  }
  const teams = [...whoami.data.teams].sort((a, b) => a.team.localeCompare(b.team));
  return (
    <Section>
      <SectionHeader title="Memberships" note="the teams you act in, at the role held, and how" />
      <SectionBody>
        {teams.length === 0 ? (
          <p className="m-0 text-body text-ink-3">You are in no team.</p>
        ) : (
          <dl data-testid="memberships" className="m-0 grid grid-cols-[max-content_max-content_1fr] gap-x-6 gap-y-1.5 font-mono text-data">
            {teams.map((membership) => (
              <div key={membership.team} className="contents">
                <dt className="text-ink">
                  <Link to={`/teams/${encodeURIComponent(membership.team)}`} className="hover:underline">
                    team:{membership.team}
                  </Link>
                </dt>
                <dd className="m-0 text-ink-2">{membership.role}</dd>
                <dd className="m-0 text-ink-3">{membership.via.map(viaLabel).join(', ')}</dd>
              </div>
            ))}
          </dl>
        )}
      </SectionBody>
    </Section>
  );
}

export function SettingsPage() {
  return (
    <>
      <PageHeader
        eyebrow="You"
        title="Settings"
        description="The credentials this controller holds on your behalf, and what stops working without them."
      />
      <Memberships />
      <ClusterIdentities />
      <OfflineCredential />
      <ApiKeys />
    </>
  );
}
