// A run's admitted work graph (`GET /api/runs/{run_id}/graph`), folded into the graph document the
// shared workflow surface draws plus the runtime state only a run has. Pure functions so the
// folding is testable without a flow — same split as workflowGraphLayout.ts.

import type { components } from '../api/schema';
import type { WorkflowGraphDoc, WorkflowGraphEdge, WorkflowGraphNode } from './workflowGraphLayout';

export type PlanTask = components['schemas']['PlanTaskDto'];
export type TaskResult = components['schemas']['TaskResultDto'];
export type GraphOutput = components['schemas']['GraphOutputDto'];
export type OutputTarget = components['schemas']['OutputTargetDto'];
type TaskKind = WorkflowGraphNode['kind'];
type Needs = WorkflowGraphNode['needs'];

/// How a task's node reads: it passed, it failed, or nothing terminal is known about it. `none`
/// covers both "never ran" and the non-verdict statuses (skipped/blocked/truncated) — neither is a
/// result worth colouring as one.
export type TaskTone = 'pass' | 'fail' | 'none';

/// What a run knows about a task that a compiled plan cannot: how it ended, how many attempts it
/// took, and what those attempts spent.
export interface TaskRuntime {
  tone: TaskTone;
  status: string;
  /// The latest iteration the task reported in.
  latestIter: number;
  attempts: number;
  /// Summed across attempts; null when no attempt carried a figure.
  costUsd: number | null;
  secs: number | null;
  /// The latest attempt's payload.
  note: string;
}

/// Where a bound came from. `unknown` is a stored exposure that predates the field, treated as
/// declared: hiding a bound would show less than the pack may write.
export type OutputSource = 'manifest' | 'engine-default' | 'unknown';

/// A declared output drawn as a terminal node, keyed in `RunGraphView.outputs` by the node name it
/// was given. `undeclared` marks the one node a revision with no stored exposure gets: the graph
/// says the disclosure is missing rather than drawing nothing.
export interface OutputNode {
  kind: string;
  count: number;
  target: OutputTarget | null;
  attachedTo: string | null;
  undeclared: boolean;
  source: OutputSource;
}

function outputSource(raw: GraphOutput['source']): OutputSource {
  return raw === 'manifest' || raw === 'engine-default' ? raw : 'unknown';
}

/// The target as one line: the address, or the scope an address has to fall inside.
export function targetLabel(target: OutputTarget | null): string | null {
  if (target === null) return null;
  if (target.kind === 'address') return target.address;
  const param = target.param ?? null;
  return param === null ? target.scope : `${target.scope} (param ${param})`;
}

export interface RunGraphView {
  graph: WorkflowGraphDoc;
  runtime: ReadonlyMap<string, TaskRuntime>;
  /// The terminal output nodes in `graph`, by node name. Anything in here is drawn as an output,
  /// not as a task.
  outputs: ReadonlyMap<string, OutputNode>;
  /// Bounds the engine filled in for kinds the pack never declared. Listed, never drawn: they are
  /// not work the pack asked for.
  engineDefaults: OutputNode[];
}

/// The node name one declared bound gets. A task name can be anything, so the prefix and the index
/// keep these from colliding with one.
function outputName(kind: string, index: number): string {
  return `output:${kind}#${index}`;
}

/// The marker node for a revision whose exposure was never extracted.
export const UNDECLARED_OUTPUTS = 'output:undeclared';

/// The latest terminal status per task: the highest iteration wins, so a task retried across
/// iterations shows where it ended up rather than where it started.
export function latestResults(results: TaskResult[]): Map<string, TaskResult> {
  const latest = new Map<string, TaskResult>();
  for (const r of results) {
    const seen = latest.get(r.task);
    if (!seen || r.iter >= seen.iter) latest.set(r.task, r);
  }
  return latest;
}

export function toneOf(status: string | null): TaskTone {
  if (status === 'pass') return 'pass';
  if (status === 'fail' || status === 'transport') return 'fail';
  return 'none';
}

/// A reducer task is named after the reducer it runs (`top_k`), so a kind the graph document has no
/// case for is drawn as a plain task rather than dropped.
function kindOf(kind: string): TaskKind {
  if (kind === 'agent' || kind === 'command' || kind === 'engine') return kind;
  return 'other';
}

