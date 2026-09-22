import { Checkbox } from '@base-ui-components/react/checkbox';
import { Field } from '@base-ui-components/react/field';
import { NumberField } from '@base-ui-components/react/number-field';
import { Select } from '@base-ui-components/react/select';
import { useState, type ReactNode } from 'react';
import { Button, cn, Mono } from '../ui';
import { arrangeOptions, type PickOption } from './pickList';

const LABEL = 'font-mono text-label font-semibold uppercase tracking-group text-ink-3';
const CONTROL =
  'w-full border border-rule-hard bg-surface px-2 py-1.5 text-ink placeholder:text-ink-3';
const MONO_CONTROL = 'font-mono text-data-lg';
const HINT = 'max-w-[66ch] text-data-lg text-ink-3';
const STEP =
  'cursor-pointer border-0 bg-transparent px-2.5 py-1.5 font-mono text-data text-ink-2 select-none hover:bg-hi hover:text-ink';

interface FieldShellProps {
  label: ReactNode;
  htmlFor?: string;
  required?: boolean;
  hint?: ReactNode;
  error?: string | null;
  children: ReactNode;
}

function FieldShell({ label, htmlFor, required = false, hint, error, children }: FieldShellProps) {
  return (
    <Field.Root className="grid gap-1.5">
      <Field.Label htmlFor={htmlFor} className={LABEL}>
        {label}
        {required && <span className="ml-1 text-red">*</span>}
      </Field.Label>
      {children}
      {error ? (
        <p role="alert" className="m-0 max-w-[66ch] text-data-lg text-red">
          {error}
        </p>
      ) : null}
      {hint !== undefined && <Field.Description className={HINT}>{hint}</Field.Description>}
    </Field.Root>
  );
}

export interface TextFieldProps {
  id: string;
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  hint?: ReactNode;
  required?: boolean;
  /** Render the value in mono: identifiers, repos, refs. */
  mono?: boolean;
  autoFocus?: boolean;
  /** A refusal to render against this input: a client check, or a server field-level rejection. */
  error?: string | null;
  onBlur?: () => void;
}

export function TextField({
  id,
  label,
  value,
  onChange,
  placeholder,
  hint,
  required,
  mono = false,
  autoFocus,
  error,
  onBlur,
}: TextFieldProps) {
  return (
    <FieldShell label={label} htmlFor={id} required={required} hint={hint} error={error}>
      <Field.Control
        id={id}
        value={value}
        onValueChange={onChange}
        onBlur={onBlur}
        placeholder={placeholder}
        autoFocus={autoFocus}
        aria-invalid={Boolean(error)}
        className={cn(CONTROL, mono && MONO_CONTROL, Boolean(error) && 'border-red')}
      />
    </FieldShell>
  );
}

export interface BareTextInputProps {
  id: string;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  'aria-label': string;
  mono?: boolean;
  className?: string;
  /** `password` hides the bytes; the default is a plain text control. */
  type?: 'text' | 'password';
}

/** A control without its own label, for a repeated row inside another field. */
export function BareTextInput({
  id,
  value,
  onChange,
  placeholder,
  'aria-label': ariaLabel,
  mono = false,
  className,
  type = 'text',
}: BareTextInputProps) {
  return (
    <input
      id={id}
      type={type}
      value={value}
      aria-label={ariaLabel}
      placeholder={placeholder}
      onChange={(event) => {
        onChange(event.target.value);
      }}
      className={cn(CONTROL, mono && MONO_CONTROL, className)}
    />
  );
}

export interface TextAreaFieldProps {
  id: string;
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  rows?: number;
  placeholder?: string;
  hint?: ReactNode;
  required?: boolean;
  autoFocus?: boolean;
}

export function TextAreaField({
  id,
  label,
  value,
  onChange,
  rows = 4,
  placeholder,
  hint,
  required,
  autoFocus,
}: TextAreaFieldProps) {
  return (
    <FieldShell label={label} htmlFor={id} required={required} hint={hint}>
      <textarea
        id={id}
        value={value}
        rows={rows}
        placeholder={placeholder}
        autoFocus={autoFocus}
        onChange={(event) => {
          onChange(event.target.value);
        }}
        className={cn(CONTROL, 'resize-y leading-normal')}
      />
    </FieldShell>
  );
}

export interface PasswordFieldProps {
  id: string;
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  hint?: ReactNode;
  required?: boolean;
  error?: string | null;
  /** Render as a masked textarea of this height instead of a single line. */
  rows?: number;
}

