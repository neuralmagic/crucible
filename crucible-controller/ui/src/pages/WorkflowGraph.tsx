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
  Background,
  BackgroundVariant,
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
  type FitViewOptions,
  type Node,
  type NodeProps,
  type NodeTypes,
} from '@xyflow/react';
import { cn, Split, SplitHandle, SplitPane } from '../ui';
import { useDeviceFlag } from '../useDeviceFlag';
import {
  layoutWorkflow,
  STACK_CARDS,
  STACK_STEP,
  type LaidOutEdge,
  type LaidOutNode,
  type LayoutDirection,
  type WorkflowGraphDoc,
} from './workflowGraphLayout';
import {
  badgeFor,
  detailRows,
  metaFor,
  runsLine,
  runtimeLine,
  runtimeRows,
  sourceFor,
} from './workflowGraphCard';
import { targetLabel, type OutputNode, type TaskRuntime, type TaskTone } from './taskGraph';
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
  className?: string;
}

/// A finished task borrows its border and its status from how it ended; one with nothing terminal
/// to say keeps the card's own rule.
const TONE_BORDER: Record<TaskTone, string> = {
  pass: 'border-green',
  fail: 'border-red',
  none: '',
};

const TONE_TEXT: Record<TaskTone, string> = {
  pass: 'text-green',
  fail: 'text-red',
  none: 'text-ink-3',
};

/// A full-screen graph too big to hold at once gets an overview to navigate by; fitted into the
/// page it is already the overview.
const MINIMAP_FROM = 8;

/// The rule down a node's left edge: what runs the task.
const KIND_RULE: Record<string, string> = {
  agent: 'bg-blue',
  command: 'bg-ink-2',
  engine: 'bg-amber',
};

/// Which way the graph runs is a per-device choice: a wide screen reads left to right, a narrow
/// pane top down.
const VERTICAL_KEY = 'crucible.graph.vertical';

const FIT: FitViewOptions = {
  padding: { top: '34px', right: '8px', bottom: '8px', left: '8px' },
  maxZoom: 1.35,
};

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
  edges: ReadonlyMap<string, LaidOutEdge>;
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

