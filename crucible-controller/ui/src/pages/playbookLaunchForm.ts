import type { components } from '../api/schema';
import { isRecord } from '../json';

type LaunchPlaybookBody = components['schemas']['LaunchPlaybookBody'];
type ScheduleBody = components['schemas']['ScheduleBody'];
type SchedulePreviewDto = components['schemas']['SchedulePreviewDto'];

/// One rendered input, narrowed out of the pack's stored JSON Schema. The subset the engine's
/// `plan params` emits is closed — a string-typed property with an optional pattern, default and
/// doc string — so this is the whole vocabulary the form speaks.
/// The declared type, which decides the spelling a value is typed in: a string as itself, an int or
/// number as its digits, a bool as true or false, a list as a JSON array or comma-separated.
export type ParamValueType = 'string' | 'integer' | 'number' | 'boolean' | 'list';

export interface ParamFieldSpec {
  name: string;
  valueType: ParamValueType;
  required: boolean;
  defaultValue: string | null;
  pattern: string | null;
  description: string | null;
  /// Typed in a text area: a string that may span lines, such as a pasted log.
  multiline: boolean;
  /// Picked from a list rather than typed: a declared choice set, or true and false for a bool.
  choices: string[] | null;
  /// The bool param that must be true for this one to be supplied; greyed out and omitted otherwise.
  enabledBy: string | null;
  /// The task output the choices come from, and the latest passing run's value of it.
  choicesFrom: ChoicesSource | null;
  choicesSnapshot: Snapshot<string[] | Record<string, string[]>> | null;
  /// The choices are suggestions; any value is accepted.
  allowOther: boolean;
  /// The task output whose value for the key param shows greyed out in the empty input.
  placeholderFrom: ChoicesSource | null;
  placeholderSnapshot: Snapshot<Record<string, string>> | null;
}

export interface ChoicesSource {
  task: string;
  field: string;
  /// The string param whose value selects the entry of a keyed output.
  keyedBy: string | null;
}

export interface Snapshot<T> {
  value: T;
  capturedAt: string | null;
}

/// The launch setting that runs only the tasks feeding the form's options.
export const CHOICES_REFRESH = 'choices.refresh';

/// A schema outside the closed subset renders nothing rather than a wrong form: the launch
/// endpoint validates against the same document, so a form that dropped or mistyped a field would
/// only produce refusals the launcher cannot act on.
export type ParsedParamsSchema =
  | { kind: 'form'; specs: ParamFieldSpec[] }
  | { kind: 'unrenderable'; reason: string };

function describe(value: unknown): string {
  return typeof value === 'string' ? value : JSON.stringify(value);
}

function stringField(
  property: Record<string, unknown>,
  key: string
): { ok: true; value: string | null } | { ok: false } {
  const raw = property[key];
  if (raw === undefined || raw === null) return { ok: true, value: null };
  if (typeof raw !== 'string') return { ok: false };
  return { ok: true, value: raw };
}

function valueTypeOf(property: Record<string, unknown>): ParamValueType | null {
  switch (property.type) {
    case undefined:
    case 'string':
      return 'string';
    case 'integer':
      return 'integer';
    case 'number':
      return 'number';
    case 'boolean':
      return 'boolean';
    case 'array': {
      const items = property.items;
      return items === undefined || (isRecord(items) && items.type === 'string') ? 'list' : null;
    }
    default:
      return null;
  }
}

function choicesSource(raw: unknown): ChoicesSource | null | 'invalid' {
  if (raw === undefined || raw === null) return null;
  if (!isRecord(raw) || typeof raw.task !== 'string' || typeof raw.field !== 'string') return 'invalid';
  if (raw.keyed_by !== undefined && typeof raw.keyed_by !== 'string') return 'invalid';
  return { task: raw.task, field: raw.field, keyedBy: raw.keyed_by ?? null };
}

const isStringList = (value: unknown): value is string[] =>
  Array.isArray(value) && value.every((item) => typeof item === 'string');

/// A snapshot the server attached, when its value has the shape `accepts` admits. A malformed one
/// is dropped: the launch endpoint checks the value against the snapshot it holds either way.
function snapshot<T>(raw: unknown, accepts: (value: unknown) => value is T): Snapshot<T> | null {
  if (!isRecord(raw) || !accepts(raw.value)) return null;
  return { value: raw.value, capturedAt: typeof raw.captured_at === 'string' ? raw.captured_at : null };
}

