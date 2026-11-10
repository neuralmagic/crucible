import { Mono } from '../ui';
import { CheckField, FormGrid, SelectField, TextAreaField, TextField } from './formControls';
import { isEnabled, paramBlocks, spellingOf, type ParamFieldSpec } from './playbookLaunchForm';

export interface PlaybookParamFieldsProps {
  idPrefix: string;
  specs: readonly ParamFieldSpec[];
  values: Readonly<Record<string, string>>;
  errors: ReadonlyMap<string, string>;
  onChange: (name: string, value: string) => void;
  onBlur: (name: string) => void;
}

/// The pack's declared params, one input each. Page-independent because the schedule form and the
/// ask forms render the same document. A boolean that enables other fields renders as a checkbox
/// heading a group of the fields it enables.
export function PlaybookParamFields({
  idPrefix,
  specs,
  values,
  errors,
  onChange,
  onBlur,
}: PlaybookParamFieldsProps) {
  const first = specs[0]?.name;
  const input = (spec: ParamFieldSpec) => {
    const field = {
      id: `${idPrefix}-param-${spec.name}`,
      label: <Mono size="label">{spec.name}</Mono>,
      required: spec.required,
      disabled: !isEnabled(spec, values),
      mono: true,
      autoFocus: spec.name === first,
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
    if (spec.choices !== null) {
      const blank = spec.required && spec.defaultValue !== null ? [] : [{ value: '', label: spec.required ? 'Select…' : '—' }];
      return (
        <SelectField
          key={spec.name}
          {...field}
          options={[...blank, ...spec.choices.map((choice) => ({ value: choice, label: choice }))]}
        />
      );
    }
    return spec.multiline ? (
      <TextAreaField key={spec.name} rows={8} {...field} />
    ) : (
      <TextField key={spec.name} {...field} />
    );
  };
  return (
    <FormGrid>
      {paramBlocks(specs).map((block) =>
        block.kind === 'field' ? (
          input(block.spec)
        ) : (
          <fieldset key={block.gate.name} className="m-0 grid gap-4 border border-rule-hard p-3">
            <CheckField
              id={`${idPrefix}-param-${block.gate.name}`}
              label={<Mono size="label">{block.gate.name}</Mono>}
              checked={(values[block.gate.name] ?? '').trim().toLowerCase() === 'true'}
              onChange={(checked) => {
                onChange(block.gate.name, checked ? 'true' : 'false');
              }}
              description={block.gate.description ?? undefined}
            />
            {block.fields.map(input)}
          </fieldset>
        )
      )}
    </FormGrid>
  );
}
