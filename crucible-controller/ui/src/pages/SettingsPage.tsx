import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import { viaLabel } from '../ownerContext';
import { Link } from 'react-router-dom';
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
import { TextField, NumberInputField, CheckField, FormActions } from './formControls';

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
    return <Empty title="CREDENTIAL UNAVAILABLE" description={formatError(credential.error)} />;
  }
  if (credential.isPending) {
    return <LoadingBlock label="LOADING CREDENTIAL" />;
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
            {revoke.isPending ? 'REVOKING…' : 'REVOKE'}
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
        variant="quiet"
        disabled={!view.revocable || revoke.isPending}
        onClick={() => {
          revoke.mutate(
            { params: { path: { id: apiKey.id } } },
            { onSuccess: () => void qc.invalidateQueries() },
          );
        }}
      >
        {revoke.isPending ? 'REVOKING…' : 'REVOKE'}
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
    return <Empty title="API KEYS UNAVAILABLE" description={formatError(keys.error)} />;
  }
  if (keys.isPending) {
    return <LoadingBlock label="LOADING API KEYS" />;
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
              {mint.isPending ? 'MINTING…' : 'MINT KEY'}
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

function Memberships() {
  const whoami = $api.useQuery('get', '/api/whoami');
  if (whoami.isError) {
    return <Empty title="MEMBERSHIPS UNAVAILABLE" description={formatError(whoami.error)} />;
  }
  if (whoami.isPending) {
    return <LoadingBlock label="LOADING MEMBERSHIPS" />;
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
      <OfflineCredential />
      <ApiKeys />
    </>
  );
}