function keyedOf<T>(accepts: (value: unknown) => value is T) {
  return (value: unknown): value is Record<string, T> =>
    isRecord(value) && Object.values(value).every((entry) => accepts(entry));
}

type ChoicesValue = string[] | Record<string, string[]>;

/// The shape a choices snapshot must have: one list, or one list per value of the key param.
function choicesShape(keyed: boolean) {
  const keyedLists = keyedOf(isStringList);
  return (value: unknown): value is ChoicesValue => (keyed ? keyedLists(value) : isStringList(value));
}

const isKeyedText = keyedOf((value: unknown): value is string => typeof value === 'string');

function boolField(
  property: Record<string, unknown>,
  key: string
): { ok: true; value: boolean } | { ok: false } {
  const raw = property[key];
  if (raw === undefined) return { ok: true, value: false };
  if (typeof raw !== 'boolean') return { ok: false };
  return { ok: true, value: raw };
}

/// A declared default in the spelling the form types it in.
function defaultText(
  raw: unknown,
  valueType: ParamValueType
): { ok: true; value: string | null } | { ok: false } {
  if (raw === undefined || raw === null) return { ok: true, value: null };
  switch (valueType) {
    case 'string':
      return typeof raw === 'string' ? { ok: true, value: raw } : { ok: false };
    case 'integer':
    case 'number':
      return typeof raw === 'number' ? { ok: true, value: String(raw) } : { ok: false };
    case 'boolean':
      return typeof raw === 'boolean' ? { ok: true, value: String(raw) } : { ok: false };
    case 'list':
      return Array.isArray(raw) && raw.every((item) => typeof item === 'string')
        ? { ok: true, value: JSON.stringify(raw) }
        : { ok: false };
  }
}

/// What a value of this type is typed as, for a field hint and a refusal.
export function spellingOf(valueType: ParamValueType): string | null {
  switch (valueType) {
    case 'string':
      return null;
    case 'integer':
      return 'a whole number';
    case 'number':
      return 'a number';
    case 'boolean':
      return 'true or false';
    case 'list':
      return 'a JSON array of strings, or comma-separated';
  }
}

/// Whether `text` reads as `valueType`, mirroring the engine's reader. The endpoint checks again with
/// the reader itself.
function readsAs(valueType: ParamValueType, text: string): boolean {
  switch (valueType) {
    case 'string':
      return true;
    case 'integer':
      return /^[+-]?\d+$/.test(text) && Math.abs(Number(text)) <= 2147483647;
    case 'number':
      return text.length > 0 && Number.isFinite(Number(text));
    case 'boolean':
      return ['true', 'True', 'false', 'False'].includes(text);
    case 'list':
      if (!text.trimStart().startsWith('[')) return true;
      try {
        const parsed: unknown = JSON.parse(text);
        return Array.isArray(parsed) && parsed.every((item) => typeof item === 'string');
      } catch {
        return false;
      }
  }
}

