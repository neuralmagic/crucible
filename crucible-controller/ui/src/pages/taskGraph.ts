// A run's admitted work graph (`GET /api/runs/{run_id}/graph`), folded into the graph document the
// shared workflow surface draws plus the runtime state only a run has. Pure functions so the
// folding is testable without a flow — same split as workflowGraphLayout.ts.

import type { components } from '../api/schema';
import { shownLinks, type ExternalLinkRef } from '../ui/ExternalLink';
import type { WorkflowGraphDoc, WorkflowGraphEdge, WorkflowGraphNode } from './workflowGraphLayout';

export type PlanTask = components['schemas']['PlanTaskDto'];
export type TaskResult = components['schemas']['TaskResultDto'];
export type GraphOutput = components['schemas']['GraphOutputDto'];
export type OutputTarget = components['schemas']['OutputTargetDto'];
export type FanOutCount = components['schemas']['FanOutCountDto'];
export type RouteDecision = components['schemas']['RouteDecisionDto'];
export type When = components['schemas']['WhenDto'];
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
  /// False for a declared task no attempt was ever recorded against: `status` then says so, and
  /// every figure below it is empty rather than zero.
  reported: boolean;
  /// The latest iteration the task reported in.
  latestIter: number;
  attempts: number;
  /// Summed across attempts; null when no attempt carried a figure.
  costUsd: number | null;
  secs: number | null;
  /// The latest attempt's payload.
  note: string;
}

/// One status a mapped task's instances reported in, and how many reported it.
export interface FanOutStatus {
  status: string;
  count: number;
}

/// A mapped task against what the run made of it: how wide it was spread, and how the instances
/// that started ended up. `items` is null when the session cannot say how wide.
export interface FanOutState {
  items: number | null;
  started: number;
  passed: number;
  /// Instances whose `when` left them out. Not a failure, and counted apart from one.
  notTaken: number;
  /// Everything that reported something other than a pass or not taken, by the status it
  /// reported: a blocked or skipped instance is neither a pass nor a failure, and dropping it loses
  /// the run's work.
  other: FanOutStatus[];
  /// Whether the run can still start the instances that have not.
  running: boolean;
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
  /// What each mapped task's fan-out came to, by the mapped task's name.
  fanout: ReadonlyMap<string, FanOutState>;
  /// The external results each node links out to, by node name.
  links: ReadonlyMap<string, NodeLinks>;
  /// What each route node or route instance decided, by its name.
  decisions: ReadonlyMap<string, RouteDecision[]>;
}

/// Everything the run graph endpoint answers with, plus whether the run is still going — which is
/// what tells a task with no result apart from one that will never get one.
export interface RunGraphSource {
  tasks: PlanTask[];
  results: TaskResult[];
  /// Null for a revision that stored no exposure.
  outputs: GraphOutput[] | null;
  fanout: FanOutCount[];
  decisions: RouteDecision[];
  running: boolean;
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
  if (kind === 'agent' || kind === 'command' || kind === 'engine' || kind === 'route') return kind;
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
  fanout?: WorkflowGraphNode['fanout'];
  keyed?: string[];
  when?: When | null;
  questions?: string[];
}

/// What a plan task maps over, as the graph document carries it. The wire writes `producer.field`
/// in one string and 0 for an uncapped mapping.
export function fanoutOf(task: PlanTask): WorkflowGraphNode['fanout'] {
  const over = task.over;
  if (over === '') return null;
  const cut = over.lastIndexOf('.');
  if (cut <= 0) return null;
  return {
    over_task: over.slice(0, cut),
    over_field: over.slice(cut + 1),
    max_fanout: task.max_fanout === 0 ? null : task.max_fanout,
  };
}

/// A plan task's `when`, which the wire writes as `route.question in a|b`.
export function whenOf(task: PlanTask): When | null {
  const [predicate, answers, ...rest] = task.when.split(' in ');
  if (predicate === undefined || answers === undefined || rest.length > 0) return null;
  const cut = predicate.lastIndexOf('.');
  if (cut <= 0) return null;
  return {
    route: predicate.slice(0, cut),
    question: predicate.slice(cut + 1),
    labels: answers.split('|'),
  };
}

/// One dependency as the graph document draws it: its consumer's join, the `when` it carries when
/// the dependency is the route the consumer reads, whether both ends map over the same list, and
/// which of the dependency's fields the consumer narrows per instance.
export function planEdge(dep: PlanTask, task: PlanTask): WorkflowGraphEdge {
  const when = whenOf(task);
  return {
    from: dep.name,
    to: task.name,
    join: 'all',
    required: task.required,
    when: when !== null && when.route === dep.name ? when : null,
    aligned: task.over !== '' && task.over === dep.over,
    keyed: task.keyed
      .filter((ref) => ref.startsWith(`${dep.name}.`))
      .map((ref) => ref.slice(dep.name.length + 1)),
  };
}

