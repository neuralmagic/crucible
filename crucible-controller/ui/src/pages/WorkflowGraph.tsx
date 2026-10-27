import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from 'react';
import type { KeyboardEvent, ReactNode } from 'react';
import {
  BaseEdge,
  ControlButton,
  Controls,
  EdgeLabelRenderer,
  Handle,
  MiniMap,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useReactFlow,
  type Edge,
  type EdgeProps,
  type EdgeTypes,
  type Node,
  type NodeProps,
  type NodeTypes,
} from '@xyflow/react';
import { cn, ExternalLinkChip, ExternalLinkMark, Split, SplitHandle, SplitPane } from '../ui';
import { useDeviceFlag } from '../useDeviceFlag';
import {
  fitViewport,
  frameHeight,
  INSTANCE_GAP,
  INSTANCE_H,
  INSTANCE_INDENT,
  isPill,
  layoutWorkflow,
  LINK_STRIP,
  type LaidOutEdge,
  type LaidOutNode,
  type LayoutDirection,
  type PillOptions,
  type WorkflowGraphDoc,
  type WorkflowGraphNode,
} from './workflowGraphLayout';
import {
  badgeFor,
  decisionLines,
  decisionRows,
  detailRows,
  fanoutCount,
  fanoutLine,
  fanoutRows,
  kindGlyph,
  metaFor,
  runsLine,
  runtimeLine,
  runtimeRows,
  sourceFor,
  statusMark,
  type StatusMark,
} from './workflowGraphCard';
import {
  targetLabel,
  type FanOutState,
  type NodeLinks,
  type OutputNode,
  type RouteDecision,
  type TaskRuntime,
} from './taskGraph';
import { TaskEvidence } from './TaskEvidence';

export interface WorkflowGraphProps {
  graph: WorkflowGraphDoc;
  /// Per-task run state, keyed by task name. Absent for a compiled plan nothing has run yet.
  runtime?: ReadonlyMap<string, TaskRuntime>;
  /// The run this graph belongs to. Absent for a compiled plan, whose tasks have no evidence.
  runId?: string;
  /// The nodes that are declared outputs rather than tasks, keyed by node name. Drawn in their own
  /// style: they must not read as work the graph does.
  outputs?: ReadonlyMap<string, OutputNode>;
  /// What each mapped task's fan-out came to, keyed by the mapped task's name.
  fanoutState?: ReadonlyMap<string, FanOutState>;
  /// The external results each node links out to, keyed by node name.
  links?: ReadonlyMap<string, NodeLinks>;
  /// What each route node or route instance decided, keyed by its name.
  decisions?: ReadonlyMap<string, RouteDecision[]>;
  className?: string;
}

/// A task borrows its border and its dot from how it ended, or from the run still being on its way
/// to it; one with nothing of either to say keeps the card's own rule and a hollow dot.
const MARK_BORDER: Record<StatusMark, string> = {
  pass: 'border-green',
  fail: 'border-red',
  blocked: 'border-amber',
  active: 'border-blue',
  idle: '',
};

const MARK_DOT: Record<StatusMark, string> = {
  pass: 'bg-green',
  fail: 'bg-red',
  blocked: 'bg-amber',
  active: 'animate-pulse bg-blue',
  idle: 'border border-ink-3',
};

function StatusDot({ runtime }: { runtime: TaskRuntime }) {
  return (
    <span
      aria-hidden
      title={runtime.status}
      data-mark={statusMark(runtime)}
      className={cn('inline-block size-2 shrink-0 rounded-full', MARK_DOT[statusMark(runtime)])}
    />
  );
}

/// What the glyph and the dot say, for whoever cannot see them.
function Spoken({ node, runtime }: { node: WorkflowGraphNode; runtime: TaskRuntime | null }) {
  return (
    <span className="sr-only">
      {badgeFor(node)}
      {runtime === null ? '' : ` ${runtime.status}`}{' '}
    </span>
  );
}

/// A full-screen graph too big to hold at once gets an overview to navigate by; fitted into the
/// page it is already the overview.
const MINIMAP_FROM = 8;

/// The rule down a node's left edge: what runs the task. A mapped task runs nothing itself and
/// takes the full-ink rule instead.
const KIND_RULE: Record<string, string> = {
  agent: 'bg-blue',
  command: 'bg-ink-2',
  engine: 'bg-amber',
  route: 'bg-ink',
};

/// The frame's border, top and bottom: its height is set border to border, the graph's inside it.
const FRAME_BORDER = 2;

/// Which way the graph runs is a per-device choice: a wide screen reads left to right, a narrow
/// pane top down.
const VERTICAL_KEY = 'crucible.graph.vertical';

/// Where an edge enters and leaves a card, by direction.
const PORTS: Record<LayoutDirection, { in: Position; out: Position }> = {
  LR: { in: Position.Left, out: Position.Right },
  TD: { in: Position.Top, out: Position.Bottom },
};

