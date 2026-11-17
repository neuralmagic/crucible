// The types a view plugin written in TypeScript checks against. A plugin is one module that
// exports `inputs`, the captured file paths it reads, and `view`, which turns those files into
// a view. The host compiles and runs it; it has no imports, no network and no files.
//
//   export const inputs = ["verdicts.json"];
//   export function view({ files }: ViewRequest): View { ... }

/** A colour role from the app's theme. */
type Tone = "neutral" | "muted" | "good" | "warn" | "bad" | "info";

/** How a plot's axis places values: `log` holds only values above zero, `time` is Unix seconds shown in UTC. */
type Scale = "linear" | "log" | "time";

/** A captured file handed to the plugin. */
interface CapturedFile {
  /** The path the task declared. */
  path: string;
  /** The task that captured it, `task[instance]` for a mapped task. */
  task: string;
  text: string;
}

interface ViewRequest {
  files: CapturedFile[];
}

interface View {
  title: string;
  body: ViewNode;
}

interface ViewCell {
  text: string;
  /** What a table sorts the cell by in place of its text: the seconds behind "3 min". */
  value?: number;
  /**
   * The address the cell opens: `http(s)://...`, `run:task/<task>` for a task of the run, or
   * `run:file/<task>/<path>` for a file it captured, with the task as a `CapturedFile` names it.
   */
  href?: string;
  tone?: Tone;
}

/** One component: an object with one key, its name. */
type ViewNode =
  | { stack: { children: ViewNode[] } }
  | {
      /** Side by side at equal widths, wrapping when narrow. Holds no table, however deep. */
      columns: { children: ViewNode[] };
    }
  | {
      /** `body` under a `title` the reader folds and unfolds; unfolded unless `open` is false. */
      section: { id: string; title: string; open?: boolean; body: ViewNode };
    }
  | { tabs: { id: string; tabs: { label: string; count?: number; body: ViewNode }[] } }
  | { stats: { items: { label: string; value: string; tone?: Tone }[] } }
  | { bars: { bars: { label: string; segments: { label: string; value: number; tone?: Tone }[] }[] } }
  | {
      plot: {
        kind: "line" | "scatter";
        x: string;
        y: string;
        /** A series left `neutral` takes the next colour of the app's palette. */
        series: { label: string; tone?: Tone; points: [number, number][] }[];
        x_scale?: Scale;
        y_scale?: Scale;
        /** Names an x value indexes, from 0; the x axis is then linear. */
        categories?: string[];
      };
    }
  | {
      /** `values` counted into `bins` (1 to 100) bins of equal width; the app picks when not given. */
      histogram: { x: string; values: number[]; bins?: number; tone?: Tone };
    }
  | {
      /** A row of `values` per `y` label, a value per `x` label, shaded from least to greatest. */
      heatmap: { x: string[]; y: string[]; values: number[][]; tone?: Tone };
    }
  | {
      /** Quantities flowing between stages, drawn as ribbons: a Sankey diagram. */
      flow: {
        nodes: { id: string; label: string; tone?: Tone }[];
        /** `value` is greater than zero, and no ribbon leads back to an earlier stage. */
        links: { from: string; to: string; value: number; tone?: Tone }[];
      };
    }
  | { grid: { columns: string[]; rows: ViewCell[][] } }
  | {
      table: {
        id: string;
        columns: {
          id: string;
          title: string;
          /** `number` is right-aligned and sorts by `value`, or by the text when it is a number. */
          kind?: "text" | "mono" | "badge" | "number";
          width?: "narrow" | "medium" | "wide" | "fill";
        }[];
        /** `detail` is Markdown shown under the table while the row is selected. */
        rows: { id: string; cells: ViewCell[]; detail?: string }[];
      };
    }
  | { text: { text: string; tone?: Tone } }
  | {
      /** Markdown. A link takes the addresses a cell's `href` does; a `mermaid` fence is drawn. */
      markdown: { text: string };
    }
  | {
      /** A Mermaid diagram from its source. */
      mermaid: { source: string };
    }
  | CanvasNode;

/** What `Canvas.node()` returns. Build it with a `Canvas`, not by hand. */
interface CanvasNode {
  canvas: { width: number; height: number; commands: unknown[] };
}

/**
 * The browser's 2D context, as far as the app draws it. A style is a tone or a hex colour.
 * There are no transforms, clips, images or gradients, and text is drawn over every shape.
 */
declare class CanvasContext2D {
  fillStyle: Tone | `#${string}`;
  strokeStyle: Tone | `#${string}`;
  globalAlpha: number;
  lineWidth: number;
  lineCap: "butt" | "round" | "square";
  /** Read for its `px` size and the words `bold` and `monospace`. */
  font: string;
  textAlign: "start" | "left" | "center" | "right" | "end";
  save(): void;
  restore(): void;
  beginPath(): void;
  closePath(): void;
  moveTo(x: number, y: number): void;
  lineTo(x: number, y: number): void;
  quadraticCurveTo(cpx: number, cpy: number, x: number, y: number): void;
  bezierCurveTo(cp1x: number, cp1y: number, cp2x: number, cp2y: number, x: number, y: number): void;
  /** Angles in radians, clockwise unless `counterclockwise`. */
  arc(x: number, y: number, radius: number, startAngle: number, endAngle: number, counterclockwise?: boolean): void;
  rect(x: number, y: number, width: number, height: number): void;
  fill(): void;
  stroke(): void;
  fillRect(x: number, y: number, width: number, height: number): void;
  strokeRect(x: number, y: number, width: number, height: number): void;
  /** `y` is the text's baseline. */
  fillText(text: string, x: number, y: number): void;
}

/** A drawing surface in points, 1 to 4,000 on each side. Use it inside `view`. */
declare class Canvas {
  constructor(width: number, height: number);
  readonly width: number;
  readonly height: number;
  getContext(kind: "2d"): CanvasContext2D;
  /** The drawing as a view component. */
  node(): CanvasNode;
}
