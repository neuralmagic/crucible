import { useEffect, useMemo } from 'react';
import { $api } from '../api/client';
import { SelectField, Notice } from './formControls';
import type { components } from '../api/schema';

type EligibleTarget = components['schemas']['EligibleTarget'];

/// How a target is described to a human. The name a personal target sends back is an opaque secret
/// id, so the label carries the name its owner gave it.
function describe(target: EligibleTarget): string {
  const suffix = target.default ? ' (default)' : '';
  switch (target.kind) {
    case 'hub':
      return `${target.label} — this controller's own cluster${suffix}`;
    case 'spoke':
      return `${target.label} — connected cluster${suffix}`;
    case 'personal':
      return `${target.label} — your registered cluster`;
  }
}

interface DispatchTargetFieldProps {
  id: string;
  /// The pack this launch runs, so its declared agent substrate narrows the set. Omit for an
  /// autoresearch scenario, which has no pack yet.
  playbook?: string;
  value: string;
  onChange: (value: string) => void;
  onChoiceRequired: (required: boolean) => void;
}

/// The cluster picker. Renders nothing at all when the caller has one target or none to choose
/// between: a deployment with no spokes and no registered credential should not grow a control that
/// offers a single option.
export function DispatchTargetField({
  id,
  playbook,
  value,
  onChange,
  onChoiceRequired,
}: DispatchTargetFieldProps) {
  const targets = $api.useQuery('get', '/api/dispatch-targets', {
    params: { query: playbook === undefined ? {} : { playbook } },
  });

  const options = useMemo(
    () => (targets.data?.targets ?? []).map((t) => ({ value: t.name, label: describe(t) })),
    [targets.data]
  );
  const choiceRequired = options.length > 0 && !targets.data?.targets.some((t) => t.default);

  useEffect(() => {
    onChoiceRequired(choiceRequired);
  }, [choiceRequired, onChoiceRequired]);

  if (targets.data === undefined) return null;
  if (targets.data.refusal !== null && targets.data.refusal !== undefined) {
    return <Notice label="Nowhere to dispatch">{targets.data.refusal}</Notice>;
  }
  if (options.length < 2 && !choiceRequired) return null;

  return (
    <SelectField
      id={id}
      label="Cluster"
      value={value}
      onChange={onChange}
      options={choiceRequired ? [{ value: '', label: 'Choose a cluster…' }, ...options] : options}
      required={choiceRequired}
      hint="Where this run's pods are created. Only clusters you are authorized for are listed."
    />
  );
}
