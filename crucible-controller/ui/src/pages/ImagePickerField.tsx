import { $api } from '../api/client';
import { formatError } from '../api/errors';
import type { components } from '../api/schema';
import { Mono } from '../ui';
import { FormError, RichSelectField } from './formControls';
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

function channelOf(image: CatalogImageDto): string {
  return image.tags.includes('latest') ? 'latest' : (image.tags[0] ?? 'untagged');
}

function describe(image: CatalogImageDto, isDefault: boolean) {
  return (
    <span className="flex min-w-0 items-center gap-2">
      <span className="truncate">{image.name}</span>
      <Mono size="data" tone="ink-3">
        {channelOf(image)} · {shortDigest(image.digest)}
      </Mono>
      {isDefault ? (
        <Mono size="data" tone="ink-3">
          default
        </Mono>
      ) : null}
    </span>
  );
}

/// The catalog ranked for this pack: compatible images to pick from, slimmest first; excluded
/// images with the predicate that excluded them; unverified images by name. Picking writes
/// `repository@digest` into the manifest.
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
  const all = [...data.compatible.map((r) => r.image), ...data.excluded.map((e) => e.image), ...data.unverified];
  if (all.length === 0) return null;
  const selected = selectedDigest(current, all);
  const customLabel = current === null ? 'none' : selected === CUSTOM ? current : 'custom';
  return (
    <div className="flex flex-col gap-2">
      <RichSelectField
        id={`${idPrefix}-sandbox-image`}
        label="Sandbox image"
        value={selected}
        onChange={(digest) => {
          if (digest === CUSTOM) return;
          const image = all.find((i) => i.digest === digest);
          if (image !== undefined) onPick(pinnedReference(image.repository, image.digest));
        }}
        options={[
          { value: CUSTOM, label: <span className="truncate text-ink-2">{customLabel}</span> },
          ...data.compatible.map((r) => ({
            value: r.image.digest,
            label: describe(r.image, r.default),
          })),
        ]}
      />
      {data.excluded.length === 0 && data.unverified.length === 0 ? null : (
        <ul className="m-0 list-none p-0" data-testid={`${idPrefix}-excluded-images`}>
          {data.excluded.map((e) => (
            <li key={e.image.digest} className="flex flex-wrap items-baseline gap-2 py-0.5 text-ink-2">
              <span>{e.image.name}</span>
              <Mono size="data" tone="amber">
                {e.unsatisfied
                  .map((u) =>
                    u.found === null || u.found === undefined
                      ? `lacks ${u.predicate}`
                      : `${u.predicate} ${u.found} ≠ ${u.required}`,
                  )
                  .join(', ')}
              </Mono>
            </li>
          ))}
          {data.unverified.map((i) => (
            <li key={i.digest} className="flex flex-wrap items-baseline gap-2 py-0.5 text-ink-2">
              <span>{i.name}</span>
              <Mono size="data" tone="amber">
                unverified
              </Mono>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