/// One edge id, injective in the pair it names: a task may be called anything at all.
function edgeId(from: string, to: string): string {
  return JSON.stringify([from, to]);
}

interface GraphView {
  direction: LayoutDirection;
  nodes: ReadonlyMap<string, LaidOutNode>;
  runtime: ReadonlyMap<string, TaskRuntime>;
  outputs: ReadonlyMap<string, OutputNode>;
  fanoutState: ReadonlyMap<string, FanOutState>;
  links: ReadonlyMap<string, NodeLinks>;
  decisions: ReadonlyMap<string, RouteDecision[]>;
  edges: ReadonlyMap<string, LaidOutEdge>;
  /// Plain command tasks drawn as pills, null when they are cards.
  pills: PillOptions | null;
  /// The hovered edge, whose plumbing label shows.
  hoveredEdge: string | null;
  /// The hovered or focused task and everything it depends on; null when nothing is traced. A
  /// picked task is not traced: reading its metadata should not dim the graph it sits in.
  lit: ReadonlySet<string> | null;
  picked: string | null;
  onTrace: (name: string | null) => void;
  onPick: (name: string) => void;
}

const GraphViewContext = createContext<GraphView | null>(null);

function useGraphView(): GraphView {
  const view = useContext(GraphViewContext);
  if (view === null) throw new Error('a workflow node was rendered outside its graph');
  return view;
}

interface ChipProps {
  children: ReactNode;
}

function Chip({ children }: ChipProps) {
  return (
    <span className="max-w-full truncate border border-rule-hard px-0.5 font-mono text-micro leading-tight text-ink-2">
      {children}
    </span>
  );
}

/// How many links, or how many providers, fit across a card before the rest become a count.
const LINKS_SHOWN = 2;
const PROVIDERS_SHOWN = 4;

/// Drawn over the strip the card reserves rather than inside it: the card is a button, and a link
/// inside a button is neither valid nor reachable from the keyboard.
function NodeLinkRow({ name, drawn, dim }: { name: string; drawn: NodeLinks; dim: boolean }) {
  const cap = drawn.kind === 'counts' ? PROVIDERS_SHOWN : LINKS_SHOWN;
  const total = drawn.kind === 'counts' ? drawn.counts.length : drawn.links.length;
  return (
    <span
      data-node-links={name}
      style={{ opacity: dim ? 0.2 : 1, height: LINK_STRIP, transition: 'opacity 120ms linear' }}
      className="pointer-events-none absolute right-1.5 bottom-0.5 left-2 flex min-w-0 items-center gap-2 overflow-hidden"
    >
      {drawn.kind === 'counts'
        ? drawn.counts.slice(0, cap).map((count) => (
            <span
              key={count.provider}
              aria-label={`${count.count} ${count.provider}`}
              className="pointer-events-auto inline-flex items-center gap-1 font-mono text-micro text-ink-2"
            >
              <ExternalLinkMark provider={count.provider} />
              {count.count}
            </span>
          ))
        : drawn.links.slice(0, cap).map((link) => (
            <ExternalLinkChip
              key={link.url}
              link={link}
              className="pointer-events-auto min-w-0 border-b-0 text-micro"
            />
          ))}
      {total > cap && <Chip>{`+${total - cap}`}</Chip>}
    </span>
  );
}