/** A secret-bearing input: hidden by default, revealed only while the caller holds it open. */
export function PasswordField({
  id,
  label,
  value,
  onChange,
  placeholder,
  hint,
  required,
  error,
  rows,
}: PasswordFieldProps) {
  const [shown, setShown] = useState(false);
  const control = cn(CONTROL, MONO_CONTROL, 'min-w-0 flex-1', Boolean(error) && 'border-red');
  return (
    <FieldShell label={label} htmlFor={id} required={required} hint={hint} error={error}>
      <div className="flex items-start gap-2">
        {rows === undefined ? (
          <input
            id={id}
            type={shown ? 'text' : 'password'}
            value={value}
            placeholder={placeholder}
            autoComplete="off"
            spellCheck={false}
            aria-invalid={Boolean(error)}
            onChange={(event) => {
              onChange(event.target.value);
            }}
            className={control}
          />
        ) : (
          <textarea
            id={id}
            value={value}
            rows={rows}
            placeholder={placeholder}
            autoComplete="off"
            spellCheck={false}
            aria-invalid={Boolean(error)}
            onChange={(event) => {
              onChange(event.target.value);
            }}
            className={cn(control, 'resize-y leading-normal', !shown && 'masked')}
          />
        )}
        <Button
          className="border border-rule-hard px-2.5"
          aria-label={shown ? 'Hide the value' : 'Show the value'}
          onClick={() => {
            setShown(!shown);
          }}
        >
          {shown ? 'HIDE' : 'SHOW'}
        </Button>
      </div>
    </FieldShell>
  );
}

export interface SelectOption {
  value: string;
  label: string;
}

export interface SelectFieldProps {
  id: string;
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  options: readonly SelectOption[];
  hint?: ReactNode;
  required?: boolean;
}

export function SelectField({
  id,
  label,
  value,
  onChange,
  options,
  hint,
  required,
}: SelectFieldProps) {
  return (
    <FieldShell label={label} htmlFor={id} required={required} hint={hint}>
      <select
        id={id}
        value={value}
        onChange={(event) => {
          onChange(event.target.value);
        }}
        className={cn(CONTROL, MONO_CONTROL, 'cursor-pointer')}
      >
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
    </FieldShell>
  );
}

export interface RichSelectOption {
  value: string;
  label: ReactNode;
}

export interface RichSelectFieldProps {
  id: string;
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  options: readonly RichSelectOption[];
  hint?: ReactNode;
  required?: boolean;
}

/** A select whose options carry markup (icons, secondary text), which a native `<option>` cannot. */
export function RichSelectField({
  id,
  label,
  value,
  onChange,
  options,
  hint,
  required,
}: RichSelectFieldProps) {
  const items = options.map((option) => ({ value: option.value, label: option.label }));
  return (
    <FieldShell label={label} htmlFor={id} required={required} hint={hint}>
      <Select.Root
        items={items}
        value={value}
        onValueChange={(next: string | null) => {
          onChange(next ?? '');
        }}
      >
        <Select.Trigger
          id={id}
          className={cn(
            CONTROL,
            MONO_CONTROL,
            'flex cursor-pointer items-center justify-between gap-2 text-left'
          )}
        >
          <Select.Value className="flex min-w-0 items-center gap-2 truncate" />
          <Select.Icon className="text-ink-3">▾</Select.Icon>
        </Select.Trigger>
        <Select.Portal>
          <Select.Positioner className="z-50 outline-none" sideOffset={2}>
            <Select.Popup className="max-h-72 w-[var(--anchor-width)] overflow-y-auto border border-rule-hard bg-surface py-1 shadow-lg">
              {options.map((option) => (
                <Select.Item
                  key={option.value}
                  value={option.value}
                  className={cn(
                    MONO_CONTROL,
                    'flex cursor-pointer items-center gap-2 px-2 py-1.5 text-ink outline-none data-[highlighted]:bg-hi'
                  )}
                >
                  <Select.ItemText className="flex min-w-0 items-center gap-2 truncate">
                    {option.label}
                  </Select.ItemText>
                </Select.Item>
              ))}
            </Select.Popup>
          </Select.Positioner>
        </Select.Portal>
      </Select.Root>
    </FieldShell>
  );
}

export interface PickListFieldProps {
  id: string;
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  options: readonly PickOption[];
  /** Starred values, listed first. */
  favorites: readonly string[];
  onToggleFavorite: (value: string) => void;
  filter: string;
  onFilterChange: (filter: string) => void;
  /** Fires when the filter input loses focus, for a caller that persists the filter. */
  onFilterSettled?: () => void;
  hint?: ReactNode;
  required?: boolean;
  placeholder?: string;
}

/** A filterable list with a star per entry. Starred entries sort first; the caller owns both the
 * stars and the filter text so it can remember them. */
