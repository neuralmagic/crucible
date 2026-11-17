import { useState } from 'react';
import { Mono } from '../ui';
import { CheckField, CheckGroupField, FormGrid, SelectField, TextAreaField, TextField } from './formControls';
import {
  confirmation,
  fanOutDependents,
  fanOutOf,
  isEnabled,
  offeredChoices,
  paramBlocks,
  perValueKey,
  picked,
  placeholderFor,
  spellingOf,
  valuesFor,
  withPick,
  type ParamBlock,
  type ParamFieldSpec,
} from './playbookLaunchForm';

/// The select entry that switches an open choice to a typed value. Not a value any choice can
/// take: the form never submits it.
const OTHER = '\u0000other';

/// The values a run of the form sees, and the form key each param's input reads and writes: its
/// name, or its per-value key inside one picked value of a fan-out param.
interface Scope {
  values: Readonly<Record<string, string>>;
  keyOf: (name: string) => string;
  compact?: boolean;
}

const domId = (key: string) => key.replace(/[^A-Za-z0-9_-]/g, '-');

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
  const top: Scope = { values, keyOf: (name) => name };
  const input = (spec: ParamFieldSpec, scope: Scope = top) => {
    const values = scope.values;
    const key = scope.keyOf(spec.name);
    const label = <Mono size="label">{spec.name}</Mono>;
    const field = {
      id: `${idPrefix}-param-${domId(key)}`,
      label: scope.compact ? <span title={spec.description ?? undefined}>{label}</span> : label,
      required: spec.required,
      disabled: !isEnabled(spec, values),
      mono: true,
      autoFocus: key === first,
      value: values[spec.name] ?? '',
      onChange: (value: string) => {
        onChange(key, value);
      },
      onBlur: () => {
        onBlur(key);
      },
      error: errors.get(key) ?? null,
      hint: scope.compact
        ? undefined
        : (spec.description ??
          (spec.pattern !== null ? `Must match ${spec.pattern}` : (spellingOf(spec.valueType) ?? undefined))),
    };
    const confirm = confirmation(spec, values);
    if (confirm?.kind === 'hidden') return null;
    if (confirm !== null) {
      return (
        <CheckField
          key={key}
          id={field.id}
          label={field.label}
          required={confirm.kind === 'needed'}
          checked={field.value.trim().toLowerCase() === 'true'}
          onChange={(checked) => {
            onChange(key, checked ? 'true' : 'false');
          }}
          description={
            confirm.kind === 'needed' ? confirm.message : scope.compact ? undefined : (spec.description ?? undefined)
          }
          error={field.error}
        />
      );
    }
    const offered = offeredChoices(spec, values);
    const placeholder = placeholderFor(spec, values) ?? undefined;
    if (offered !== null) {
      const value = field.value;
      const listed = offered.includes(value);
      const other = spec.allowOther && (typing.has(key) || (value !== '' && !listed));
      const blank = spec.required && spec.defaultValue !== null && listed ? [] : [{ value: '', label: spec.required ? 'Select…' : '—' }];
      const stale = !listed && value !== '' && !spec.allowOther ? [{ value, label: `${value} (not offered)` }] : [];
      const options = [
        ...blank,
        ...offered.map((choice) => ({ value: choice, label: choice })),
        ...stale,
        ...(spec.allowOther ? [{ value: OTHER, label: 'Other…' }] : []),
      ];
      return (
        <div key={key} className="grid gap-2">
          <SelectField
            {...field}
            value={other ? OTHER : value}
            options={options}
            onChange={(choice) => {
              setTypingFor(key, choice === OTHER);
              onChange(key, choice === OTHER ? '' : choice);
            }}
          />
          {other ? (
            <TextField
              {...field}
              id={`${field.id}-other`}
              label={<Mono size="label">{spec.name} (other)</Mono>}
              autoFocus={typing.has(key)}
              placeholder={placeholder}
              hint={undefined}
            />
          ) : null}
        </div>
      );
    }
    return spec.multiline ? (
      <TextAreaField key={key} rows={8} {...field} />
    ) : (
      <TextField key={key} {...field} placeholder={placeholder} />
    );
  };
  const render = (block: ParamBlock, scope: Scope = top) => {
    if (block.kind === 'field') return input(block.spec, scope);
    const gate = scope.keyOf(block.gate.name);
    const on = (scope.values[block.gate.name] ?? '').trim().toLowerCase() === 'true';
    const label = <Mono size="label">{block.gate.name}</Mono>;
    return (
      <fieldset
        key={gate}
        className={scope.compact ? 'm-0 grid gap-2 border-0 p-0' : 'm-0 grid gap-4 border border-rule-hard p-3'}
      >
        <CheckField
          id={`${idPrefix}-param-${domId(gate)}`}
          label={scope.compact ? <span title={block.gate.description ?? undefined}>{label}</span> : label}
          checked={on}
          onChange={(checked) => {
            onChange(gate, checked ? 'true' : 'false');
          }}
          description={scope.compact ? undefined : (block.gate.description ?? undefined)}
        />
        {scope.compact && !on ? null : block.fields.map((spec) => input(spec, scope))}
      </fieldset>
    );
  };

  const fan = fanOutOf(specs);
  const picks = fan === null ? [] : picked(fan, values);
  const dependents = fan === null ? new Set<string>() : fanOutDependents(specs, fan.name);
  const dependentBlocks = paramBlocks(specs.filter((spec) => dependents.has(spec.name)));
  const blockNames = (block: ParamBlock) =>
    block.kind === 'field' ? [block.spec.name] : [block.gate.name, ...block.fields.map((spec) => spec.name)];
  const detail = (fanned: ParamFieldSpec, value: string) => {
    const scope: Scope = {
      values: valuesFor(specs, values, fanned, value),
      keyOf: (name) => (dependents.has(name) ? perValueKey(name, value) : name),
      compact: true,
    };
    return dependentBlocks.map((block) => render(block, scope));
  };
  const fanHint = (fanned: ParamFieldSpec) => {
    const description = fanned.description?.trim() ?? '';
    const sentence = description === '' || /[.!?]$/.test(description) ? description : `${description}.`;
    const runs = picks.length > 1 ? `One run per picked value: ${String(picks.length)} runs.` : '';
    const text = [sentence, runs].filter((part) => part !== '').join(' ');
    return text === '' ? undefined : text;
  };
  return (
    <FormGrid>
      {paramBlocks(specs).map((block) => {
        if (fan !== null && block.kind === 'field' && block.spec.name === fan.name) {
          return (
            <CheckGroupField
              key={fan.name}
              id={`${idPrefix}-param-${domId(fan.name)}`}
              label={<Mono size="label">{fan.name}</Mono>}
              options={offeredChoices(fan, values) ?? []}
              checked={picks}
              onToggle={(value, on) => {
                onChange(fan.name, withPick(fan, values, value, on));
              }}
              required={fan.required}
              hint={fanHint(fan)}
              error={errors.get(fan.name) ?? null}
              detail={dependents.size > 0 ? (value) => detail(fan, value) : undefined}
            />
          );
        }
        if (fan !== null && blockNames(block).every((name) => dependents.has(name))) return null;
        return render(block);
      })}
    </FormGrid>
  );
}
