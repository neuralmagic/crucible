import { Mono } from '../ui';
import { FormGrid, TextField } from './formControls';
import type { ParamFieldSpec } from './playbookLaunchForm';

export interface PlaybookParamFieldsProps {
  idPrefix: string;
  specs: readonly ParamFieldSpec[];
  values: Readonly<Record<string, string>>;
  errors: ReadonlyMap<string, string>;
  onChange: (name: string, value: string) => void;
  onBlur: (name: string) => void;
}

/// The pack's declared params, one input each. Page-independent because the schedule form and the
/// ask forms render the same document.
export function PlaybookParamFields({
  idPrefix,
  specs,
  values,
  errors,
  onChange,
  onBlur,
}: PlaybookParamFieldsProps) {
  return (
    <FormGrid>
      {specs.map((spec, index) => (
        <TextField
          key={spec.name}
          id={`${idPrefix}-param-${spec.name}`}
          label={<Mono size="label">{spec.name}</Mono>}
          required={spec.required}
          mono
          autoFocus={index === 0}
          value={values[spec.name] ?? ''}
          onChange={(value) => {
            onChange(spec.name, value);
          }}
          onBlur={() => {
            onBlur(spec.name);
          }}
          error={errors.get(spec.name) ?? null}
          hint={spec.description ?? (spec.pattern !== null ? `Must match ${spec.pattern}` : undefined)}
        />
      ))}
    </FormGrid>
  );
}