/// An edge that carries nothing but the dependency itself.
function plainEdge(from: string, to: string, required: boolean): WorkflowGraphEdge {
  return { from, to, join: 'all', required, when: null, aligned: false, keyed: [] };
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
    fanout: spec.fanout ?? null,
    keyed: spec.keyed ?? [],
    when: spec.when ?? null,
    questions: spec.questions ?? [],
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
    reported: true,
    latestIter: latest.iter,
    attempts: attempts.length,
    costUsd: sum(attempts.map((r) => r.cost_usd)),
    secs: sum(attempts.map((r) => r.secs)),
    note: latest.note,
  };
}

/// The state of a declared task nothing reported on. A run still going may yet get to it; a run
/// that is over never will, and the card has to say which.
function unreportedRuntime(running: boolean): TaskRuntime {
  return {
    tone: 'none',
    status: running ? 'pending' : 'never ran',
    reported: false,
    latestIter: 0,
    attempts: 0,
    costUsd: null,
    secs: null,
    note: '',
  };
}

/// What a mapped task's instances came to. An instance is named `task[item]`, so the results carry
/// the width the run reached; the item count the producer emitted comes off the wire.
export function fanoutStates(
  tasks: PlanTask[],
  latest: ReadonlyMap<string, TaskResult>,
  counts: FanOutCount[],
  running: boolean,
): Map<string, FanOutState> {
  const items = new Map(counts.map((c) => [c.task, c.items]));
  const folded = new Map(counts.map((c) => [c.task, c.not_taken]));
  const states = new Map<string, FanOutState>();
  for (const task of tasks) {
    if (task.over === '') continue;
    states.set(task.name, {
      items: items.get(task.name) ?? null,
      started: 0,
      passed: 0,
      notTaken: 0,
      other: [],
      running,
    });
  }
  for (const [name, result] of latest) {
    const from = mappedFrom(name);
    const state = from === null ? undefined : states.get(from);
    if (state === undefined) continue;
    state.started += 1;
    if (result.status === 'pass') {
      state.passed += 1;
      continue;
    }
    if (result.status === 'not_taken') {
      state.notTaken += 1;
      continue;
    }
    const seen = state.other.find((s) => s.status === result.status);
    if (seen === undefined) state.other.push({ status: result.status, count: 1 });
    else seen.count += 1;
  }
  // The settled node's fold is the engine's own count; instances it settled not taken without a
  // row still settled.
  for (const [name, state] of states) {
    const unreported = (folded.get(name) ?? 0) - state.notTaken;
    if (unreported <= 0) continue;
    state.notTaken += unreported;
    state.started += unreported;
  }
  return states;
}

/// The mapped task an instance came from. The executor names an instance `node[item]` and a
/// declared name may not hold a bracket, so the prefix is the node it was mapped from.
export function mappedFrom(name: string): string | null {
  const cut = name.indexOf('[');
  if (cut <= 0 || !name.endsWith(']')) return null;
  return name.slice(0, cut);
}

/// One provider a mapped task's instances linked out to, and how many urls they reported there.
export interface ProviderCount {
  provider: string;
  count: number;
}

/// What a node says about the external results its task reported: the links themselves, or for a
/// mapped task a tally by provider.
export type NodeLinks =
  | { kind: 'links'; links: ExternalLinkRef[] }
  | { kind: 'counts'; counts: ProviderCount[] };

function providerCounts(links: ExternalLinkRef[]): ProviderCount[] {
  const counts: ProviderCount[] = [];
  for (const link of links) {
    const seen = counts.find((c) => c.provider === link.provider);
    if (seen === undefined) counts.push({ provider: link.provider, count: 1 });
    else seen.count += 1;
  }
  return counts;
}

/// Every link each node draws, by node name. A task's own links are every url it reported across
/// its attempts; a mapped task's node tallies every url reported under it, itself and its
/// instances both, each url counted once however many instances reported it.
export function nodeLinks(tasks: PlanTask[], results: TaskResult[]): Map<string, NodeLinks> {
  const reported = new Map<string, ExternalLinkRef[]>();
  for (const result of results) {
    if (result.links.length === 0) continue;
    reported.set(result.task, [...(reported.get(result.task) ?? []), ...result.links]);
  }
  const mapped = new Set(tasks.filter((task) => task.over !== '').map((task) => task.name));
  const spread = new Map<string, ExternalLinkRef[]>();
  const drawn = new Map<string, NodeLinks>();
  for (const [task, links] of reported) {
    const shown = shownLinks(links);
    if (shown.length > 0) drawn.set(task, { kind: 'links', links: shown });
    const deck = mapped.has(task) ? task : mappedFrom(task);
    if (deck === null || !mapped.has(deck)) continue;
    spread.set(deck, [...(spread.get(deck) ?? []), ...links]);
  }
  for (const [deck, links] of spread) {
    const counts = providerCounts(shownLinks(links));
    if (counts.length > 0) drawn.set(deck, { kind: 'counts', counts });
  }
  return drawn;
}

