import type { components } from '../api/schema';
import { isRecord } from '../json';

type LaunchPlaybookBody = components['schemas']['LaunchPlaybookBody'];
type ScheduleBody = components['schemas']['ScheduleBody'];
type SchedulePreviewDto = components['schemas']['SchedulePreviewDto'];

/// One rendered input, narrowed out of the pack's stored JSON Schema. The subset the engine's
/// `plan params` emits is closed — a string-typed property with an optional pattern, default and
/// doc string — so this is the whole vocabulary the form speaks.
export interface ParamFieldSpec {
  name: string;
  required: boolean;
  defaultValue: string | null;
  pattern: string | null;
  description: string | null;
}

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

  const specs: ParamFieldSpec[] = [];
  for (const [name, raw] of Object.entries(properties)) {
    if (!isRecord(raw)) {
      return { kind: 'unrenderable', reason: `param ${name} is not an object` };
    }
    const propertyType = raw.type;
    if (propertyType !== undefined && propertyType !== 'string') {
      return {
        kind: 'unrenderable',
        reason: `param ${name} is typed ${describe(propertyType)}; the form renders strings only`,
      };
    }
    const defaultValue = stringField(raw, 'default');
    if (!defaultValue.ok) {
      return { kind: 'unrenderable', reason: `param ${name} has a non-string default` };
    }
    const pattern = stringField(raw, 'pattern');
    if (!pattern.ok) {
      return { kind: 'unrenderable', reason: `param ${name} has a non-string pattern` };
    }
    const description = stringField(raw, 'description');
    specs.push({
      name,
      required: required.has(name),
      defaultValue: defaultValue.value,
      pattern: pattern.value,
      description: description.ok ? description.value : null,
    });
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
export function validateParam(spec: ParamFieldSpec, value: string): string | null {
  const trimmed = value.trim();
  if (trimmed.length === 0) {
    return spec.required ? `${spec.name} is required` : null;
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
    if (value.length === 0 && !spec.required) continue;
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