export function parseParamsSchema(schema: unknown): ParsedParamsSchema {
  if (!isRecord(schema)) return { kind: 'unrenderable', reason: 'the schema is not an object' };
  const type = schema.type;
  if (type !== undefined && type !== 'object') {
    return { kind: 'unrenderable', reason: `the schema declares type ${describe(type)}, not object` };
  }
  const properties = schema.properties;
  if (properties === undefined) return { kind: 'form', specs: [] };
  if (!isRecord(properties)) {
    return { kind: 'unrenderable', reason: 'the schema properties are not an object' };
  }
  const requiredRaw = schema.required;
  if (requiredRaw !== undefined && !(Array.isArray(requiredRaw) && requiredRaw.every((r) => typeof r === 'string'))) {
    return { kind: 'unrenderable', reason: 'the schema required list is not an array of names' };
  }
  const required = new Set<string>(requiredRaw ?? []);
  const orderRaw = schema['x-order'];
  if (orderRaw !== undefined && !(Array.isArray(orderRaw) && orderRaw.every((n) => typeof n === 'string'))) {
    return { kind: 'unrenderable', reason: 'the schema x-order is not an array of names' };
  }

  const specs: ParamFieldSpec[] = [];
  for (const [name, raw] of Object.entries(properties)) {
    if (!isRecord(raw)) {
      return { kind: 'unrenderable', reason: `param ${name} is not an object` };
    }
    const valueType = valueTypeOf(raw);
    if (valueType === null) {
      return {
        kind: 'unrenderable',
        reason: `param ${name} is typed ${describe(raw.type)}; the form renders strings, numbers, bools, and lists of strings`,
      };
    }
    const defaultValue = defaultText(raw.default, valueType);
    if (!defaultValue.ok) {
      return { kind: 'unrenderable', reason: `param ${name} has a default that is not its type` };
    }
    const pattern = stringField(raw, 'pattern');
    if (!pattern.ok) {
      return { kind: 'unrenderable', reason: `param ${name} has a non-string pattern` };
    }
    const multiline = boolField(raw, 'x-multiline');
    if (!multiline.ok) {
      return { kind: 'unrenderable', reason: `param ${name} has a non-boolean x-multiline` };
    }
    if (multiline.value && valueType !== 'string') {
      return { kind: 'unrenderable', reason: `param ${name} is multiline but not a string` };
    }
    let choices: string[] | null = valueType === 'boolean' ? ['true', 'false'] : null;
    if (raw.enum !== undefined) {
      const spelled = Array.isArray(raw.enum) && valueType !== 'list' ? raw.enum.map((item) => defaultText(item, valueType)) : [];
      if (spelled.length === 0 || spelled.some((item) => !item.ok || item.value === null)) {
        return { kind: 'unrenderable', reason: `param ${name} has choices that are not a nonempty list of its type` };
      }
      choices = spelled.map((item) => (item.ok ? (item.value ?? '') : ''));
    }
    const enabledBy = stringField(raw, 'x-enabled-by');
    if (!enabledBy.ok) {
      return { kind: 'unrenderable', reason: `param ${name} has a non-string x-enabled-by` };
    }
    const gate = enabledBy.value === null ? undefined : properties[enabledBy.value];
    if (enabledBy.value !== null && !(isRecord(gate) && gate.type === 'boolean')) {
      return { kind: 'unrenderable', reason: `param ${name} is enabled by ${enabledBy.value}, which is not a declared bool` };
    }
    const choicesFrom = choicesSource(raw['x-choices-from']);
    const placeholderFrom = choicesSource(raw['x-placeholder-from']);
    if (choicesFrom === 'invalid' || placeholderFrom === 'invalid') {
      return { kind: 'unrenderable', reason: `param ${name} names its options' task output in a shape this form does not read` };
    }
    for (const source of [choicesFrom, placeholderFrom]) {
      const key = source?.keyedBy;
      if (key !== undefined && key !== null && !(isRecord(properties[key]) && valueTypeOf(properties[key]) === 'string')) {
        return { kind: 'unrenderable', reason: `param ${name} is keyed by ${key}, which is not a declared string` };
      }
    }
    const allowOther = boolField(raw, 'x-allow-other');
    if (!allowOther.ok) {
      return { kind: 'unrenderable', reason: `param ${name} has a non-boolean x-allow-other` };
    }
    const keyedChoices = choicesFrom?.keyedBy !== undefined && choicesFrom.keyedBy !== null;
    const description = stringField(raw, 'description');
    specs.push({
      name,
      valueType,
      required: required.has(name),
      defaultValue: defaultValue.value,
      pattern: pattern.value,
      description: description.ok ? description.value : null,
      multiline: multiline.value,
      choices,
      enabledBy: enabledBy.value,
      choicesFrom,
      choicesSnapshot:
        choicesFrom === null ? null : snapshot(raw['x-choices-snapshot'], choicesShape(keyedChoices)),
      allowOther: allowOther.value,
      placeholderFrom,
      placeholderSnapshot: placeholderFrom === null ? null : snapshot(raw['x-placeholder-snapshot'], isKeyedText),
    });
  }
  if (orderRaw !== undefined) {
    const rank = new Map<string, number>(orderRaw.map((name: string, index: number) => [name, index]));
    const last = orderRaw.length;
    specs.sort((a, b) => (rank.get(a.name) ?? last) - (rank.get(b.name) ?? last));
  }
  return { kind: 'form', specs };
}

