// The types a view plugin written in TypeScript checks against. A plugin is one module that
// exports `inputs`, the captured file paths it reads, and `view`, which turns those files into
// a view. The host compiles and runs it; it has no imports, no network and no files.
//
//   export const inputs = ["verdicts.json"];
//   export function view({ files }: ViewRequest): View { ... }

/** A colour role from the app's theme. */
type Tone = "neutral" | "muted" | "good" | "warn" | "bad" | "info";

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
  /** An http(s) address the cell opens. */
  href?: string;
  tone?: Tone;
}

/** One component: an object with one key, its name. */
type ViewNode =
  | { stack: { children: ViewNode[] } }
  | { tabs: { id: string; tabs: { label: string; count?: number; body: ViewNode }[] } }
  | { stats: { items: { label: string; value: string; tone?: Tone }[] } }
  | { bars: { bars: { label: string; segments: { label: string; value: number; tone?: Tone }[] }[] } }
  | {
      plot: {
        kind: "line" | "scatter";
        x: string;
        y: string;
        series: { label: string; tone?: Tone; points: [number, number][] }[];
      };
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
          kind?: "text" | "mono" | "badge";
          width?: "narrow" | "medium" | "wide" | "fill";
        }[];
        rows: { id: string; cells: ViewCell[] }[];
      };
    }
  | { text: { text: string; tone?: Tone } }
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