/// One task, drawn. The card is a button: pressing it opens the task's metadata, and focusing it
/// traces the same ancestry hovering does, so the graph reads from the keyboard.
function TaskCard({ id }: NodeProps) {
  const view = useGraphView();
  const laid = view.nodes.get(id);
  if (laid === undefined) return null;

  const output = view.outputs.get(id);
  if (output !== undefined) return <OutputCard id={id} output={output} />;
  if (laid.instance !== null) return <InstanceRow laid={laid} />;
  if (isPill(laid.node, view.pills)) return <TaskPill laid={laid} />;

  const { node } = laid;
  const advisory = !node.required;
  const dim = view.lit !== null && !view.lit.has(id);
  const fanout = node.fanout ?? null;
  const route = node.kind === 'route';
  const emitted = [...node.emits, ...node.emits_files];
  const decided = decisionLines(view.decisions.get(node.name) ?? []);
  const runs = decided.length === 0 ? runsLine(node) : null;
  const meta = metaFor(node);
  const runtime = view.runtime.get(node.name) ?? null;
  const spread = view.fanoutState.get(node.name) ?? null;
  const links = view.links.get(node.name) ?? null;
  const ran = runtime === null ? null : runtimeLine(runtime);
  const ports = PORTS[view.direction];

  return (
    <>
      <Handle type="target" position={ports.in} isConnectable={false} className="opacity-0" />
      <button
        type="button"
        data-task={node.name}
        data-status={runtime?.status}
        aria-pressed={view.picked === node.name}
        onClick={() => view.onPick(node.name)}
        onFocus={(event) => {
          // Keyboard focus traces; the focus a click leaves behind does not, or picking a task
          // would dim the graph you picked it out of.
          if (event.currentTarget.matches(':focus-visible')) view.onTrace(node.name);
        }}
        onBlur={() => view.onTrace(null)}
        style={{
          opacity: dim ? 0.2 : 1,
          transition: 'opacity 120ms linear',
          paddingBottom: links === null ? undefined : LINK_STRIP,
        }}
        className={cn(
          'relative flex h-full w-full flex-col overflow-hidden py-0.5 pr-1.5 pl-2 text-left leading-tight',
          route ? 'bg-sunk pl-3' : 'bg-raised',
          advisory ? 'border border-dashed border-rule-hard' : 'border border-ink-3',
          route && !advisory && 'border-double border-[3px]',
          laid.isResult && 'border-2 border-ink',
          runtime !== null && MARK_BORDER[statusMark(runtime)],
          view.picked === node.name && 'outline-2 outline-offset-2 outline-blue'
        )}
      >
        <span
          aria-hidden
          className={cn(
            route ? 'absolute top-3 bottom-3 left-1 w-[2px]' : 'absolute top-1 bottom-1 left-0 w-[2px]',
            fanout !== null ? 'bg-ink' : (KIND_RULE[node.kind] ?? 'bg-ink-3')
          )}
        />
        {/* The result task ends the graph: a solid end bar, the way a column ends. */}
        {laid.isResult && <span aria-hidden className="absolute inset-y-0 right-0 w-[3px] bg-ink" />}

        <Spoken node={node} runtime={runtime} />
        <span className="flex items-center gap-1.5">
          <span
            aria-hidden
            title={badgeFor(node)}
            className="w-3 shrink-0 text-center font-mono text-data text-ink-3"
          >
            {kindGlyph(node)}
          </span>
          <span
            title={node.name}
            className={cn(
              'min-w-0 flex-1 truncate font-mono text-body',
              laid.isResult ? 'font-semibold text-ink' : advisory ? 'text-ink-2' : 'text-ink'
            )}
          >
            {node.name}
          </span>
          {spread !== null && (
            <span data-count={node.name} className="shrink-0 font-mono text-label text-ink-2">
              ×{fanoutCount(spread)}
            </span>
          )}
          {runtime !== null && <StatusDot runtime={runtime} />}
        </span>

        {runs !== null && (
          <span title={runs} className="block truncate font-mono text-label text-ink-2">
            {runs}
          </span>
        )}
        {meta !== null && (
          <span title={meta} className="block truncate font-mono text-label text-ink-2">
            {meta}
          </span>
        )}
        {decided.map((line) => (
          <span
            key={line}
            data-decision={node.name}
            title={line}
            className="block truncate font-mono text-label text-ink"
          >
            {line}
          </span>
        ))}
        {spread !== null && (
          <span
            data-fanout={node.name}
            title={fanoutLine(spread)}
            className="block truncate font-mono text-label text-ink-2"
          >
            {fanoutLine(spread)}
          </span>
        )}
        {ran !== null && (
          <span className="block truncate font-mono text-label text-ink-2">{ran}</span>
        )}
        {emitted.length > 0 && (
          <span className="flex min-w-0 gap-1 overflow-hidden">
            {emitted.slice(0, 3).map((field) => (
              <Chip key={field}>{field}</Chip>
            ))}
            {emitted.length > 3 && <Chip>{`+${emitted.length - 3}`}</Chip>}
          </span>
        )}
      </button>
      {links !== null && <NodeLinkRow name={node.name} drawn={links} dim={dim} />}
      <Handle type="source" position={ports.out} isConnectable={false} className="opacity-0" />
    </>
  );
}

