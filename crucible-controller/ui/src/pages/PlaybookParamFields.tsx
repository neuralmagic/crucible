import { Mono } from '../ui';
import { FormGrid, TextAreaField, TextField } from './formControls';
import { spellingOf, type ParamFieldSpec } from './playbookLaunchForm';

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
      {specs.map((spec, index) => {
        const field = {
          id: `${idPrefix}-param-${spec.name}`,
          label: <Mono size="label">{spec.name}</Mono>,
          required: spec.required,
          mono: true,
          autoFocus: index === 0,
          value: values[spec.name] ?? '',
          onChange: (value: string) => {
            onChange(spec.name, value);
          },
          onBlur: () => {
            onBlur(spec.name);
          },
          error: errors.get(spec.name) ?? null,
          hint:
            spec.description ??
            (spec.pattern !== null ? `Must match ${spec.pattern}` : (spellingOf(spec.valueType) ?? undefined)),
        };
        return spec.multiline ? (
          <TextAreaField key={spec.name} rows={8} {...field} />
        ) : (
          <TextField key={spec.name} {...field} />
        );
      })}
    </FormGrid>
  );
}