function needsOf(needs: string): Needs {
  if (needs === 'any' || needs === 'all') return needs;
  return 'other';
}

interface NodeSpec {
  name: string;
  kind: TaskKind;
  needs: Needs;
  required: boolean;
  session: string | null;
}

/// A plan task carries a fraction of what a compiled pack does: the rest of the document's fields
/// are absent, not empty, and the card leaves their lines out.
function nodeOf(spec: NodeSpec): WorkflowGraphNode {
  return {
    ...spec,
    join: 'all',
    isolation: null,
    emits: [],
    emits_files: [],
    fanout: null,
    harness: null,
    model: null,
    effort: null,
    prompt: null,
    command: null,
  };
}

export function sum(xs: (number | null | undefined)[]): number | null {
  const known = xs.filter((x): x is number => typeof x === 'number');
  return known.length === 0 ? null : known.reduce((a, b) => a + b, 0);
}

/// Results with `secs` dropped when no attempt in the run carried a positive figure. The plan
/// executor does not time tasks and reports `0.0` for every one, which is not a duration; a run
/// where nothing is timed reads as untimed rather than as instantaneous.
export function timedResults(results: TaskResult[]): TaskResult[] {
  const timed = results.some((r) => typeof r.secs === 'number' && r.secs > 0);
  return timed ? results : results.map((r) => ({ ...r, secs: null }));
}

function runtimeOf(attempts: TaskResult[], latest: TaskResult): TaskRuntime {
  return {
    tone: toneOf(latest.status),
    status: latest.status,
    latestIter: latest.iter,
    attempts: attempts.length,
    costUsd: sum(attempts.map((r) => r.cost_usd)),
    secs: sum(attempts.map((r) => r.secs)),
    note: latest.note,
  };
}

/// The mapped task an instance came from. The executor names an instance `node[item]` and a
/// declared name may not hold a bracket, so the prefix is the node it was mapped from.
export function mappedFrom(name: string): string | null {
  const cut = name.indexOf('[');
  if (cut <= 0 || !name.endsWith(']')) return null;
  return name.slice(0, cut);
}

/// Hide a dependency already implied by a longer path. The executor may retain such dependencies
/// for admission semantics, but drawing both routes makes the DAG read as though downstream work
/// starts directly from an ancestor.
function transitiveReduction(edges: WorkflowGraphEdge[]): WorkflowGraphEdge[] {
  const children = new Map<string, { to: string; edge: number }[]>();
  edges.forEach((edge, index) => {
    children.set(edge.from, [...(children.get(edge.from) ?? []), { to: edge.to, edge: index }]);
  });
  return edges.filter((edge, skipped) => {
    const pending = (children.get(edge.from) ?? [])
      .filter((child) => child.edge !== skipped)
      .map((child) => child.to);
    const seen = new Set<string>([edge.from]);
    while (pending.length > 0) {
      const name = pending.pop();
      if (name === undefined || seen.has(name)) continue;
      if (name === edge.to) return false;
      seen.add(name);
      pending.push(...(children.get(name) ?? []).map((child) => child.to));
    }
    return true;
  });
}