/// One runtime instance of a mapped task, drawn as a row under its deck: the item it ran for, and
/// what it answered or how it ended. A button like the card, so it opens the instance's evidence.
function InstanceRow({ laid }: { laid: LaidOutNode }) {
  const view = useGraphView();
  const { node, instance } = laid;
  if (instance === null) return null;
  const dim = view.lit !== null && !view.lit.has(node.name);
  const runtime = view.runtime.get(node.name) ?? null;
  const item = node.name.slice(instance.deck.length + 1, -1);
  const answer = view.decisions.get(node.name)?.[0]?.labels[0]?.label ?? null;
  const links = view.links.get(node.name) ?? null;
  const link = links?.kind === 'links' ? (links.links[0] ?? null) : null;
  const ports = PORTS[view.direction];

  return (
    <>
      <Handle type="target" position={ports.in} isConnectable={false} className="opacity-0" />
      {view.direction === 'LR' && (
        <span
          aria-hidden
          className="absolute border-b border-l border-rule-hard"
          style={{
            left: -INSTANCE_INDENT / 2,
            width: INSTANCE_INDENT / 2,
            top: -(INSTANCE_GAP + (instance.index === 0 ? 0 : INSTANCE_H / 2)),
            bottom: INSTANCE_H / 2,
            opacity: dim ? 0.2 : 1,
          }}
        />
      )}
      <button
        type="button"
        data-task={node.name}
        data-status={runtime?.status}
        aria-pressed={view.picked === node.name}
        onClick={() => view.onPick(node.name)}
        onFocus={(event) => {
          if (event.currentTarget.matches(':focus-visible')) view.onTrace(node.name);
        }}
        onBlur={() => view.onTrace(null)}
        style={{ opacity: dim ? 0.2 : 1, transition: 'opacity 120ms linear' }}
        className={cn(
          'flex h-full w-full items-center gap-1.5 overflow-hidden border bg-raised px-1.5 text-left font-mono text-label leading-none',
          (runtime === null ? '' : MARK_BORDER[statusMark(runtime)]) || 'border-rule-hard',
          link !== null && 'pr-12',
          view.picked === node.name && 'outline-2 outline-offset-2 outline-blue'
        )}
      >
        <Spoken node={node} runtime={runtime} />
        {runtime !== null && <StatusDot runtime={runtime} />}
        <span title={item} className="min-w-0 flex-1 truncate text-ink-2">
          {item}
        </span>
        {answer !== null && <span className="shrink-0 text-ink">{answer}</span>}
      </button>
      {link !== null && (
        <span
          data-node-links={node.name}
          style={{ opacity: dim ? 0.2 : 1 }}
          className="pointer-events-none absolute inset-y-0 right-1 flex items-center"
        >
          <ExternalLinkChip link={link} className="pointer-events-auto border-b-0 text-micro" />
        </span>
      )}
      <Handle type="source" position={ports.out} isConnectable={false} className="opacity-0" />
    </>
  );
}

/// The room a pill keeps for the link chip drawn over its end, in characters of its note.
const PILL_LINK_CHROME = 3;

/// A plain command task in a run, drawn as one line: how it ended, its name, and what its attempts
/// cost when that is anything worth saying.
function TaskPill({ laid }: { laid: LaidOutNode }) {
  const view = useGraphView();
  const { node } = laid;
  const advisory = !node.required;
  const dim = view.lit !== null && !view.lit.has(node.name);
  const runtime = view.runtime.get(node.name) ?? null;
  const ran = runtime === null ? null : runtimeLine(runtime);
  const links = view.links.get(node.name) ?? null;
  const link = links?.kind === 'links' ? (links.links[0] ?? null) : null;
  const ports = PORTS[view.direction];

  return (
    <>
      <Handle type="target" position={ports.in} isConnectable={false} className="opacity-0" />
      <button
        type="button"
        data-task={node.name}
        data-status={runtime?.status}
        aria-pressed={view.picked === node.name}
        onClick={() => view.onPick(node.name)}
        onFocus={(event) => {
          if (event.currentTarget.matches(':focus-visible')) view.onTrace(node.name);
        }}
        onBlur={() => view.onTrace(null)}
        style={{ opacity: dim ? 0.2 : 1, transition: 'opacity 120ms linear' }}
        className={cn(
          'flex h-full w-full items-center gap-1.5 overflow-hidden bg-raised px-2.5 text-left font-mono leading-none',
          advisory ? 'border border-dashed border-rule-hard' : 'border border-ink-3',
          laid.isResult && 'border-2 border-ink',
          runtime !== null && MARK_BORDER[statusMark(runtime)],
          link !== null && 'pr-12',
          view.picked === node.name && 'outline-2 outline-offset-2 outline-blue'
        )}
      >
        <Spoken node={node} runtime={runtime} />
        {runtime !== null && <StatusDot runtime={runtime} />}
        <span
          title={node.name}
          className={cn(
            'min-w-0 flex-1 truncate text-data-lg',
            laid.isResult ? 'font-semibold text-ink' : advisory ? 'text-ink-2' : 'text-ink'
          )}
        >
          {node.name}
        </span>
        {ran !== null && (
          <span title={ran} className="max-w-[45%] shrink-0 truncate text-label text-ink-2">
            {ran}
          </span>
        )}
      </button>
      {link !== null && (
        <span
          data-node-links={node.name}
          style={{ opacity: dim ? 0.2 : 1 }}
          className="pointer-events-none absolute inset-y-0 right-2 flex items-center"
        >
          <ExternalLinkChip link={link} className="pointer-events-auto border-b-0 text-micro" />
        </span>
      )}
      <Handle type="source" position={ports.out} isConnectable={false} className="opacity-0" />
    </>
  );
}