/// Hide a dependency already implied by a longer path. The executor may retain such dependencies
/// for admission semantics, but drawing both routes makes the DAG read as though downstream work
/// starts directly from an ancestor. An edge that carries a `when` or a narrowed field says
/// something no other path does, and stays.
function transitiveReduction(edges: WorkflowGraphEdge[]): WorkflowGraphEdge[] {
  const children = new Map<string, { to: string; edge: number }[]>();
  edges.forEach((edge, index) => {
    children.set(edge.from, [...(children.get(edge.from) ?? []), { to: edge.to, edge: index }]);
  });
  return edges.filter((edge, skipped) => {
    if ((edge.when ?? null) !== null || edge.keyed.length > 0) return true;
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
export function runGraphView(source: RunGraphSource): RunGraphView {
  const { tasks, outputs: declaredOutputs } = source;
  const results = timedResults(source.results);
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
      fanout: fanoutOf(t),
      keyed: t.keyed,
      when: whenOf(t),
      questions: questionsOf(t.name, source.decisions),
    }),
  );
  const declaredEdges: WorkflowGraphEdge[] = [];
  for (const t of tasks) {
    for (const dep of t.depends_on) {
      const from = declared.get(dep);
      if (from === undefined) continue;
      declaredEdges.push(planEdge(from, t));
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
      declaredEdges.push(plainEdge(sink.name, report.name, report.required));
    }
  }
  const edges: WorkflowGraphEdge[] = [];
  // An instance a per-element edge feeds hangs off that edge, not off its own deck.
  const pairedTargets = new Set<string>();
  for (const edge of transitiveReduction(declaredEdges)) {
    // Once a mapped task has concrete runtime instances, those instances are the leaves of its
    // subgraph. Draw downstream work after them instead of shortcutting directly from the
    // declared task node and making the consumer look like their sibling.
    const producers = instances.get(edge.from) ?? [edge.from];
    // An aligned edge whose consumer has expanded too is read per element: each instance pairs
    // with its counterpart, and the deck-to-deck edge keeps what the consumer reads off it.
    const paired = edge.aligned
      ? pairedInstances(edge, producers, latest, source.decisions)
      : [];
    if (paired.length > 0) {
      edges.push(edge, ...paired);
      for (const pair of paired) pairedTargets.add(pair.to);
      continue;
    }
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
        questions: questionsOf(name, source.decisions),
      }),
    );
    if (from !== null && mapped !== undefined && !pairedTargets.has(name)) {
      edges.push(plainEdge(from, name, mapped.required));
    }
  }

  const attempts = new Map<string, TaskResult[]>();
  for (const r of results) attempts.set(r.task, [...(attempts.get(r.task) ?? []), r]);
  const runtime = new Map<string, TaskRuntime>();
  for (const [name, last] of latest) runtime.set(name, runtimeOf(attempts.get(name) ?? [last], last));
  // A mapped task is the deck, not an attempt: its instances report, it does not, and saying it
  // never ran would contradict the instances hanging off it.
  for (const task of tasks) {
    if (runtime.has(task.name)) continue;
    if (task.over !== '') continue;
    runtime.set(task.name, unreportedRuntime(source.running));
  }
  const fanout = fanoutStates(tasks, latest, source.fanout, source.running);

  // A revision that stored no exposure (`declaredOutputs === null`) gets one marker node: an
  // unextracted pack must never read as a pack that writes nothing.
  const drawn = new Set(nodes.map((n) => n.name));
  const consumed = new Set(edges.map((edge) => edge.from));
  const terminals = [...drawn].filter((name) => !consumed.has(name));
  const outputs = new Map<string, OutputNode>();
  const attach = (name: string, from: string | null) => {
    const parents = from !== null && drawn.has(from) ? [from] : terminals;
    for (const parent of parents) {
      edges.push(plainEdge(parent, name, true));
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
    fanout,
    links: nodeLinks(tasks, results),
    decisions: decisionsByTask(source.decisions),
  };
}

/// The per-element edges under an aligned edge: `producer[k]` to `consumer[k]` for every key both
/// ends reported an instance for. Under a `when`, each carries the answer instance k's route gave.
function pairedInstances(
  edge: WorkflowGraphEdge,
  producers: string[],
  reported: ReadonlyMap<string, TaskResult>,
  decisions: RouteDecision[],
): WorkflowGraphEdge[] {
  const when = edge.when ?? null;
  return producers.flatMap((producer) => {
    const consumer = `${edge.to}${producer.slice(edge.from.length)}`;
    if (producer === edge.from || !reported.has(consumer)) return [];
    const answer =
      when === null
        ? undefined
        : decisions.find((d) => d.task === producer && d.question === when.question)?.labels[0]
            ?.label;
    return [
      {
        ...edge,
        from: producer,
        to: consumer,
        when:
          when === null || answer === undefined
            ? null
            : { route: producer, question: when.question, labels: [answer] },
        keyed: [],
      },
    ];
  });
}

function decisionsByTask(decisions: RouteDecision[]): Map<string, RouteDecision[]> {
  const byTask = new Map<string, RouteDecision[]>();
  for (const decision of decisions) {
    byTask.set(decision.task, [...(byTask.get(decision.task) ?? []), decision]);
  }
  return byTask;
}

/// A route's question ids, as far as the run knows them: the ones it recorded a decision for.
function questionsOf(task: string, decisions: RouteDecision[]): string[] {
  return decisions.filter((d) => d.task === task).map((d) => d.question);
}