/// The form's starting values: every declared param present, defaults filled in, and a prefill
/// (a relaunch's frozen snapshot) winning over both.
export function initialValues(
  specs: readonly ParamFieldSpec[],
  prefill?: Readonly<Record<string, string>>
): Record<string, string> {
  const values: Record<string, string> = {};
  for (const spec of specs) {
    const prefilled = prefill?.[spec.name];
    values[spec.name] = prefilled ?? spec.defaultValue ?? '';
  }
  return values;
}

/// Client-side convenience only. An unanchored `new RegExp` matches JSON Schema semantics, and a
/// pattern the browser cannot compile (Rust-regex syntax the engine accepts) checks nothing here
/// rather than failing a value the endpoint would have taken.
/// Whether a gated param's switch is on, so its value is taken; an ungated param always is.
/// One block of the rendered form: a lone field, or a boolean gate with the fields it enables.
export type ParamBlock =
  | { kind: 'field'; spec: ParamFieldSpec }
  | { kind: 'gated'; gate: ParamFieldSpec; fields: ParamFieldSpec[] };

/// The form's blocks in declaration order. A boolean another field names in enabled_by renders once,
/// as the head of a group at its first gated field's position, so the gate sits with what it unlocks.
export function paramBlocks(specs: readonly ParamFieldSpec[]): ParamBlock[] {
  const gates = new Map(
    specs
      .filter((spec) => specs.some((other) => other.enabledBy === spec.name))
      .map((spec) => [spec.name, spec])
  );
  const blocks: ParamBlock[] = [];
  const groups = new Map<string, ParamFieldSpec[]>();
  for (const spec of specs) {
    if (gates.has(spec.name)) continue;
    const gate = spec.enabledBy === null ? undefined : gates.get(spec.enabledBy);
    if (gate === undefined) {
      blocks.push({ kind: 'field', spec });
      continue;
    }
    const fields = groups.get(gate.name);
    if (fields === undefined) {
      const group: ParamFieldSpec[] = [spec];
      groups.set(gate.name, group);
      blocks.push({ kind: 'gated', gate, fields: group });
    } else {
      fields.push(spec);
    }
  }
  return blocks;
}

/// The values the form offers for this param right now: its declared choices, or the latest
/// snapshot of the task output it reads, narrowed to the key param's value when it is keyed. Null
/// when there is no list to offer, so the param is typed.
export function offeredChoices(
  spec: ParamFieldSpec,
  values: Readonly<Record<string, string>>
): string[] | null {
  if (spec.choices !== null) return spec.choices;
  const snap = spec.choicesSnapshot;
  if (snap === null) return null;
  if (Array.isArray(snap.value)) return snap.value;
  const key = spec.choicesFrom?.keyedBy;
  return key === undefined || key === null ? null : (snap.value[(values[key] ?? '').trim()] ?? []);
}

/// What the empty input shows greyed out: the pack's own pick for the key param's value.
export function placeholderFor(spec: ParamFieldSpec, values: Readonly<Record<string, string>>): string | null {
  const key = spec.placeholderFrom?.keyedBy;
  if (spec.placeholderSnapshot === null || key === undefined || key === null) return null;
  return spec.placeholderSnapshot.value[(values[key] ?? '').trim()] ?? null;
}