/// One task, drawn. The card is a button: pressing it opens the task's metadata, and focusing it
/// traces the same ancestry hovering does, so the graph reads from the keyboard.
function TaskCard({ id }: NodeProps) {
  const view = useGraphView();
  const laid = view.nodes.get(id);
  if (laid === undefined) return null;

  const output = view.outputs.get(id);
  if (output !== undefined) return <OutputCard id={id} output={output} />;

  const { node } = laid;
  const advisory = !node.required;
  const dim = view.lit !== null && !view.lit.has(id);
  const fanout = node.fanout ?? null;
  const emitted = [...node.emits, ...node.emits_files];
  const runs = runsLine(node);
  const meta = metaFor(node);
  const runtime = view.runtime.get(node.name) ?? null;
  const ports = PORTS[view.direction];

  return (
    <>
      <Handle type="target" position={ports.in} isConnectable={false} className="opacity-0" />
      {/* A mapped task is drawn as the deck of instances it becomes at run time. */}
      {fanout !== null &&
        Array.from({ length: STACK_CARDS }, (_, i) => (
          <span
            key={i}
            aria-hidden
            className="absolute inset-0 border border-rule-hard bg-raised"
            style={{
              transform: `translate(${(STACK_CARDS - i) * STACK_STEP}px, ${
                (STACK_CARDS - i) * STACK_STEP
              }px)`,
              opacity: dim ? 0.2 : 1,
            }}
          />
        ))}
      <button
        type="button"
        data-task={node.name}
        aria-label={`${badgeFor(node)} ${node.name}`}
        aria-pressed={view.picked === node.name}
        onClick={() => view.onPick(node.name)}
        onFocus={(event) => {
          // Keyboard focus traces; the focus a click leaves behind does not, or picking a task
          // would dim the graph you picked it out of.
          if (event.currentTarget.matches(':focus-visible')) view.onTrace(node.name);
        }}
        onBlur={() => view.onTrace(null)}
        style={{ opacity: dim ? 0.2 : 1, transition: 'opacity 120ms linear' }}
        className={cn(
          'relative flex h-full w-full flex-col overflow-hidden bg-raised py-0.5 pr-1.5 pl-2 text-left leading-tight',
          advisory ? 'border border-dashed border-rule-hard' : 'border border-ink-3',
          laid.isResult && 'border-2 border-ink',
          runtime !== null && TONE_BORDER[runtime.tone],
          view.picked === node.name && 'outline-2 outline-offset-2 outline-blue'
        )}
      >
        <span
          aria-hidden
          className={cn(
            'absolute top-1 bottom-1 left-0 w-[2px]',
            KIND_RULE[node.kind] ?? 'bg-ink-3'
          )}
        />
        {/* The result task ends the graph: a solid end bar, the way a column ends. */}
        {laid.isResult && <span aria-hidden className="absolute inset-y-0 right-0 w-[3px] bg-ink" />}

        <span className="flex items-baseline justify-between gap-2 font-mono text-micro tracking-label text-ink-2 uppercase">
          <span>{badgeFor(node)}</span>
          {runtime === null ? (
            <span>{laid.isResult ? 'result' : advisory ? 'advisory' : ''}</span>
          ) : (
            <span className={TONE_TEXT[runtime.tone]}>{runtime.status}</span>
          )}
        </span>

        <span
          title={node.name}
          className={cn(
            'block truncate font-mono text-data-lg',
            laid.isResult ? 'font-semibold text-ink' : advisory ? 'text-ink-2' : 'text-ink'
          )}
        >
          {node.name}
        </span>

        {runs !== null && (
          <span title={runs} className="block truncate font-mono text-micro text-ink-2">
            {runs}
          </span>
        )}
        {meta !== null && (
          <span title={meta} className="block truncate font-mono text-micro text-ink-2">
            {meta}
          </span>
        )}
        {runtime !== null && (
          <span className="block truncate font-mono text-micro text-ink-2">
            {runtimeLine(runtime)}
          </span>
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
  arrowLit: string;
}

const EdgeMarkersContext = createContext<EdgeMarkers>({ arrow: '', arrowLit: '' });

/// One dependency, drawn on the path the layout routed through the gutters. An edge into an
/// advisory task is dashed, and one its consumer joins on carries the join.
function PlanEdge({ id }: EdgeProps) {
  const view = useGraphView();
  const markers = useContext(EdgeMarkersContext);
  const laid = view.edges.get(id);
  if (laid === undefined) return null;

  const lit = view.lit !== null && view.lit.has(laid.from) && view.lit.has(laid.to);
  const dim = view.lit !== null && !lit;

  return (
    <>
      <BaseEdge
        id={id}
        path={laid.path}
        markerEnd={`url(#${lit ? markers.arrowLit : markers.arrow})`}
        style={{
          stroke: lit ? 'var(--ink)' : 'var(--ink-3)',
          strokeWidth: lit ? 1.6 : 1,
          strokeDasharray: laid.required ? undefined : '3 3',
          opacity: dim ? 0.15 : 1,
        }}
      />
      {laid.label !== null && (
        <EdgeLabelRenderer>
          <span
            className="absolute bg-paper px-1 font-mono text-micro tracking-label text-ink-2 uppercase"
            style={{
              transform: `translate(-50%, -50%) translate(${laid.labelX}px, ${laid.labelY}px)`,
              opacity: dim ? 0.15 : 1,
            }}
          >
            {laid.label}
          </span>
        </EdgeLabelRenderer>
      )}
    </>
  );
}

const NODE_TYPES: NodeTypes = { task: TaskCard };
const EDGE_TYPES: EdgeTypes = { plan: PlanEdge };

interface TaskPanelProps {
  laid: LaidOutNode;
  runtime: TaskRuntime | null;
  runId: string | undefined;
  onClose: () => void;
}

/// Everything the graph document holds about one task, the source it runs included: too long for a
/// card, and the thing an importer most wants to read before registering a pack.
function TaskPanel({ laid, runtime, runId, onClose }: TaskPanelProps) {
  const { node } = laid;
  const source = sourceFor(node);
  const rows = runtime === null ? detailRows(node) : [...runtimeRows(runtime), ...detailRows(node)];

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

const CANVAS_ONLY = ['canvas'];
const CANVAS_AND_PANEL = ['canvas', 'panel'];

function GraphCanvas({ graph, runtime, runId, outputs, className }: WorkflowGraphProps) {
  const marker = useId().replace(/:/g, '');
  const markers = useMemo(
    () => ({ arrow: `arrow-${marker}`, arrowLit: `arrow-lit-${marker}` }),
    [marker]
  );
  const [vertical, setVertical] = useDeviceFlag(VERTICAL_KEY, false);
  const direction: LayoutDirection = vertical ? 'TD' : 'LR';
  const layout = useMemo(() => layoutWorkflow(graph, direction), [graph, direction]);
  const [traced, setTraced] = useState<string | null>(null);
  const [picked, setPicked] = useState<string | null>(null);
  const [full, setFull] = useState(false);
  const { fitView } = useReactFlow();

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
    void fitView(FIT);
  }, [layout, fitView]);

  const view = useMemo<GraphView>(() => {
    return {
      direction,
      nodes: new Map(layout.nodes.map((laid) => [laid.node.name, laid])),
      runtime: runtime ?? NO_RUNTIME,
      outputs: outputs ?? NO_OUTPUTS,
      edges: new Map(layout.edges.map((laid) => [edgeId(laid.from, laid.to), laid])),
      lit: traced === null ? null : (layout.ancestry.get(traced) ?? new Set([traced])),
      picked,
      onTrace: setTraced,
      onPick: setPicked,
    };
  }, [direction, layout, runtime, outputs, traced, picked]);

  const canvasRef = useRef<HTMLDivElement | null>(null);
  // The canvas changes size when a divider is dragged, the rail collapses, or the window resizes;
  // the plan is refit to whatever it is now.
  useEffect(() => {
    const element = canvasRef.current;
    if (element === null) return;
    let frame = 0;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        void fitView(FIT);
      });
    });
    observer.observe(element);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [fitView]);

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

  const tones = new Set([...view.runtime.values()].map((state) => state.tone));
  const marks = [
    tones.has('pass') ? 'green = passed' : null,
    tones.has('fail') ? 'red = failed' : null,
    layout.nodes.some((laid) => (laid.node.fanout ?? null) !== null)
      ? 'stacked = mapped over a producer field'
      : null,
    layout.nodes.some((laid) => !laid.node.required) ? 'dashed = advisory' : null,
    layout.edges.some((laid) => laid.label !== null) ? 'passed = joins only on what passed' : null,
    layout.nodes.some((laid) => laid.isResult) ? 'end bar = result' : null,
  ].filter((mark): mark is string => mark !== null);

  const pickedNode = picked === null ? undefined : view.nodes.get(picked);
  const panelIds = pickedNode === undefined ? CANVAS_ONLY : CANVAS_AND_PANEL;

  if (layout.nodes.length === 0) return null;

  return (
    <EdgeMarkersContext.Provider value={markers}>
      <div className={className}>
        <div
          role="region"
          aria-label="Compiled workflow graph"
          data-testid="workflow-graph"
          onKeyDown={onKeyDown}
          data-fullscreen={full ? 'true' : undefined}
          style={full ? undefined : { aspectRatio: `${layout.width} / ${layout.height}` }}
          className={cn(
            'border border-rule-hard bg-paper',
            full
              ? 'fixed inset-0 z-40 h-screen w-screen'
              : 'relative max-h-[min(780px,64vh)] min-h-[240px] w-full'
          )}
        >
          <div
            role="toolbar"
            aria-label="Graph view"
            className="absolute top-2 right-2 z-10 flex font-mono text-micro tracking-label uppercase"
          >
            {(
              [
                { vertical: false, label: '\u2192 LR', title: 'Lay the graph out left to right' },
                { vertical: true, label: '\u2193 TD', title: 'Lay the graph out top down' },
              ] as const
            ).map((option) => (
              <button
                key={option.label}
                type="button"
                title={option.title}
                aria-label={option.title}
                aria-pressed={vertical === option.vertical}
                onClick={() => setVertical(option.vertical)}
                className={cn(
                  'cursor-pointer border border-r-0 border-rule-hard bg-surface px-2 py-1 text-ink-2 hover:bg-hi hover:text-ink',
                  vertical === option.vertical && 'bg-ink text-surface hover:bg-ink hover:text-surface'
                )}
              >
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
                    fitView
                    fitViewOptions={FIT}
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
                    onPaneClick={close}
                  >
                    <Background variant={BackgroundVariant.Dots} gap={12} size={1} />
                    <Controls showInteractive={false} />
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
                    runId={runId}
                    onClose={close}
                  />
                </SplitPane>
              </>
            )}
          </Split>
        </div>
        {marks.length > 0 && (
          <p className="m-0 pt-3 font-mono text-micro tracking-label text-ink-3 uppercase">
            {marks.join('   ·   ')}
          </p>
        )}
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