/// A declared output, drawn. Not a task: the card is amber-ruled and double-bordered, carries no
/// status and no evidence, and says where the write lands.
function OutputCard({ id, output }: { id: string; output: OutputNode }) {
  const view = useGraphView();
  const dim = view.lit !== null && !view.lit.has(id);
  const label = output.undeclared ? `${output.kind}` : `${output.kind} \u00d7${output.count}`;
  const where = output.undeclared
    ? 'this revision stored no exposure'
    : (targetLabel(output.target) ?? 'no address');
  const ports = PORTS[view.direction];

  return (
    <>
      <Handle type="target" position={ports.in} isConnectable={false} className="opacity-0" />
      <div
        data-output={id}
        aria-label={`declared output ${label}`}
        style={{ opacity: dim ? 0.2 : 1, transition: 'opacity 120ms linear' }}
        className={cn(
          'relative flex h-full w-full flex-col overflow-hidden border-2 border-dashed bg-raised py-0.5 pr-1.5 pl-2 text-left leading-tight',
          output.undeclared ? 'border-red' : 'border-amber'
        )}
      >
        <span
          aria-hidden
          className={cn(
            'absolute top-1 bottom-1 left-0 w-[2px]',
            output.undeclared ? 'bg-red' : 'bg-amber'
          )}
        />
        <span className="font-mono text-micro tracking-label text-ink-2 uppercase">
          {output.undeclared ? 'undeclared' : 'output'}
        </span>
        <span title={label} className="block truncate font-mono text-data-lg text-ink">
          {label}
        </span>
        <span title={where} className="block truncate font-mono text-micro text-ink-2">
          {where}
        </span>
      </div>
      <Handle type="source" position={ports.out} isConnectable={false} className="opacity-0" />
    </>
  );
}

interface EdgeMarkers {
  arrow: string;
  arrowSpine: string;
  arrowLit: string;
}

const EdgeMarkersContext = createContext<EdgeMarkers>({ arrow: '', arrowSpine: '', arrowLit: '' });

/// One dependency, drawn on the path the layout routed through the gutters. An edge into an
/// advisory task is dashed, and one its consumer joins on carries the join.
function PlanEdge({ id }: EdgeProps) {
  const view = useGraphView();
  const markers = useContext(EdgeMarkersContext);
  const laid = view.edges.get(id);
  if (laid === undefined) return null;

  const lit = view.lit !== null && view.lit.has(laid.from) && view.lit.has(laid.to);
  const dim = view.lit !== null && !lit;
  const tone = lit ? 'var(--ink)' : laid.spine ? 'var(--ink-2)' : 'var(--ink-3)';
  const width = lit ? 1.6 : laid.spine ? 1.4 : 1;
  // The spine is drawn whole, advisory or not: it is the line a reader follows first.
  const dashed = !laid.required && !laid.spine;
  const untaken = view.runtime.get(laid.to)?.status === 'not_taken';
  const fade = dim ? 0.15 : untaken ? 0.45 : 1;
  const double = laid.aligned && !untaken;
  const plumbing = lit || view.hoveredEdge === id ? laid.plumbing : null;
  if (laid.implied && !lit) return null;

  return (
    <>
      {double && (
        <path
          d={laid.path}
          data-aligned-edge={id}
          fill="none"
          style={{
            stroke: tone,
            strokeWidth: width * 2 + 2.5,
            strokeDasharray: dashed ? '3 3' : undefined,
            opacity: fade,
          }}
        />
      )}
      <BaseEdge
        id={id}
        path={laid.path}
        markerEnd={`url(#${lit ? markers.arrowLit : laid.spine ? markers.arrowSpine : markers.arrow})`}
        style={{
          stroke: double ? 'var(--paper)' : tone,
          strokeWidth: double ? 2.5 : width,
          strokeDasharray: untaken ? '2 4' : dashed && !double ? '3 3' : undefined,
          opacity: fade,
        }}
      />
      {(laid.label !== null || laid.reads !== null || plumbing !== null) && (
        <EdgeLabelRenderer>
          <span
            data-edge-label={id}
            className={cn(
              'absolute flex gap-1 bg-paper px-1 font-mono text-label text-ink-2',
              plumbing !== null && 'z-10'
            )}
            style={{
              transform: `translate(-50%, -50%) translate(${laid.labelX}px, ${laid.labelY}px)`,
              opacity: fade,
            }}
          >
            {laid.label !== null && <span className="tracking-label uppercase">{laid.label}</span>}
            {laid.reads !== null && <span className="text-ink">{laid.reads}</span>}
            {plumbing !== null && <span data-plumbing>{plumbing}</span>}
          </span>
        </EdgeLabelRenderer>
      )}
    </>
  );
}

const NODE_TYPES: NodeTypes = { task: TaskCard };
const PRO_OPTIONS = { hideAttribution: true };
const EDGE_TYPES: EdgeTypes = { plan: PlanEdge };

interface TaskPanelProps {
  laid: LaidOutNode;
  runtime: TaskRuntime | null;
  spread: FanOutState | null;
  decisions: RouteDecision[];
  runId: string | undefined;
  onClose: () => void;
}