/// How long ago a snapshot was captured, coarsely: the form shows it so a stale list is noticed.
export function snapshotAge(capturedAt: string | null, now: Date = new Date()): string | null {
  if (capturedAt === null) return null;
  const at = new Date(capturedAt).getTime();
  if (Number.isNaN(at)) return null;
  const minutes = Math.max(0, Math.floor((now.getTime() - at) / 60000));
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours} h ago`;
  return `${Math.floor(hours / 24)} days ago`;
}

/// Whether the pack has options computed by a task, which a refresh launch recomputes.
export function refreshable(specs: readonly ParamFieldSpec[]): boolean {
  return specs.some((spec) => spec.choicesFrom !== null || spec.placeholderFrom !== null);
}

export function isEnabled(spec: ParamFieldSpec, values: Readonly<Record<string, string>>): boolean {
  return spec.enabledBy === null || (values[spec.enabledBy] ?? '').trim().toLowerCase() === 'true';
}

export function validateParam(
  spec: ParamFieldSpec,
  value: string,
  values?: Readonly<Record<string, string>>
): string | null {
  if (values !== undefined && !isEnabled(spec, values)) return null;
  const trimmed = value.trim();
  if (trimmed.length === 0) {
    return spec.required ? `${spec.name} is required` : null;
  }
  if (!readsAs(spec.valueType, trimmed)) {
    return `${spec.name} must be ${spellingOf(spec.valueType) ?? 'text'}`;
  }
  const offered = spec.allowOther ? null : offeredChoices(spec, values ?? {});
  if (offered !== null && !offered.includes(spec.valueType === 'boolean' ? trimmed.toLowerCase() : trimmed)) {
    return offered.length === 0
      ? `${spec.name} has no options for this ${spec.choicesFrom?.keyedBy ?? 'launch'}`
      : `${spec.name} must be one of ${offered.join(', ')}`;
  }
  if (spec.pattern === null) return null;
  let re: RegExp;
  try {
    re = new RegExp(spec.pattern);
  } catch {
    return null;
  }
  return re.test(trimmed) ? null : `${spec.name} must match ${spec.pattern}`;
}

export interface LaunchCeilings {
  maxCost: number;
  maxTime: string;
  schemaDigest: string;
  /// The cluster to dispatch onto. Blank omits the field, which selects the controller's default.
  dispatchTarget?: string;
}

/// The declared params, trimmed. Blank optional params are omitted, not sent empty: a pack that
/// declares a pattern would refuse the empty string, and an absent optional is what "unset" means.
export function trimmedParams(
  specs: readonly ParamFieldSpec[],
  values: Readonly<Record<string, string>>
): Record<string, string> {
  const params: Record<string, string> = {};
  for (const spec of specs) {
    const value = (values[spec.name] ?? '').trim();
    if (!isEnabled(spec, values) || (value.length === 0 && !spec.required)) continue;
    params[spec.name] = value;
  }
  return params;
}

/// Build the launch POST body.
export function launchBody(
  specs: readonly ParamFieldSpec[],
  values: Readonly<Record<string, string>>,
  ceilings: LaunchCeilings
): LaunchPlaybookBody {
  const params = trimmedParams(specs, values);
  const target = (ceilings.dispatchTarget ?? '').trim();
  return {
    params,
    max_cost: ceilings.maxCost,
    max_time: ceilings.maxTime.trim(),
    schema_digest: ceilings.schemaDigest,
    ...(target === '' ? {} : { dispatch_target: target }),
  };
}

/// The launch POST body of a refresh: no params, only the setting that runs the option tasks.
export function refreshBody(ceilings: LaunchCeilings): LaunchPlaybookBody {
  const target = (ceilings.dispatchTarget ?? '').trim();
  return {
    params: { [CHOICES_REFRESH]: 'true' },
    max_cost: ceilings.maxCost,
    max_time: ceilings.maxTime.trim(),
    schema_digest: ceilings.schemaDigest,
    ...(target === '' ? {} : { dispatch_target: target }),
  };
}

export interface ServerRejection {
  fieldErrors: Map<string, string>;
  general: string | null;
}

function generalOnly(message: string): ServerRejection {
  return { fieldErrors: new Map(), general: message };
}

/// Split a refused launch into the messages that belong on inputs and the ones that do not. A
/// rejection naming a field this form does not render (a digest that drifted under the launcher)
/// still reaches the launcher, in the general block.
export function mapServerRejection(err: unknown, fields: readonly string[]): ServerRejection {
  if (!isRecord(err)) {
    return generalOnly(typeof err === 'string' ? err : 'Unknown error');
  }
  const error = typeof err.error === 'string' ? err.error : null;
  const raw = err.fields;
  if (!Array.isArray(raw)) {
    return generalOnly(error ?? (err instanceof Error ? err.message : JSON.stringify(err)));
  }
  const known = new Set(fields);
  const fieldErrors = new Map<string, string>();
  const orphans: string[] = [];
  for (const entry of raw) {
    if (!isRecord(entry)) continue;
    const { field, message } = entry;
    if (typeof message !== 'string') continue;
    const name = typeof field === 'string' ? field : '';
    if (name.length > 0 && known.has(name) && !fieldErrors.has(name)) {
      fieldErrors.set(name, message);
    } else {
      orphans.push(name.length > 0 ? `${name}: ${message}` : message);
    }
  }
  const general = orphans.length > 0 ? orphans.join('\n') : fieldErrors.size > 0 ? null : error;
  return { fieldErrors, general };
}

/// Hold a ceiling inside the admin cap. The endpoint refuses anything above it, so the input
/// never offers a value that cannot be launched.
export function clampCeiling(input: number, cap: number | null): number {
  if (!Number.isFinite(input) || input <= 0) return 0;
  if (cap === null || !Number.isFinite(cap)) return input;
  return Math.min(input, cap);
}

/// The engine's duration grammar (`90s`, `30m`, `2h`, a bare count of seconds), in seconds. Null
/// for anything outside it, which is exactly what `MaxTime::parse` refuses.
export function parseMaxTime(text: string): number | null {
  const match = /^(\d+)([smh]?)$/.exec(text.trim());
  if (match === null) return null;
  const count = Number(match[1]);
  if (count <= 0) return null;
  const unit = match[2];
  const scale = unit === 'h' ? 3600 : unit === 'm' ? 60 : 1;
  return count * scale;
}

export function validateMaxTime(value: string, cap: string | null): string | null {
  const secs = parseMaxTime(value);
  if (secs === null) return 'max_time takes a duration like 90s, 30m or 2h';
  if (cap === null) return null;
  const capSecs = parseMaxTime(cap);
  if (capSecs !== null && secs > capSecs) return `max_time is above this controller's cap of ${cap}`;
  return null;
}