/// Fold the admitted plan and everything recorded against it into one graph document. Dependencies
/// on names the plan does not carry are dropped. A result whose task the plan never declared still
/// renders: an instance hangs off the task it was mapped from, and anything else stands alone.
export function runGraphView(
  tasks: PlanTask[],
  reported: TaskResult[],
  declaredOutputs: GraphOutput[] | null = [],
): RunGraphView {
  const results = timedResults(reported);
  const known = new Set(tasks.map((t) => t.name));
  const latest = latestResults(results);
  const declared = new Map(tasks.map((t) => [t.name, t]));
  const instances = new Map<string, string[]>();
  for (const name of latest.keys()) {
    if (known.has(name)) continue;
    const from = mappedFrom(name);
    if (from !== null && declared.has(from)) {
      instances.set(from, [...(instances.get(from) ?? []), name]);
    }
  }

  const nodes = tasks.map((t) =>
    nodeOf({
      name: t.name,
      kind: kindOf(t.kind),
      needs: needsOf(t.needs),
      required: t.required,
      session: t.session === '' ? null : t.session,
    }),
  );
  const declaredEdges: WorkflowGraphEdge[] = [];
  for (const t of tasks) {
    for (const dep of t.depends_on) {
      if (!known.has(dep)) continue;
      declaredEdges.push({
        from: dep,
        to: t.name,
        join: 'all',
        required: t.required,
      });
    }
  }
  // Core runs report tasks in the epilogue, after the ordinary task graph has finished. That
  // stage boundary is not carried by PlanTaskDto, so its root reports arrive with no dependency
  // and otherwise render as disconnected roots. Reconstruct the implicit boundary from every
  // ordinary sink to each root report.
  const producers = new Set(declaredEdges.map((edge) => edge.from));
  const sinks = tasks.filter((task) => task.kind !== 'report' && !producers.has(task.name));
  for (const report of tasks.filter(
    (task) => task.kind === 'report' && task.depends_on.length === 0,
  )) {
    for (const sink of sinks) {
      declaredEdges.push({
        from: sink.name,
        to: report.name,
        join: 'all',
        required: report.required,
      });
    }
  }
  const edges: WorkflowGraphEdge[] = [];
  for (const edge of transitiveReduction(declaredEdges)) {
    // Once a mapped task has concrete runtime instances, those instances are the leaves of its
    // subgraph. Draw downstream work after them instead of shortcutting directly from the
    // declared task node and making the consumer look like their sibling.
    const producers = instances.get(edge.from) ?? [edge.from];
    for (const producer of producers) {
      edges.push({ ...edge, from: producer });
    }
  }

  for (const name of latest.keys()) {
    if (known.has(name)) continue;
    const from = mappedFrom(name);
    const mapped = from === null ? undefined : declared.get(from);
    nodes.push(
      nodeOf({
        name,
        kind: mapped === undefined ? 'other' : kindOf(mapped.kind),
        needs: mapped === undefined ? 'any' : needsOf(mapped.needs),
        required: mapped?.required ?? true,
        session: null,
      }),
    );
    if (from !== null && mapped !== undefined) {
      edges.push({ from, to: name, join: 'all', required: mapped.required });
    }
  }

  const attempts = new Map<string, TaskResult[]>();
  for (const r of results) attempts.set(r.task, [...(attempts.get(r.task) ?? []), r]);
  const runtime = new Map<string, TaskRuntime>();
  for (const [name, last] of latest) runtime.set(name, runtimeOf(attempts.get(name) ?? [last], last));

  // A revision that stored no exposure (`declaredOutputs === null`) gets one marker node: an
  // unextracted pack must never read as a pack that writes nothing.
  const drawn = new Set(nodes.map((n) => n.name));
  const consumed = new Set(edges.map((edge) => edge.from));
  const terminals = [...drawn].filter((name) => !consumed.has(name));
  const outputs = new Map<string, OutputNode>();
  const attach = (name: string, from: string | null) => {
    const parents = from !== null && drawn.has(from) ? [from] : terminals;
    for (const parent of parents) {
      edges.push({ from: parent, to: name, join: 'all', required: true });
    }
  };
  if (declaredOutputs === null) {
    nodes.push(
      nodeOf({
        name: UNDECLARED_OUTPUTS,
        kind: 'other',
        needs: 'all',
        required: true,
        session: null,
      }),
    );
    outputs.set(UNDECLARED_OUTPUTS, {
      kind: 'outputs undeclared',
      count: 0,
      target: null,
      attachedTo: null,
      undeclared: true,
      source: 'unknown',
    });
    attach(UNDECLARED_OUTPUTS, null);
  }
  const engineDefaults: OutputNode[] = [];
  (declaredOutputs ?? []).forEach((output, index) => {
    const node: OutputNode = {
      kind: output.kind,
      count: output.count,
      target: output.target ?? null,
      attachedTo: output.attached_to ?? null,
      undeclared: false,
      source: outputSource(output.source),
    };
    if (node.source === 'engine-default') {
      engineDefaults.push(node);
      return;
    }
    const name = outputName(output.kind, index);
    nodes.push(
      nodeOf({ name, kind: 'other', needs: 'all', required: true, session: null }),
    );
    outputs.set(name, node);
    attach(name, output.attached_to ?? null);
  });

  return {
    graph: { workflow_type: 'run', result: null, nodes, edges },
    runtime,
    outputs,
    engineDefaults,
  };
}