/// Everything the graph document holds about one task, the source it runs included: too long for a
/// card, and the thing an importer most wants to read before registering a pack.
function TaskPanel({ laid, runtime, spread, decisions, runId, onClose }: TaskPanelProps) {
  const { node } = laid;
  const source = sourceFor(node);
  const rows = [
    ...(runtime === null ? [] : runtimeRows(runtime)),
    ...(spread === null ? [] : fanoutRows(spread)),
    ...decisionRows(decisions),
    ...detailRows(node),
  ];

  return (
    <aside
      aria-label={`Task ${node.name}`}
      className="flex h-full min-h-0 flex-col border-l border-rule-hard bg-surface"
    >
      <header className="flex items-start justify-between gap-2 border-b border-rule px-3 py-2">
        <div className="min-w-0">
          <p className="m-0 font-mono text-micro tracking-label text-ink-3 uppercase">
            {badgeFor(node)}
            {laid.isResult ? ' · result' : ''}
            {runtime === null ? '' : ` · ${runtime.status}`}
          </p>
          <p className="m-0 font-mono text-data-lg break-words text-ink">{node.name}</p>
        </div>
        <button
          type="button"
          onClick={onClose}
          className="border border-rule-hard px-1.5 py-0.5 font-mono text-micro tracking-label text-ink-2 uppercase hover:border-ink hover:text-ink"
        >
          Close
        </button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-3 py-2">
        <dl className="m-0 grid grid-cols-[104px_1fr] gap-x-3 gap-y-1">
          {rows.map((row) => (
            <div key={row.label} className="contents">
              <dt className="font-mono text-micro tracking-label text-ink-3 uppercase">
                {row.label}
              </dt>
              <dd className="m-0 font-mono text-data break-words text-ink-2">{row.value}</dd>
            </div>
          ))}
        </dl>
        {runId !== undefined && <TaskEvidence key={node.name} runId={runId} task={node.name} />}
        {source !== null && (
          <>
            <p className="mt-3 mb-1 font-mono text-micro tracking-label text-ink-3 uppercase">
              {source.label}
            </p>
            <pre className="m-0 max-h-64 overflow-auto border border-rule bg-paper px-2 py-1.5 font-mono text-data whitespace-pre-wrap text-ink-2">
              {source.body}
            </pre>
          </>
        )}
      </div>
    </aside>
  );
}

const NO_RUNTIME: ReadonlyMap<string, TaskRuntime> = new Map();
const NO_OUTPUTS: ReadonlyMap<string, OutputNode> = new Map();
const NO_FANOUT: ReadonlyMap<string, FanOutState> = new Map();
const NO_LINKS: ReadonlyMap<string, NodeLinks> = new Map();
const NO_DECISIONS: ReadonlyMap<string, RouteDecision[]> = new Map();

const CANVAS_ONLY = ['canvas'];
const CANVAS_AND_PANEL = ['canvas', 'panel'];