/// What the form fires: one launch now, or a schedule that fires it repeatedly. The values and
/// ceilings are the same document either way.
export type LaunchMode = 'now' | 'schedule';

/// The recurring half of a scheduled launch.
export interface Recurrence {
  playbook: string;
  cronExpr: string;
  tz: string;
}

const CRON_FIELDS = 5;

/// Client-side convenience only, and only for the field count: the endpoint parses the grammar
/// (ranges, steps, lists, `L`, `#`) and answers for anything this shape check lets through.
export function validateCronExpr(expr: string): string | null {
  const fields = expr.trim().split(/\s+/).filter((field) => field.length > 0);
  if (fields.length === 0) return 'cron_expr is required';
  if (fields.length !== CRON_FIELDS) {
    return `cron_expr takes five fields (minute hour day month weekday), not ${fields.length}`;
  }
  return null;
}

/// Build the schedule POST body. Values, ceilings and digest come from the same document a
/// run-now launch posts, so a schedule cannot be authorized on anything the launch path would
/// have refused.
export function scheduleBody(
  specs: readonly ParamFieldSpec[],
  values: Readonly<Record<string, string>>,
  ceilings: LaunchCeilings,
  recurrence: Recurrence
): ScheduleBody {
  const launch = launchBody(specs, values, ceilings);
  return {
    playbook: recurrence.playbook,
    params: launch.params,
    max_cost: launch.max_cost,
    max_time: launch.max_time,
    schema_digest: launch.schema_digest,
    ...(launch.dispatch_target === undefined ? {} : { dispatch_target: launch.dispatch_target }),
    cron_expr: recurrence.cronExpr.trim(),
    tz: recurrence.tz.trim(),
  };
}

/// Whether a preview describes the recurrence now in the form. Firings are computed server-side,
/// so one taken against a different expression or zone says nothing about what this schedule would
/// do, and the form withholds the commit until a matching preview is in hand.
export function previewMatches(
  preview: SchedulePreviewDto | null,
  recurrence: Recurrence
): boolean {
  if (preview === null) return false;
  return (
    preview.cron_expr === recurrence.cronExpr.trim() && preview.tz === recurrence.tz.trim()
  );
}

/// The zone the browser is in, as the form's starting guess. The endpoint reads the expression in
/// whatever zone is posted, so this is a default, not an interpretation.
export function browserTimeZone(): string {
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  return zone.length > 0 ? zone : 'UTC';
}

/// A launch's frozen authorization, as form state.
export interface RunSnapshot {
  values: Record<string, string>;
  maxCost: number;
  maxTime: string;
}

/// Read a relaunch's prefill out of a launch row. `params` is opaque JSONB on the wire, so string
/// values are narrowed out of it and anything else is dropped rather than stringified into an
/// input the relaunch would then post back as a value nobody wrote.
export function runSnapshot(run: {
  params: unknown;
  max_cost: number;
  max_time: string;
}): RunSnapshot {
  const values: Record<string, string> = {};
  if (isRecord(run.params)) {
    for (const [name, value] of Object.entries(run.params)) {
      if (typeof value === 'string') values[name] = value;
    }
  }
  return { values, maxCost: run.max_cost, maxTime: run.max_time };
}
