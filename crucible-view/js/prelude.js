// What a JavaScript view plugin can use besides the language itself: a canvas that records
// drawing commands under the browser's 2D context names, and `Cell`, which formats numbers
// into table cells the way `crucible_view::Cell` does in Rust. The host puts this after the
// plugin's source, so the plugin's line numbers stay its own, and `epilogue.js` after both.
// A plugin can therefore use it inside its functions, not while its module is first run.
class CanvasContext2D {
  #commands;
  #state = {
    fillStyle: "neutral",
    strokeStyle: "neutral",
    globalAlpha: 1,
    lineWidth: 1,
    lineCap: "butt",
    font: "10px sans-serif",
    textAlign: "start",
    textBaseline: "alphabetic",
  };
  #saved = [];

  constructor(commands) {
    this.#commands = commands;
  }

  #set(name, command, value) {
    this.#state[name] = value;
    this.#commands.push({ [command]: value });
  }

  get fillStyle() { return this.#state.fillStyle; }
  set fillStyle(value) { this.#set("fillStyle", "fill_style", String(value)); }
  get strokeStyle() { return this.#state.strokeStyle; }
  set strokeStyle(value) { this.#set("strokeStyle", "stroke_style", String(value)); }
  get globalAlpha() { return this.#state.globalAlpha; }
  set globalAlpha(value) { this.#set("globalAlpha", "global_alpha", Number(value)); }
  get lineWidth() { return this.#state.lineWidth; }
  set lineWidth(value) { this.#set("lineWidth", "line_width", Number(value)); }
  get lineCap() { return this.#state.lineCap; }
  set lineCap(value) { this.#set("lineCap", "line_cap", value === "square" ? "butt" : String(value)); }
  get font() { return this.#state.font; }
  set font(value) { this.#set("font", "font", String(value)); }
  get textAlign() { return this.#state.textAlign; }
  set textAlign(value) { this.#set("textAlign", "text_align", String(value)); }
  get textBaseline() { return this.#state.textBaseline; }
  set textBaseline(value) {
    const named = String(value);
    this.#state.textBaseline = named;
    const drawn = named === "hanging" ? "top" : named === "ideographic" ? "bottom" : named;
    this.#commands.push({ text_baseline: drawn });
  }

  save() {
    this.#saved.push({ ...this.#state });
    this.#commands.push("save");
  }
  restore() {
    const earlier = this.#saved.pop();
    if (earlier) this.#state = earlier;
    this.#commands.push("restore");
  }
  translate(x, y) { this.#commands.push({ translate: [x, y] }); }
  scale(x, y) { this.#commands.push({ scale: [x, y] }); }
  rotate(angle) { this.#commands.push({ rotate: angle }); }
  transform(a, b, c, d, e, f) { this.#commands.push({ transform: [a, b, c, d, e, f] }); }
  setTransform(a, b, c, d, e, f) {
    if (a === undefined) this.resetTransform();
    else this.#commands.push({ set_transform: [a, b, c, d, e, f] });
  }
  resetTransform() { this.#commands.push("reset_transform"); }
  beginPath() { this.#commands.push("begin_path"); }
  closePath() { this.#commands.push("close_path"); }
  fill() { this.#commands.push("fill"); }
  stroke() { this.#commands.push("stroke"); }
  moveTo(x, y) { this.#commands.push({ move_to: [x, y] }); }
  lineTo(x, y) { this.#commands.push({ line_to: [x, y] }); }
  quadraticCurveTo(cpx, cpy, x, y) {
    this.#commands.push({ quadratic_curve_to: [cpx, cpy, x, y] });
  }
  bezierCurveTo(cp1x, cp1y, cp2x, cp2y, x, y) {
    this.#commands.push({ bezier_curve_to: [cp1x, cp1y, cp2x, cp2y, x, y] });
  }
  arc(x, y, radius, startAngle, endAngle, counterclockwise = false) {
    this.#commands.push({
      arc: { x, y, radius, start_angle: startAngle, end_angle: endAngle, counterclockwise },
    });
  }
  rect(x, y, width, height) { this.#commands.push({ rect: [x, y, width, height] }); }
  fillRect(x, y, width, height) { this.#commands.push({ fill_rect: [x, y, width, height] }); }
  strokeRect(x, y, width, height) { this.#commands.push({ stroke_rect: [x, y, width, height] }); }
  fillText(text, x, y, maxWidth) {
    const drawn = { text: String(text), x, y };
    if (maxWidth !== undefined) drawn.max_width = maxWidth;
    this.#commands.push({ fill_text: drawn });
  }
  /** Not the browser's: shows `text` on hover over the rectangle. */
  hint(x, y, width, height, text) {
    this.#commands.push({ hint: { x, y, width, height, text: String(text) } });
  }
  /** Not the browser's: a rectangle the reader clicks, which runs `view` again with `{ hit: id }`. */
  hit(x, y, width, height, id) {
    this.#commands.push({ hit: { id: String(id), x, y, width, height } });
  }
}

class Canvas {
  #commands = [];
  #context;

  constructor(width, height) {
    this.width = width;
    this.height = height;
    this.#context = new CanvasContext2D(this.#commands);
  }

  getContext(kind) {
    if (kind !== "2d") throw new Error(`a canvas has only a "2d" context, not "${kind}"`);
    return this.#context;
  }

  /** The drawing as a view component. */
  node() {
    return { canvas: { width: this.width, height: this.height, commands: [...this.#commands] } };
  }
}

/** Table cells holding a number shown formatted and sorted by the number itself. */
const Cell = (() => {
  const MAGNITUDE = 1e15;
  const TIME_SPAN = [-62135596800, 253370764800];
  const within = (value) => typeof value === "number" && value >= -MAGNITUDE && value <= MAGNITUDE;
  const round = (value) => BigInt(Math.round(value));
  const group = (n) => String(n).replace(/\B(?=(\d{3})+$)/g, ",");
  const tenths = (n) => (n % 10n === 0n ? group(n / 10n) : `${group(n / 10n)}.${n % 10n}`);
  const formatted = (value, fine, text) => (fine ? { text: text(value), value } : { text: "" });
  const signed = (value, size) => {
    const shown = size(Math.abs(value));
    return value < 0 && /[1-9]/.test(shown) ? `-${shown}` : shown;
  };
  const pair = (big, unit, small, of) =>
    small === 0n ? `${group(big)} ${unit}` : `${group(big)} ${unit} ${small} ${of}`;
  const durationText = (seconds) => {
    const ms = round(seconds * 1000);
    if (ms < 1000n) return `${ms} ms`;
    const tenth = round(seconds * 10);
    if (tenth < 600n) return `${tenths(tenth)} s`;
    const whole = round(seconds);
    if (whole < 3600n) return pair(whole / 60n, "min", whole % 60n, "s");
    const minutes = round(seconds / 60);
    if (minutes < 1440n) return pair(minutes / 60n, "h", minutes % 60n, "min");
    const hours = round(seconds / 3600);
    return pair(hours / 24n, "d", hours % 24n, "h");
  };
  const bytesText = (bytes) => {
    const whole = round(bytes);
    if (whole < 1024n) return `${group(whole)} B`;
    let scale = 1;
    let shown = "";
    for (const unit of ["KiB", "MiB", "GiB", "TiB"]) {
      scale *= 1024;
      const tenth = round((bytes / scale) * 10);
      shown = `${tenths(tenth)} ${unit}`;
      if (tenth < 10240n) break;
    }
    return shown;
  };
  const dateText = (seconds) => {
    const days = Math.floor(seconds / 86400);
    const z = days + 719468;
    const era = Math.floor(z / 146097);
    const ofEra = z - era * 146097;
    const yearOfEra = Math.floor(
      (ofEra - Math.floor(ofEra / 1460) + Math.floor(ofEra / 36524) - Math.floor(ofEra / 146096)) / 365,
    );
    const ofYear = ofEra - (365 * yearOfEra + Math.floor(yearOfEra / 4) - Math.floor(yearOfEra / 100));
    const shifted = Math.floor((5 * ofYear + 2) / 153);
    const day = ofYear - Math.floor((153 * shifted + 2) / 5) + 1;
    const month = shifted < 10 ? shifted + 3 : shifted - 9;
    const year = yearOfEra + era * 400 + (month <= 2 ? 1 : 0);
    const pad = (n, width) => String(n).padStart(width, "0");
    return `${pad(year, 4)}-${pad(month, 2)}-${pad(day, 2)}`;
  };
  return Object.freeze({
    duration: (seconds) => formatted(seconds, within(seconds), (v) => signed(v, durationText)),
    bytes: (bytes) => formatted(bytes, within(bytes), (v) => signed(v, bytesText)),
    percent: (fraction) =>
      formatted(fraction, within(fraction), (v) => signed(v, (size) => `${tenths(round(size * 1000))}%`)),
    count: (count) => formatted(count, within(count), (v) => signed(v, (size) => group(round(size)))),
    date: (seconds) =>
      formatted(seconds, typeof seconds === "number" && seconds >= TIME_SPAN[0] && seconds <= TIME_SPAN[1], dateText),
  });
})();