function GraphCanvas({
  graph,
  runtime,
  runId,
  outputs,
  fanoutState,
  links,
  decisions,
  className,
}: WorkflowGraphProps) {
  const marker = useId().replace(/:/g, '');
  const markers = useMemo(
    () => ({
      arrow: `arrow-${marker}`,
      arrowSpine: `arrow-spine-${marker}`,
      arrowLit: `arrow-lit-${marker}`,
    }),
    [marker]
  );
  const [vertical, setVertical] = useDeviceFlag(VERTICAL_KEY, false);
  const direction: LayoutDirection = vertical ? 'TD' : 'LR';
  // A pill is as wide as what it says, so the layout is told how much trails each name.
  const pills = useMemo<PillOptions | null>(() => {
    if (runtime === undefined) return null;
    const trailing = new Map<string, number>();
    for (const node of graph.nodes) {
      const state = runtime.get(node.name);
      const note = state === undefined ? null : runtimeLine(state);
      const drawn = links?.get(node.name);
      const first = drawn?.kind === 'links' ? drawn.links[0] : undefined;
      const linked = first === undefined ? 0 : first.label.length + PILL_LINK_CHROME;
      const chars = (note?.length ?? 0) + linked;
      if (chars > 0) trailing.set(node.name, chars);
    }
    return { trailing };
  }, [graph, runtime, links]);
  const layout = useMemo(
    () => layoutWorkflow(graph, direction, { pills }),
    [graph, direction, pills]
  );
  const [traced, setTraced] = useState<string | null>(null);
  const [hoveredEdge, setHoveredEdge] = useState<string | null>(null);
  const [picked, setPicked] = useState<string | null>(null);
  const [full, setFull] = useState(false);
  const { setViewport } = useReactFlow();
  const canvasRef = useRef<HTMLDivElement | null>(null);
  const frameRef = useRef<HTMLDivElement | null>(null);
  const [frameWidth, setFrameWidth] = useState(0);
  const fit = useCallback(() => {
    const element = canvasRef.current;
    if (element === null) return;
    void setViewport(
      fitViewport(layout, { width: element.clientWidth, height: element.clientHeight })
    );
  }, [layout, setViewport]);

  const nodes = useMemo<Node[]>(
    () =>
      layout.nodes.map((laid) => ({
        id: laid.node.name,
        type: 'task',
        position: { x: laid.x, y: laid.y },
        width: laid.width,
        height: laid.height,
        draggable: false,
        selectable: false,
        connectable: false,
        focusable: false,
        data: {},
      })),
    [layout]
  );
  const edges = useMemo<Edge[]>(
    () =>
      layout.edges.map((laid) => ({
        id: edgeId(laid.from, laid.to),
        source: laid.from,
        target: laid.to,
        type: 'plan',
        focusable: false,
        selectable: false,
        data: {},
      })),
    [layout]
  );

  // A recompile is a different plan: nothing stays picked across it. Keyed on what the plan says
  // rather than on the object the fetch handed us, so a refetch that changes nothing leaves the
  // reader where they were.
  const signature = useMemo(() => JSON.stringify(graph), [graph]);
  const drawn = useRef<string | null>(null);
  useEffect(() => {
    if (drawn.current === signature) return;
    drawn.current = signature;
    setPicked(null);
    setTraced(null);
  }, [signature]);

  // A new arrangement, whether from a recompile or from turning the graph, is refit.
  useEffect(() => {
    fit();
  }, [fit]);

  const view = useMemo<GraphView>(() => {
    return {
      direction,
      nodes: new Map(layout.nodes.map((laid) => [laid.node.name, laid])),
      runtime: runtime ?? NO_RUNTIME,
      outputs: outputs ?? NO_OUTPUTS,
      fanoutState: fanoutState ?? NO_FANOUT,
      links: links ?? NO_LINKS,
      decisions: decisions ?? NO_DECISIONS,
      edges: new Map(layout.edges.map((laid) => [edgeId(laid.from, laid.to), laid])),
      pills,
      hoveredEdge,
      lit: traced === null ? null : (layout.ancestry.get(traced) ?? new Set([traced])),
      picked,
      onTrace: setTraced,
      onPick: setPicked,
    };
  }, [
    direction,
    layout,
    runtime,
    outputs,
    fanoutState,
    links,
    decisions,
    pills,
    hoveredEdge,
    traced,
    picked,
  ]);

  // The canvas changes size when a divider is dragged, the rail collapses, or the window resizes;
  // the plan is refit to whatever it is now.
  useEffect(() => {
    const element = canvasRef.current;
    if (element === null) return;
    let frame = 0;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(fit);
    });
    observer.observe(element);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [fit]);

  // The frame is as tall as its graph needs at the width it is given.
  useEffect(() => {
    const element = frameRef.current;
    if (element === null) return;
    const observer = new ResizeObserver((entries) => {
      const width = entries[0]?.contentRect.width;
      if (width !== undefined) setFrameWidth(width);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const close = useCallback(() => setPicked(null), []);
  // Escape closes the innermost thing first: the task panel, then the full-screen view.
  const onKeyDown = useCallback(
    (event: KeyboardEvent<HTMLDivElement>) => {
      if (event.key !== 'Escape') return;
      if (picked !== null) {
        event.stopPropagation();
        const card = event.currentTarget.querySelector(`[data-task="${CSS.escape(picked)}"]`);
        close();
        if (card instanceof HTMLElement) card.focus();
        return;
      }
      if (full) {
        event.stopPropagation();
        setFull(false);
      }
    },
    [picked, close, full]
  );

  const pickedNode = picked === null ? undefined : view.nodes.get(picked);
  const height = frameHeight(layout, frameWidth, pickedNode !== undefined);
  const panelIds = pickedNode === undefined ? CANVAS_ONLY : CANVAS_AND_PANEL;

  if (layout.nodes.length === 0) return null;

  return (
    <EdgeMarkersContext.Provider value={markers}>
      <div className={className}>
        <div
          role="region"
          aria-label="Compiled workflow graph"
          data-testid="workflow-graph"
          ref={frameRef}
          onKeyDown={onKeyDown}
          data-fullscreen={full ? 'true' : undefined}
          style={full ? undefined : { height: `min(${height + FRAME_BORDER}px, 780px, 64vh)` }}
          className={cn(
            'border border-rule-hard bg-paper',
            full ? 'fixed inset-0 z-40 h-screen w-screen' : 'relative w-full'
          )}
        >
          <div
            role="toolbar"
            aria-label="Graph view"
            className="absolute top-2 right-2 z-10 flex font-mono text-micro tracking-label uppercase"
          >
            {(
              [
                { vertical: false, label: 'LR', title: 'Lay the graph out left to right' },
                { vertical: true, label: 'TD', title: 'Lay the graph out top down' },
              ] as const
            ).map((option) => (
              <button
                key={option.label}
                type="button"
                title={option.title}
                aria-label={`${option.label}: ${option.title}`}
                aria-pressed={vertical === option.vertical}
                onClick={() => setVertical(option.vertical)}
                className={cn(
                  'cursor-pointer border border-r-0 border-rule-hard bg-surface px-2 py-1 text-ink-2 hover:bg-hi hover:text-ink',
                  vertical === option.vertical && 'bg-ink text-surface hover:bg-ink hover:text-surface'
                )}
              >
                <span aria-hidden>{option.vertical ? '\u2193' : '\u2192'} </span>
                {option.label}
              </button>
            ))}
            <button
              type="button"
              aria-pressed={full}
              onClick={() => setFull((value) => !value)}
              className="cursor-pointer border border-rule-hard bg-surface px-2 py-1 text-ink-2 hover:bg-hi hover:text-ink"
            >
              {full ? 'Exit full screen' : 'Full screen'}
            </button>
          </div>
          <svg aria-hidden width={0} height={0} className="absolute">
            <defs>
              {[
                { id: markers.arrow, tone: 'var(--ink-3)' },
                { id: markers.arrowSpine, tone: 'var(--ink-2)' },
                { id: markers.arrowLit, tone: 'var(--ink)' },
              ].map(({ id, tone }) => (
                <marker
                  key={id}
                  id={id}
                  viewBox="0 0 8 8"
                  refX={7}
                  refY={4}
                  markerWidth={7}
                  markerHeight={7}
                  markerUnits="userSpaceOnUse"
                  orient="auto-start-reverse"
                >
                  <path d="M 0 1 L 7 4 L 0 7 z" fill={tone} />
                </marker>
              ))}
            </defs>
          </svg>

          <Split id="crucible.graph" panelIds={panelIds} className="h-full w-full">
            <SplitPane id="canvas" minSize="30%">
              <div ref={canvasRef} className="relative min-h-0 min-w-0 flex-1">
                <GraphViewContext.Provider value={view}>
                  <ReactFlow
                    nodes={nodes}
                    edges={edges}
                    nodeTypes={NODE_TYPES}
                    edgeTypes={EDGE_TYPES}
                    proOptions={PRO_OPTIONS}
                    minZoom={0.2}
                    maxZoom={2}
                    nodesDraggable={false}
                    nodesConnectable={false}
                    nodesFocusable={false}
                    edgesFocusable={false}
                    elementsSelectable={false}
                    zoomOnScroll={false}
                    zoomOnDoubleClick={false}
                    preventScrolling={false}
                    onNodeMouseEnter={(_, node) => setTraced(node.id)}
                    onNodeMouseLeave={() => setTraced(null)}
                    onEdgeMouseEnter={(_, edge) => setHoveredEdge(edge.id)}
                    onEdgeMouseLeave={() => setHoveredEdge(null)}
                    onPaneClick={close}
                  >
                    <Controls showInteractive={false} showFitView={false}>
                      <ControlButton onClick={fit} title="Fit view" aria-label="Fit view">
                        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 30" aria-hidden>
                          <path d="M3.692 4.63c0-.53.4-.938.939-.938h5.215V0H4.708C2.13 0 0 2.054 0 4.63v5.216h3.692V4.631zM27.354 0h-5.2v3.692h5.17c.53 0 .984.4.984.939v5.215H32V4.631A4.624 4.624 0 0027.354 0zm.954 24.83c0 .532-.4.94-.939.94h-5.215v3.768h5.215c2.577 0 4.631-2.13 4.631-4.707v-5.139h-3.692v5.139zm-23.677.94c-.531 0-.939-.4-.939-.94v-5.138H0v5.139c0 2.577 2.13 4.707 4.708 4.707h5.138V25.77H4.631z" />
                        </svg>
                      </ControlButton>
                    </Controls>
                    {full && layout.nodes.length >= MINIMAP_FROM && <MiniMap pannable zoomable />}
                  </ReactFlow>
                </GraphViewContext.Provider>
              </div>
            </SplitPane>

            {pickedNode !== undefined && (
              <>
                <SplitHandle label="Resize the task panel" />
                <SplitPane id="panel" defaultSize="320px" minSize="14rem" maxSize="70%">
                  <TaskPanel
                    laid={pickedNode}
                    runtime={view.runtime.get(pickedNode.node.name) ?? null}
                    spread={view.fanoutState.get(pickedNode.node.name) ?? null}
                    decisions={view.decisions.get(pickedNode.node.name) ?? []}
                    runId={runId}
                    onClose={close}
                  />
                </SplitPane>
              </>
            )}
          </Split>
        </div>
      </div>
    </EdgeMarkersContext.Provider>
  );
}

/// A compiled plan, drawn. Layers run left to right, or top down, in dependency order; a mapped task is a deck of
/// cards, an advisory one is weaker and dashed, an edge its consumer joins on is labelled, and the
/// result task carries an end bar. Hovering or focusing a task traces what it depends on, and
/// picking one opens everything the graph document holds about it. Given per-task run state, each
/// card also carries how the task ended, and given the run it belongs to, the panel reads that
/// task's evidence: its result, what it emitted, and the files it captured.
///
/// Every label is React text, so a pack's own task names can never be markup here.
export function WorkflowGraph(props: WorkflowGraphProps) {
  return (
    <ReactFlowProvider>
      <GraphCanvas {...props} />
    </ReactFlowProvider>
  );
}
