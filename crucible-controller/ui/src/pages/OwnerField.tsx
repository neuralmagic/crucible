import type { ActedAs } from '../ownerContext';
import { Mono } from '../ui';
import { SelectField } from './formControls';
import { principalOptions } from './pickList';

export interface OwnerFieldProps {
  id: string;
  value: string;
  onChange: (owner: string) => void;
  options: readonly ActedAs[];
}

/// The owner a creation form registers under: one of the principals the caller may own as.
export function OwnerField({ id, value, onChange, options }: OwnerFieldProps) {
  return (
    <SelectField
      id={id}
      label={<Mono size="label">Owner</Mono>}
      value={value}
      onChange={onChange}
      options={principalOptions(options)}
      required
    />
  );
}
