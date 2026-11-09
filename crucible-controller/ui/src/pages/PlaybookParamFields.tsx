import { useState } from 'react';
import { Mono } from '../ui';
import { CheckField, FormGrid, SelectField, TextAreaField, TextField } from './formControls';
import {
  isEnabled,
  offeredChoices,
  paramBlocks,
  placeholderFor,
  snapshotAge,
  spellingOf,
  type ParamFieldSpec,
} from './playbookLaunchForm';

/// The select entry that switches an open choice to a typed value. Not a value any choice can
/// take: the form never submits it.
const OTHER = '\u0000other';

/// Where a param's options came from and how old they are, for its hint.
function sourceHint(spec: ParamFieldSpec, values: Readonly<Record<string, string>>): string | null {
  const from = spec.choicesFrom;
  if (from === null) return null;
  if (spec.choicesSnapshot === null) return `Options appear once ${from.task} has passed; refresh to compute them`;
  const key = from.keyedBy;
  const keyValue = key === null ? '' : (values[key] ?? '').trim();
  if (key !== null && keyValue === '') return `Pick ${key} to see options`;
  const age = snapshotAge(spec.choicesSnapshot.capturedAt);
  const scope = key === null ? '' : ` for ${key} ${keyValue}`;
  return `Options${scope} from ${from.task}${age === null ? '' : `, ${age}`}`;
}

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
  const [typing, setTyping] = useState<ReadonlySet<string>>(new Set());
  const setTypingFor = (name: string, on: boolean) => {
    setTyping((current) => {
      const next = new Set(current);
      if (on) next.add(name);
      else next.delete(name);
      return next;
    });
  };
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
        [spec.description, sourceHint(spec, values)].filter((part) => part !== null).join(' · ') ||
        (spec.pattern !== null ? `Must match ${spec.pattern}` : (spellingOf(spec.valueType) ?? undefined)),
    };
    const offered = offeredChoices(spec, values);
    const placeholder = placeholderFor(spec, values) ?? undefined;
    if (offered !== null) {
      const value = field.value;
      const listed = offered.includes(value);
      const other = spec.allowOther && (typing.has(spec.name) || (value !== '' && !listed));
      const blank = spec.required && spec.defaultValue !== null && listed ? [] : [{ value: '', label: spec.required ? 'Select…' : '—' }];
      const stale = !listed && value !== '' && !spec.allowOther ? [{ value, label: `${value} (not offered)` }] : [];
      const options = [
        ...blank,
        ...offered.map((choice) => ({ value: choice, label: choice })),
        ...stale,
        ...(spec.allowOther ? [{ value: OTHER, label: 'Other…' }] : []),
      ];
      return (
        <div key={spec.name} className="grid gap-2">
          <SelectField
            {...field}
            value={other ? OTHER : value}
            options={options}
            onChange={(picked) => {
              setTypingFor(spec.name, picked === OTHER);
              onChange(spec.name, picked === OTHER ? '' : picked);
            }}
          />
          {other ? (
            <TextField
              {...field}
              id={`${field.id}-other`}
              label={<Mono size="label">{spec.name} (other)</Mono>}
              autoFocus={typing.has(spec.name)}
              placeholder={placeholder}
              hint={undefined}
            />
          ) : null}
        </div>
      );
    }
    return spec.multiline ? (
      <TextAreaField key={spec.name} rows={8} {...field} />
    ) : (
      <TextField key={spec.name} {...field} placeholder={placeholder} />
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
