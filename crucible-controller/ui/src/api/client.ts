import createClient from 'openapi-fetch';
import createQueryHook from 'openapi-react-query';
import type { paths } from './schema.d';
import { isSessionExpired, redirectToSignIn } from './session';

const client = createClient<paths>({ baseUrl: '/' });

// Every typed endpoint speaks JSON, so an ok-but-HTML body can only be the sign-in page; catch
// it here (plus plain 401s) so no page ever parses or renders it.
client.use({
  onResponse({ response }) {
    const contentType = response.headers.get('content-type') ?? '';
    if (isSessionExpired(response) || (response.ok && contentType.includes('text/html'))) {
      redirectToSignIn();
      throw new Error('session expired, redirecting to sign-in');
    }
    return undefined;
  },
});

export const $api = createQueryHook(client);

/// The same typed client the hooks are built on, for the rare read that must NOT touch the query
/// cache — fetching the version that overtook a save would otherwise reload the editor's buffers
/// out from under the writer.
export const apiClient = client;

async function rawFetch(url: string): Promise<Response> {
  const res = await fetch(url);
  if (isSessionExpired(res)) {
    redirectToSignIn();
    throw new Error('session expired, redirecting to sign-in');
  }
  return res;
}

/// Fetch a whitelisted run artifact (`session.jsonl` | `RESULTS.md` | `summary.json` | `diffs/<file>`)
/// as raw text. The proxy streams bytes with no JSON schema, so the typed client gives no shape here —
/// a plain text fetch is the honest model. `path` segments are already-safe artifact names; only the
/// run id can hold odd characters, so just it is encoded (the `diffs/` slash must survive).
export async function fetchArtifact(runId: string, path: string): Promise<string> {
  const res = await rawFetch(`/api/runs/${encodeURIComponent(runId)}/artifacts/${path}`);
  if (!res.ok) {
    throw new Error(`artifact ${path}: ${res.status} ${res.statusText}`);
  }
  return res.text();
}

/// One captured run file's content, by the key `/api/runs/{run_id}/files` listed it under. The key
/// holds task brackets and slashes, so every segment is encoded and rejoined.
export async function fetchRunFile(runId: string, key: string): Promise<string> {
  const encodedKey = key.split('/').map(encodeURIComponent).join('/');
  const res = await rawFetch(`/api/runs/${encodeURIComponent(runId)}/files/${encodedKey}`);
  if (!res.ok) {
    throw new Error(`run file ${key}: ${res.status} ${res.statusText}`);
  }
  return res.text();
}

/// One captured run file's bytes, tagged with the media type its name implies. The endpoint serves
/// every capture as an octet stream, so a viewer that renders the bytes as an image applies the type
/// itself rather than leaving the browser to sniff it.
export async function fetchRunFileBlob(
  runId: string,
  key: string,
  mediaType: string,
): Promise<Blob> {
  const encodedKey = key.split('/').map(encodeURIComponent).join('/');
  const res = await rawFetch(`/api/runs/${encodeURIComponent(runId)}/files/${encodedKey}`);
  if (!res.ok) {
    throw new Error(`run file ${key}: ${res.status} ${res.statusText}`);
  }
  return new Blob([await res.arrayBuffer()], { type: mediaType });
}

/// Like fetchArtifact, but a 404 resolves to null: runs published before an artifact existed
/// (e.g. pre-flow-report runs have no flow.html) are an expected state, not an error.
export async function fetchArtifactOrNull(runId: string, path: string): Promise<string | null> {
  const res = await rawFetch(`/api/runs/${encodeURIComponent(runId)}/artifacts/${path}`);
  if (res.status === 404) return null;
  if (!res.ok) {
    throw new Error(`artifact ${path}: ${res.status} ${res.statusText}`);
  }
  return res.text();
}

/// The span-enriched flow fetch's outcome. HTTP failures resolve (never throw): a run without
/// Datadog keys or with a too-old engine is an expected state the page presents inline.
export type FlowEnrichedFetch =
  | { ok: true; html: string }
  | { ok: false; status: number; error: string | null };

/// Fetch the server-rendered span-enriched flow page for a run + Datadog trace id. Raw-text like
/// fetchArtifact (the endpoint serves HTML, not a JSON schema); on a non-2xx the JSON `ErrorBody`
/// message rides back for the page to map into a human message.
export async function fetchFlowEnriched(
  runId: string,
  traceId: string,
): Promise<FlowEnrichedFetch> {
  const res = await rawFetch(
    `/api/runs/${encodeURIComponent(runId)}/flow-enriched?trace_id=${encodeURIComponent(traceId)}`,
  );
  if (res.ok) return { ok: true, html: await res.text() };
  let error: string | null = null;
  try {
    const body: unknown = await res.json();
    if (
      typeof body === 'object' &&
      body !== null &&
      'error' in body &&
      typeof body.error === 'string'
    ) {
      error = body.error;
    }
  } catch {
    error = null;
  }
  return { ok: false, status: res.status, error };
}

/// Fetch the latest scope turn's preserved agent transcript (session NDJSON) for an issue, or
/// `null` when none was recorded (404 — a pre-transcript scope, or a turn that never streamed).
/// Same honest plain-text model as fetchArtifact: the endpoint serves NDJSON, not a JSON schema.
export async function fetchScopeTranscript(issueKey: string): Promise<string | null> {
  const res = await rawFetch(`/api/issues/${encodeURIComponent(issueKey)}/scope-transcript`);
  if (res.status === 404) return null;
  if (!res.ok) {
    throw new Error(`scope transcript: ${res.status} ${res.statusText}`);
  }
  return res.text();
}
