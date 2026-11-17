// What a JavaScript view plugin can use besides the language itself: a canvas that records
// drawing commands under the browser's 2D context names. The host puts this after the
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
