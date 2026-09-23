import { Mono, Section, SectionBody, SectionHeader } from '../ui';
import { MetaRow, Notice } from './formControls';
import type { components } from '../api/schema';
import { resourcesLabel } from './sandboxResources';

type PackDispatchDto = components['schemas']['PackDispatchDto'];

export interface PackDispatchNoticeProps {
  dispatch: PackDispatchDto;
}

/// What a pack's agent needs and whether this deployment can give it. Shown wherever a pack is
/// looked at before it runs — the preview gate, the import review, the launch form — so an
/// undispatchable backend or an image that fails its preflight is read here rather than
/// discovered by a failed run.
export function PackDispatchNotice({ dispatch }: PackDispatchNoticeProps) {
  const backend = dispatch.backend ?? 'unknown';
  const image = dispatch.image;
  const imageBad = image.refusals.length > 0;
  const resources = resourcesLabel(dispatch.resources);
  return (
    <Section>
      <SectionHeader
        title="Substrate"
        actions={
          <Mono size="data" tone={dispatch.dispatchable && !imageBad ? 'ink-3' : 'amber'}>
            {dispatch.local_mode ? 'local dispatch' : 'pod dispatch'}
          </Mono>
        }
      />
      {dispatch.refusal === null || dispatch.refusal === undefined ? null : (
        <Notice label="Cannot dispatch">{dispatch.refusal}</Notice>
      )}
      {image.refusals.map((refusal) => (
        <Notice key={refusal} label="Image preflight">
          {refusal}
        </Notice>
      ))}
      {image.warnings.map((warning) => (
        <Notice key={warning} label="Image warning">
          {warning}
        </Notice>
      ))}
      <SectionBody>
        <dl className="m-0">
          <MetaRow label="declares">{`[agent] backend = ${backend}`}</MetaRow>
          {dispatch.harness === null || dispatch.harness === undefined ? null : (
            <MetaRow label="harness">{dispatch.harness}</MetaRow>
          )}
          {dispatch.sandbox_image === null || dispatch.sandbox_image === undefined ? null : (
            <MetaRow label="sandbox image">{dispatch.sandbox_image}</MetaRow>
          )}
          {resources === null ? null : <MetaRow label="resources">{resources}</MetaRow>}
          {image.tags.length === 0 ? null : <MetaRow label="channel">{image.tags.join(', ')}</MetaRow>}
          {image.digest === null || image.digest === undefined ? null : (
            <MetaRow label="resolved digest">
              <Mono size="data">{image.digest}</Mono>
            </MetaRow>
          )}
        </dl>
      </SectionBody>
    </Section>
  );
}