export function PickListField({
  id,
  label,
  value,
  onChange,
  options,
  favorites,
  onToggleFavorite,
  filter,
  onFilterChange,
  onFilterSettled,
  hint,
  required,
  placeholder = 'filter',
}: PickListFieldProps) {
  const arranged = arrangeOptions(options, filter, favorites);
  const listId = `${id}-list`;
  return (
    <FieldShell label={label} htmlFor={id} required={required} hint={hint}>
      <div className="grid gap-1">
        <input
          id={id}
          type="search"
          role="combobox"
          aria-controls={listId}
          aria-expanded
          aria-autocomplete="list"
          value={filter}
          placeholder={placeholder}
          onChange={(event) => {
            onFilterChange(event.target.value);
          }}
          onBlur={onFilterSettled}
          className={cn(CONTROL, MONO_CONTROL)}
        />
        <ul
          id={listId}
          role="listbox"
          aria-label={typeof label === 'string' ? label : undefined}
          className="m-0 max-h-48 list-none overflow-y-auto border border-rule-hard bg-surface p-0"
        >
          {arranged.length === 0 ? (
            <li className={cn('px-2 py-1.5', MONO_CONTROL, 'text-ink-3')}>nothing matches</li>
          ) : (
            arranged.map((option) => {
              const selected = option.value === value;
              return (
                <li
                  key={option.value}
                  role="option"
                  aria-selected={selected}
                  data-value={option.value}
                  className={cn(
                    'flex items-center gap-1',
                    selected ? 'bg-hi text-ink' : 'text-ink-2 hover:bg-hi hover:text-ink'
                  )}
                >
                  <button
                    type="button"
                    aria-pressed={option.favorite}
                    aria-label={`${option.favorite ? 'Unstar' : 'Star'} ${option.label}`}
                    onClick={() => {
                      onToggleFavorite(option.value);
                    }}
                    className={cn(
                      'cursor-pointer border-0 bg-transparent px-2 py-1.5 text-data-lg leading-none',
                      option.favorite ? 'text-amber' : 'text-ink-3 hover:text-ink'
                    )}
                  >
                    {option.favorite ? '★' : '☆'}
                  </button>
                  <button
                    type="button"
                    onClick={() => {
                      onChange(option.value);
                    }}
                    className={cn(
                      MONO_CONTROL,
                      'flex-1 cursor-pointer truncate border-0 bg-transparent py-1.5 pr-2 text-left text-inherit'
                    )}
                  >
                    {option.label}
                  </button>
                </li>
              );
            })
          )}
        </ul>
      </div>
    </FieldShell>
  );
}

export interface CheckFieldProps {
  id: string;
  label: ReactNode;
  checked: boolean;
  onChange: (checked: boolean) => void;
  description?: ReactNode;
}

interface CheckProps {
  id?: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
  label?: string;
}

export function Check({ id, checked, onChange, label }: CheckProps) {
  return (
    <Checkbox.Root
      id={id}
      aria-label={label}
      checked={checked}
      onCheckedChange={onChange}
      className="flex size-3.5 shrink-0 cursor-pointer items-center justify-center border border-rule-hard bg-surface"
    >
      <Checkbox.Indicator className="size-2 bg-ink" />
    </Checkbox.Root>
  );
}

export function CheckField({ id, label, checked, onChange, description }: CheckFieldProps) {
  return (
    <div className="grid gap-1.5">
      <div className="flex items-center gap-2">
        <Check id={id} checked={checked} onChange={onChange} />
        <label htmlFor={id} className="cursor-pointer text-ink">
          {label}
        </label>
      </div>
      {description !== undefined && <p className={cn('m-0 pl-5.5', HINT)}>{description}</p>}
    </div>
  );
}

export interface NumberFieldProps {
  id: string;
  label: ReactNode;
  value: number;
  onChange: (value: number) => void;
  min?: number;
  max?: number;
  step?: number;
  hint?: ReactNode;
  error?: string | null;
}

export function NumberInputField({
  id,
  label,
  value,
  onChange,
  min,
  max,
  step = 1,
  hint,
  error,
}: NumberFieldProps) {
  return (
    <FieldShell label={label} htmlFor={id} hint={hint} error={error}>
      <NumberField.Root
        id={id}
        value={value}
        min={min}
        max={max}
        step={step}
        onValueChange={(next) => {
          if (next !== null) onChange(next);
        }}
      >
        <NumberField.Group className="flex w-fit border border-rule-hard bg-surface">
          <NumberField.Decrement className={cn(STEP, 'border-r border-rule')}>−</NumberField.Decrement>
          <NumberField.Input className="w-24 border-0 bg-transparent px-2 py-1.5 text-center font-mono text-data-lg text-ink" />
          <NumberField.Increment className={cn(STEP, 'border-l border-rule')}>+</NumberField.Increment>
        </NumberField.Group>
      </NumberField.Root>
    </FieldShell>
  );
}

