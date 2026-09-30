import { useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import {
  Breadcrumb,
  Button,
  Empty,
  formatStamp,
  LoadingBlock,
  Mono,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
  Spec,
  Status,
} from '../ui';
import { FormError, MetaRow, Notice } from './formControls';
import { CopyButton, SecretOnce } from './SecretOnce';
import {
  bodyText,
  outcomeTone,
  STATE_TONE,
  webhookView,
  type WebhookDeliveryDto,
} from './webhooksView';

const PAGE = 25;

interface DeliveryPageProps {
  webhookId: string;
  before: string | null;
  /// Set on the oldest page loaded so far: asks for the page after this one's last delivery.
  onOlder: ((before: string) => void) | null;
}

/// One page of the delivery log, newest first, starting after `before`.
function DeliveryPage({ webhookId, before, onOlder }: DeliveryPageProps) {
  const page = $api.useQuery('get', '/api/webhooks/{id}/deliveries', {
    params: {
      path: { id: webhookId },
      query: before === null ? { limit: PAGE } : { limit: PAGE, before },
    },
  });
  if (page.isPending) return <LoadingBlock label="Loading deliveries" />;
  if (page.isError) return <FormError>{formatError(page.error)}</FormError>;
  const last = page.data[page.data.length - 1];
  return (
    <>
      {before === null && page.data.length === 0 ? (
        <Notice label="Empty">No delivery recorded yet. Post one to the address above.</Notice>
      ) : null}
      {page.data.map((delivery) => (
        <DeliveryRow key={delivery.id} delivery={delivery} />
      ))}
      {onOlder !== null && last !== undefined && page.data.length === PAGE ? (
        <div className="px-4.5 py-3">
          <Button variant="quiet" className="uppercase" onClick={() => onOlder(last.id)}>
            Load older
          </Button>
        </div>
      ) : null}
    </>
  );
}

function DeliveryRow({ delivery }: { delivery: WebhookDeliveryDto }) {
  return (
    <details className="border-b border-rule">
      <summary className="flex cursor-pointer flex-wrap items-center gap-3 px-4.5 py-2">
        <Status status={delivery.outcome} tone={outcomeTone(delivery.outcome)} pulse={delivery.outcome === 'pending'} />
        <Mono size="data" tone="ink-2">
          {formatStamp(delivery.received_at)}
        </Mono>
        {delivery.launch_key ? (
          <Link
            className="font-mono text-data text-blue"
            to={`/playbook-runs/${encodeURIComponent(delivery.launch_key)}`}
          >
            {delivery.launch_key}
          </Link>
        ) : null}
        {delivery.reason ? (
          <Mono size="data" tone="ink-3">
            {delivery.reason}
          </Mono>
        ) : null}
      </summary>
      <div className="grid gap-2 px-4.5 pb-3">
        <MetaRow label="Delivery">
          <Mono size="data">{delivery.id}</Mono>
        </MetaRow>
        {delivery.dedupe_key ? (
          <MetaRow label="Dedupe key">
            <Mono size="data">{delivery.dedupe_key}</Mono>
          </MetaRow>
        ) : null}
        <MetaRow label="Headers">
          <pre className="m-0 overflow-x-auto font-mono text-data text-ink-2">
            {JSON.stringify(delivery.headers, null, 2)}
          </pre>
        </MetaRow>
        <MetaRow label="Body">
          <pre className="m-0 max-h-96 overflow-auto font-mono text-data text-ink-2">{bodyText(delivery)}</pre>
        </MetaRow>
      </div>
    </details>
  );
}

/// One webhook: how a delivery proves itself, what it becomes, the controls an owner has over it,
/// and every delivery it recorded.
export function WebhookDetailPage() {
  const { id = '' } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const webhook = $api.useQuery('get', '/api/webhooks/{id}', { params: { path: { id } } });
  const setEnabled = $api.useMutation('post', '/api/webhooks/{id}/enabled');
  const rotate = $api.useMutation('post', '/api/webhooks/{id}/secret');
  const remove = $api.useMutation('delete', '/api/webhooks/{id}');
  const [secret, setSecret] = useState<{ secret: string; url: string | null } | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [cursors, setCursors] = useState<(string | null)[]>([null]);

  const crumbs = [{ label: 'Webhooks', to: '/webhooks' }, { label: id.slice(0, 13) }];
  if (webhook.isPending) return <LoadingBlock label="Loading webhook" />;
  if (webhook.isError) {
    return (
      <>
        <Breadcrumb items={crumbs} />
        <Empty title="No such webhook" description={formatError(webhook.error)} />
      </>
    );
  }
  const w = webhook.data;
  const view = webhookView(w);
  const refresh = () => {
    void queryClient.invalidateQueries({ queryKey: ['get', '/api/webhooks/{id}'] });
    void queryClient.invalidateQueries({ queryKey: ['get', '/api/webhooks/{id}/deliveries'] });
  };

  const toggle = async () => {
    setError(null);
    try {
      await setEnabled.mutateAsync({ params: { path: { id } }, body: { enabled: !w.enabled } });
      refresh();
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const rotateSecret = async () => {
    setError(null);
    try {
      const rotated = await rotate.mutateAsync({ params: { path: { id } } });
      setSecret({ secret: rotated.secret, url: rotated.delivery_url ?? null });
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const destroy = async () => {
    if (!confirmDelete) {
      setConfirmDelete(true);
      return;
    }
    setError(null);
    try {
      await remove.mutateAsync({ params: { path: { id } } });
      void queryClient.invalidateQueries({ queryKey: ['get', '/api/webhooks'] });
      void navigate('/webhooks');
    } catch (err: unknown) {
      setError(formatError(err));
    }
  };

  const derive = Object.entries(w.derive ?? {});
  const fixed = Object.entries(w.params ?? {});
  const loadOlder = (before: string) => {
    setCursors((previous) => (previous.includes(before) ? previous : [...previous, before]));
  };

  return (
    <>
      <Breadcrumb items={crumbs} />
      <PageHeader
        eyebrow="Webhook"
        title={w.playbook}
        description={view.detail}
        actions={<Status status={view.headline} tone={STATE_TONE[view.state]} pulse={false} />}
      />

      {secret !== null && <SecretOnce secret={secret.secret} url={secret.url} path={w.delivery_path} verifier={w.verifier} />}
      {error !== null && <FormError className="mx-4.5 my-3">{error}</FormError>}

      <Section>
        <SectionHeader
          title="Delivery"
          actions={
            <div className="flex gap-2">
              <Button
                variant="quiet"
                className="uppercase"
                render={<Link to={`/webhooks/${encodeURIComponent(id)}/edit`} />}
              >
                Edit
              </Button>
              <Button variant="quiet" className="uppercase" onClick={() => void toggle()} disabled={setEnabled.isPending}>
                {w.enabled ? 'Disable' : 'Enable'}
              </Button>
              <Button variant="quiet" className="uppercase" onClick={() => void rotateSecret()} disabled={rotate.isPending}>
                Rotate secret
              </Button>
              <Button variant="quiet" className="uppercase text-red" onClick={() => void destroy()} disabled={remove.isPending}>
                {confirmDelete ? 'Confirm delete' : 'Delete'}
              </Button>
            </div>
          }
        />
        <SectionBody>
          <Spec
            className="mb-3"
            items={[
              { label: 'Verifier', value: w.verifier },
              { label: 'Rate', value: w.max_launches_per_hour, note: '/h' },
              { label: 'Retention', value: w.retention_days, note: 'days' },
              { label: 'Fails', value: w.consecutive_failures, tone: w.consecutive_failures > 0 ? 'bad' : 'default' },
            ]}
          />
          <div className="grid gap-2">
            <MetaRow label="Delivery URL">
              <span className="flex items-center gap-2">
                <Mono size="data" className="break-all">
                  {w.delivery_url ?? w.delivery_path}
                  {w.verifier === 'path_token' ? '/<token>' : ''}
                </Mono>
                {w.verifier === 'hmac_sha256' ? <CopyButton text={w.delivery_url ?? w.delivery_path} /> : null}
              </span>
            </MetaRow>
            {w.verifier === 'path_token' ? (
              <Mono size="data" tone="ink-3">
                The token was shown once, at creation. Rotate the secret to get a new URL.
              </Mono>
            ) : null}
            {w.header ? (
              <MetaRow label="Signature header">
                <Mono size="data">{w.header}</Mono>
              </MetaRow>
            ) : null}
            <MetaRow label="Playbook">
              <Link className="font-mono text-data text-blue" to={`/playbooks/${encodeURIComponent(w.playbook)}`}>
                {w.playbook}
              </Link>
              <Mono size="data" tone="ink-3" className="ml-2">
                @ {(w.adopted_rev ?? '').slice(0, 12)}
              </Mono>
            </MetaRow>
            <MetaRow label="Owner">
              <Mono size="data">{w.owner_principal ?? '—'}</Mono>
            </MetaRow>
          </div>
        </SectionBody>
      </Section>

      <Section>
        <SectionHeader title="Transform" note="CEL over body, headers, delivery, and received_at" />
        <SectionBody>
          <div className="grid gap-2">
            <MetaRow label="Filter">
              <Mono size="data">{w.filter}</Mono>
            </MetaRow>
            <MetaRow label="Dedupe">
              <Mono size="data">{w.dedupe}</Mono>
            </MetaRow>
            {derive.map(([name, expression]) => (
              <MetaRow key={name} label={name}>
                <Mono size="data">{String(expression)}</Mono>
              </MetaRow>
            ))}
            {fixed.map(([name, value]) => (
              <MetaRow key={name} label={name}>
                <Mono size="data" tone="ink-2">
                  {String(value)} (fixed)
                </Mono>
              </MetaRow>
            ))}
          </div>
        </SectionBody>
      </Section>

      <Section>
        <SectionHeader title="Deliveries" note="newest first" />
        {cursors.map((before, index) => (
          <DeliveryPage
            key={before ?? 'newest'}
            webhookId={id}
            before={before}
            onOlder={index === cursors.length - 1 ? loadOlder : null}
          />
        ))}
      </Section>
    </>
  );
}
