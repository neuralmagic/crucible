import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import { Mono } from '../ui';
import { FormError, RichSelectField } from './formControls';
import { buildLabel, imageChoices, unavailableImages, type Build, type ImageChoice } from './imageChoices';
import { pinnedReference, shortDigest } from './imagePicker';

type PackDispatchDto = components['schemas']['PackDispatchDto'];
type RankedCatalog = components['schemas']['RankedCatalog'];
type CatalogImageDto = components['schemas']['CatalogImageDto'];

export interface ImagePickerFieldProps {
  idPrefix: string;
  /** The compiled `[agent]` of the newest save: what to rank the catalog for. */
  dispatch: PackDispatchDto;
  /** The `sandbox_image` the buffer names right now, or null. */
  current: string | null;
  /** Fires with the digest-pinned reference to write into the manifest. */
  onPick: (reference: string) => void;
}

const CUSTOM = '';

/** Which ranked image the buffer's reference names, by digest or by a tag it carries. */
export function selectedDigest(current: string | null, images: readonly CatalogImageDto[]): string {
  if (current === null) return CUSTOM;
  const at = current.indexOf('@');
  if (at !== -1) {
    const digest = current.slice(at + 1);
    return images.some((i) => i.digest === digest) ? digest : CUSTOM;
  }
  const slash = current.lastIndexOf('/');
  const colon = current.indexOf(':', slash + 1);
  const repository = colon === -1 ? current : current.slice(0, colon);
  const tag = colon === -1 ? 'latest' : current.slice(colon + 1);
  const hit = images.find((i) => i.repository === repository && i.tags.includes(tag));
  return hit?.digest ?? CUSTOM;
}

function describeImage(choice: ImageChoice<CatalogImageDto>) {
  return (
    <span className="flex min-w-0 items-center gap-2">
      <span className="truncate">{choice.name}</span>
      <Mono size="data" tone="ink-3">
        {buildLabel(choice.newest)}
      </Mono>
      {choice.isDefault ? (
        <Mono size="data" tone="ink-3">
          default
        </Mono>
      ) : null}
    </span>
  );
}

function describeBuild({ image, reason }: Build<CatalogImageDto>) {
  const created = image.created_at ? image.created_at.slice(0, 10) : null;
  return (
    <span className="flex min-w-0 items-center gap-2">
      <span className="truncate">{buildLabel(image)}</span>
      <Mono size="data" tone="ink-3">
        {shortDigest(image.digest)}
        {created === null ? '' : ` · ${created}`}
      </Mono>
      {reason === null ? null : (
        <Mono size="data" tone="amber">
          {reason}
        </Mono>
      )}
    </span>
  );
}

/// One choice per image, then its builds. Picking writes `repository@digest`.
export function ImagePickerField({ idPrefix, dispatch, current, onPick }: ImagePickerFieldProps) {
  const ranked = $api.useQuery('post', '/api/images/rank', {
    body: {
      requires: dispatch.requires,
      prefers: dispatch.prefers,
      harness: dispatch.harness ?? undefined,
    },
  });
  if (ranked.isError) return <FormError>{formatError(ranked.error)}</FormError>;
  const data: RankedCatalog | undefined = ranked.data;
  if (data === undefined) return null;
  const choices = imageChoices(data.compatible, data.excluded, data.unverified);
  const unavailable = unavailableImages(data.excluded, data.unverified, choices);
  if (choices.length === 0 && unavailable.length === 0) return null;

  const builds = choices.flatMap((c) => c.builds.map((b) => b.image));
  const selected = selectedDigest(current, builds);
  const chosen = choices.find((c) => c.builds.some((b) => b.image.digest === selected));
  return (
    <div className="flex flex-col gap-2">
      <RichSelectField
        id={`${idPrefix}-sandbox-image`}
        label="Sandbox image"
        value={chosen?.repository ?? CUSTOM}
        onChange={(repository) => {
          const newest = choices.find((c) => c.repository === repository)?.newest;
          if (newest !== undefined) onPick(pinnedReference(newest.repository, newest.digest));
        }}
        options={[
          { value: CUSTOM, label: <span className="truncate text-ink-2">{current ?? 'none'}</span> },
          ...choices.map((choice) => ({ value: choice.repository, label: describeImage(choice) })),
        ]}
      />
      {chosen === undefined ? null : (
        <RichSelectField
          id={`${idPrefix}-sandbox-build`}
          label="Build"
          value={selected}
          onChange={(digest) => {
            const build = chosen.builds.find((b) => b.image.digest === digest && b.reason === null);
            if (build !== undefined) onPick(pinnedReference(build.image.repository, build.image.digest));
          }}
          options={chosen.builds.map((build) => ({
            value: build.image.digest,
            label: describeBuild(build),
            disabled: build.reason !== null,
          }))}
        />
      )}
      {unavailable.length === 0 ? null : (
        <ul className="m-0 list-none p-0" data-testid={`${idPrefix}-excluded-images`}>
          {unavailable.map((row) => (
            <li key={row.repository} className="flex flex-wrap items-baseline gap-2 py-0.5 text-ink-2">
              <span>{row.name}</span>
              <Mono size="data" tone="amber">
                {row.reason}
              </Mono>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