export interface FormErrorProps {
  children: ReactNode;
  className?: string;
}

export function FormError({ children, className }: FormErrorProps) {
  return (
    <div
      role="alert"
      className={cn('border border-red bg-surface px-3 py-2 whitespace-pre-wrap text-red', className)}
    >
      {children}
    </div>
  );
}

export interface NoticeProps {
  label: string;
  children: ReactNode;
  className?: string;
}

/** A full-width band stating what a privileged action will do. */
export function Notice({ label, children, className }: NoticeProps) {
  return (
    <div className={cn('flex gap-3 border-b border-rule bg-sunk px-4.5 py-2.5', className)}>
      <Mono size="label" weight="semibold" uppercase tone="amber" className="mt-0.5 shrink-0 tracking-section">
        {label}
      </Mono>
      <p className="m-0 max-w-[80ch] text-ink-2">{children}</p>
    </div>
  );
}

export interface NoteProps {
  children: ReactNode;
  className?: string;
}

/** An inline note where a section would otherwise hold controls. */
export function Note({ children, className }: NoteProps) {
  return (
    <div className={cn('border border-rule bg-sunk px-3 py-2 text-data-lg text-ink-2', className)}>
      {children}
    </div>
  );
}

export interface FormActionsProps {
  children: ReactNode;
  className?: string;
}

export function FormActions({ children, className }: FormActionsProps) {
  return (
    <div className={cn('flex items-center gap-2.5 border-b border-rule-hard bg-surface px-4.5 py-3.5', className)}>
      {children}
    </div>
  );
}

export interface FormGridProps {
  children: ReactNode;
  className?: string;
}

export function FormGrid({ children, className }: FormGridProps) {
  return <div className={cn('grid max-w-[80ch] gap-4', className)}>{children}</div>;
}

interface RepoRowsProps {
  idPrefix: string;
  repos: readonly string[];
  onChangeAt: (index: number, value: string) => void;
  onAdd: () => void;
  onRemove: (index: number) => void;
}

export function useRepoRows() {
  const [repos, setRepos] = useState<string[]>(['']);
  const trimmed = repos.map((r) => r.trim()).filter((r) => r.length > 0);
  const onChangeAt = (i: number, v: string) => {
    setRepos((prev) => prev.map((r, idx) => (idx === i ? v : r)));
  };
  const onAdd = () => setRepos((prev) => [...prev, '']);
  const onRemove = (i: number) => setRepos((prev) => prev.filter((_, idx) => idx !== i));
  return { repos, trimmed, onChangeAt, onAdd, onRemove };
}

/** Affected repos: one required row plus any number of extras. */
export function RepoRows({ idPrefix, repos, onChangeAt, onAdd, onRemove }: RepoRowsProps) {
  return (
    <div className="grid gap-1.5">
      <span className="font-mono text-label font-semibold uppercase tracking-group text-ink-3">
        Affected repos<span className="ml-1 text-red">*</span>
      </span>
      {repos.map((repo, i) => (
        <div key={i} className="flex items-center gap-2">
          <BareTextInput
            id={`${idPrefix}-affected-repos-${i}`}
            aria-label={i === 0 ? 'Affected repo' : `Affected repo ${i + 1}`}
            value={repo}
            onChange={(value) => {
              onChangeAt(i, value);
            }}
            placeholder="Git URL or local path"
            mono
          />
          {repos.length > 1 && (
            <Button
              className="border border-rule-hard px-2.5"
              aria-label="Remove repo"
              onClick={() => {
                onRemove(i);
              }}
            >
              −
            </Button>
          )}
        </div>
      ))}
      <div>
        <Button className="border border-rule-hard px-2.5" onClick={onAdd}>
          + ADD REPO
        </Button>
      </div>
      <p className="m-0 max-w-[66ch] text-data-lg text-ink-3">
        Repos likely involved (best guess is fine — scoping can change this). The first one is where
        work starts.
      </p>
    </div>
  );
}

/// A labeled value row for metadata lists: label left in caps, value right, long values
/// breaking rather than overflowing. Wrap consecutive rows in a <dl>.
export function MetaRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-3 border-b border-rule py-1.5 last:border-b-0">
      <dt>
        <Mono size="label" tone="ink-3" uppercase>
          {label}
        </Mono>
      </dt>
      <dd className="m-0 min-w-0 text-right break-all">{children}</dd>
    </div>
  );
}
