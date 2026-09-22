import type { components } from '../api/schema';

type RunFile = components['schemas']['RunFile'];

/** How a captured file is rendered inline; null means download only. */
export type InlineKind = 'markdown' | 'json' | 'text' | 'image';

const IMAGE_MIME: Record<string, string> = {
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  avif: 'image/avif',
  bmp: 'image/bmp',
  svg: 'image/svg+xml',
};

function extensionOf(path: string): string {
  const slash = path.lastIndexOf('/');
  const dot = path.lastIndexOf('.');
  if (dot <= slash + 1) return '';
  return path.slice(dot + 1).toLowerCase();
}

/** The image media type a file's extension names, or null when it names no image. */
export function imageMimeType(path: string): string | null {
  return IMAGE_MIME[extensionOf(path)] ?? null;
}

export function inlineKind(path: string): InlineKind | null {
  const ext = extensionOf(path);
  if (ext === 'md') return 'markdown';
  if (ext === 'json') return 'json';
  if (ext === 'txt' || ext === 'log') return 'text';
  if (imageMimeType(path) !== null) return 'image';
  return null;
}

/** The inline kinds that read as text; an image is bytes and has no text rendering. */
export type TextKind = 'markdown' | 'json' | 'text';

export function textKind(path: string): TextKind | null {
  const kind = inlineKind(path);
  return kind === null || kind === 'image' ? null : kind;
}

export function prettyJson(text: string): string {
  try {
    const parsed: unknown = JSON.parse(text);
    return JSON.stringify(parsed, null, 2);
  } catch {
    return text;
  }
}

/** A run file addressed by route: the run, the launch it hangs under if any, and the file key. */
export interface RunFileRoute {
  launchKey: string | null;
  runId: string;
  fileKey: string;
}

/**
 * A file key is a path whose segments carry brackets, spaces and unicode (`assess[3]/VERDICT.json`).
 * Encoding per segment keeps the slashes as separators and everything else literal.
 */
export function encodeFileKey(key: string): string {
  return key.split('/').map(encodeURIComponent).join('/');
}

function decodeSegment(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

export function decodeFileKey(encoded: string): string {
  return encoded.split('/').map(decodeSegment).join('/');
}

export function runPath(runId: string, launchKey: string | null): string {
  const run = `/runs/${encodeURIComponent(runId)}`;
  return launchKey === null ? run : `/playbook-runs/${encodeURIComponent(launchKey)}${run}`;
}

export function runFilePath(route: RunFileRoute): string {
  return `${runPath(route.runId, route.launchKey)}/files/${encodeFileKey(route.fileKey)}`;
}

/** The shareable link, origin included, for pasting somewhere that is not this tab. */
export function runFileUrl(origin: string, route: RunFileRoute): string {
  return `${origin}${runFilePath(route)}`;
}

/** Where the file's bytes are served: authenticated, and the download target. */
export function runFileApiPath(runId: string, key: string): string {
  return `/api/runs/${encodeURIComponent(runId)}/files/${encodeFileKey(key)}`;
}

/**
 * The route a client pathname addresses, or null when the pathname is not a run file deep link.
 * Parsed off the pathname rather than a router splat so each segment decodes on its own.
 */
export function parseRunFilePath(pathname: string): RunFileRoute | null {
  const parts = pathname.split('/').filter((p) => p !== '');
  let launchKey: string | null = null;
  let rest = parts;
  if (parts[0] === 'playbook-runs') {
    if (parts[1] === undefined) return null;
    launchKey = decodeSegment(parts[1]);
    rest = parts.slice(2);
  }
  if (rest[0] !== 'runs' || rest[2] !== 'files') return null;
  const runId = rest[1];
  if (runId === undefined) return null;
  const fileKey = decodeFileKey(rest.slice(3).join('/'));
  if (fileKey === '') return null;
  return { launchKey, runId: decodeSegment(runId), fileKey };
}

/** What the files panel shows: the requested file, a broken link, or nothing captured. */
export type FileSelection =
  | { state: 'empty' }
  | { state: 'missing'; key: string }
  | { state: 'found'; file: RunFile };

/**
 * A requested key must resolve to that file or to nothing: silently showing a different file would
 * make a shared link lie. With no key requested the report is what a reader came for.
 */
export function selectRunFile(entries: readonly RunFile[], requested: string | null): FileSelection {
  if (requested !== null) {
    const hit = entries.find((e) => e.key === requested);
    return hit === undefined ? { state: 'missing', key: requested } : { state: 'found', file: hit };
  }
  const fallback =
    entries.find((e) => e.path.toLowerCase() === 'report.md') ??
    entries.find((e) => inlineKind(e.path) === 'markdown') ??
    entries[0];
  return fallback === undefined ? { state: 'empty' } : { state: 'found', file: fallback };
}

/**
 * Whether the files panel should scroll itself into view. Only a page that was OPENED on a file
 * deep link anchors: clicking a file rewrites the URL into one, and yanking the viewport down on a
 * click the reader already aimed is the opposite of helpful.
 */
export function shouldAnchorFiles(state: {
  openedOnDeepLink: boolean;
  anchored: boolean;
  loaded: number;
}): boolean {
  return state.openedOnDeepLink && !state.anchored && state.loaded > 0;
}

/** The task a captured file belongs to; an instance of a mapped task is keyed `task[instance]`. */
export function producerOf(file: RunFile): string {
  return file.instance === null || file.instance === undefined
    ? file.task
    : `${file.task}[${file.instance}]`;
}

export function downloadName(runId: string, key: string): string {
  return `${runId}-${key.replaceAll('/', '-')}`;
}
