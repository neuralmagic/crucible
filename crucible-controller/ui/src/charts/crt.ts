// WebGL post-process for an ECharts canvas: bright-pass -> separable blur -> CRT composite.
// The source canvas keeps every pointer event; this only draws.

const QUAD_VS = `
attribute vec2 a_pos;
varying vec2 v_uv;
void main() {
  v_uv = a_pos * 0.5 + 0.5;
  gl_Position = vec4(a_pos, 0.0, 1.0);
}`;

const BRIGHT_FS = `
precision mediump float;
uniform sampler2D u_src;
uniform float u_threshold;
uniform float u_chroma;
varying vec2 v_uv;
void main() {
  vec4 c = texture2D(u_src, v_uv);
  float l = dot(c.rgb, vec3(0.2126, 0.7152, 0.0722));
  float k = max(l - u_threshold, 0.0) / max(1.0 - u_threshold, 0.001);
  // Series marks are saturated and labels are neutral, so gating the bright pass on chroma keeps
  // the glow on the data and off the text. Bright grey type contributes nothing to the bloom.
  float hi = max(c.r, max(c.g, c.b));
  float sat = (hi - min(c.r, min(c.g, c.b))) / max(hi, 0.001);
  gl_FragColor = vec4(c.rgb * k * smoothstep(u_chroma, u_chroma + 0.18, sat), c.a);
}`;

const BLUR_FS = `
precision mediump float;
uniform sampler2D u_src;
uniform vec2 u_texel;
uniform vec2 u_dir;
varying vec2 v_uv;
void main() {
  // 9-tap gaussian, linear-sampled offsets
  float w[5];
  w[0] = 0.227027; w[1] = 0.194594; w[2] = 0.121621; w[3] = 0.054054; w[4] = 0.016216;
  vec4 sum = texture2D(u_src, v_uv) * w[0];
  for (int i = 1; i < 5; i++) {
    vec2 off = u_texel * u_dir * float(i) * 2.2;
    sum += texture2D(u_src, v_uv + off) * w[i];
    sum += texture2D(u_src, v_uv - off) * w[i];
  }
  gl_FragColor = sum;
}`;

const PHOSPHOR_FS = `
precision mediump float;
uniform sampler2D u_cur;
uniform sampler2D u_prev;
uniform float u_decay;
varying vec2 v_uv;
void main() {
  vec4 cur = texture2D(u_cur, v_uv);
  vec4 prev = texture2D(u_prev, v_uv) * u_decay;
  // Light adds instantly and fades slowly, so a bar that moves leaves a trail behind it.
  gl_FragColor = max(cur, prev);
}`;

const BLIT_FS = `
precision mediump float;
uniform sampler2D u_src;
varying vec2 v_uv;
void main() { gl_FragColor = texture2D(u_src, v_uv); }`;

const CRT_FS = `
precision mediump float;
uniform sampler2D u_src;
uniform sampler2D u_bloom;
uniform float u_bloomAmt;
uniform vec2 u_res;
uniform float u_scan;
uniform float u_mask;
uniform float u_aberr;
uniform float u_vignette;
uniform float u_hum;
uniform float u_grain;
uniform float u_sat;
uniform float u_sweep;
uniform float u_sweepSpeed;
uniform float u_sweepTight;
uniform float u_time;
varying vec2 v_uv;

float hash(vec2 p) {
  return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453);
}

void main() {
  vec2 uv = v_uv;
  vec4 src = texture2D(u_src, uv);

  // Misconvergence, in device pixels and radial, so it stays sub-pixel at any chart size. Alpha
  // comes from the centre tap: splitting it too would erode the glyph edge the fix just restored.
  vec2 c = uv - 0.5;
  float r2 = dot(c, c);
  vec2 dir = c / max(length(c), 1e-4);
  vec2 shift = dir * (r2 * 2.0) * u_aberr / u_res;
  vec3 col = vec3(
    texture2D(u_src, uv + shift).r,
    src.g,
    texture2D(u_src, uv - shift).b
  );

  // The chart canvas is transparent and the panel's surface colour shows through from CSS, so the
  // post-process has to stay transparent too. Glow spreads past the mark it came from, which needs
  // alpha of its own or it would be composited away.
  vec3 glow = texture2D(u_bloom, uv).rgb * u_bloomAmt;
  col += glow;
  float alpha = clamp(max(src.a, dot(glow, vec3(0.2126, 0.7152, 0.0722)) * 3.4), 0.0, 1.0);

  // Beam: the phosphor saturates where it passes rather than lighting up. Extrapolating away
  // from the grey point deepens the hue and leaves luminance where it was, so it reads as colour
  // rather than a hot spot.
  float lum = dot(col, vec3(0.2126, 0.7152, 0.0722));
  float onBar = smoothstep(0.10, 0.30, lum);
  float head = fract(u_time * u_sweepSpeed);
  float dx = abs(uv.x - head);
  dx = min(dx, 1.0 - dx);
  float beam = exp(-dx * dx * u_sweepTight);
  col = clamp(mix(vec3(lum), col, u_sat + u_sweep * beam * onBar), 0.0, 1.0);

  // Scanlines and the aperture grille only scale colour, never position, and both are masked by
  // alpha so they modulate the tube's content instead of striping the panel behind it.
  float line = sin(uv.y * u_res.y * 3.14159);
  col *= 1.0 - u_scan * (0.5 - 0.5 * line) * src.a;

  float m = mod(gl_FragCoord.x, 3.0);
  vec3 mask = vec3(1.0 - u_mask);
  if (m < 1.0) mask.r = 1.0;
  else if (m < 2.0) mask.g = 1.0;
  else mask.b = 1.0;
  col *= mix(vec3(1.0), mask, src.a);

  col *= 1.0 - u_vignette * r2 * 2.2;

  // Mains hum: a soft band drifting up the tube, the 50Hz beat against the frame rate.
  float humPos = fract(u_time * 0.05);
  float band = abs(fract(uv.y - humPos + 0.5) - 0.5);
  col *= 1.0 + u_hum * smoothstep(0.3, 0.0, band);

  // Grain, resampled every frame so it shimmers rather than sits there.
  col += (hash(gl_FragCoord.xy + fract(u_time) * 137.0) - 0.5) * u_grain;

  gl_FragColor = vec4(col, alpha);
}`;

export interface CrtSettings {
  bloom: number;
  threshold: number;
  scan: number;
  mask: number;
  aberr: number;
  /// Minimum saturation a pixel needs before it blooms, which keeps neutral text out of the glow.
  chroma: number;
  vignette: number;
  hum: number;
  grain: number;
  decay: number;
  sat: number;
  sweep: number;
  sweepSpeed: number;
  sweepTight: number;
}

export interface Crt {
  render: (t?: number) => void;
  settle: () => void;
  pulse: (ms?: number) => void;
  stop: () => void;
  settings: CrtSettings;
}

interface Target {
  tex: WebGLTexture;
  fbo: WebGLFramebuffer;
  w: number;
  h: number;
}

function compile(gl: WebGLRenderingContext, type: number, src: string): WebGLShader {
  const s = gl.createShader(type);
  if (!s) throw new Error('createShader failed');
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) {
    throw new Error('shader: ' + gl.getShaderInfoLog(s));
  }
  return s;
}

function program(gl: WebGLRenderingContext, fs: string): WebGLProgram {
  const p = gl.createProgram();
  if (!p) throw new Error('createProgram failed');
  gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, QUAD_VS));
  gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, fs));
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
    throw new Error('link: ' + gl.getProgramInfoLog(p));
  }
  return p;
}

function target(gl: WebGLRenderingContext, w: number, h: number): Target {
  const tex = gl.createTexture();
  if (!tex) throw new Error('createTexture failed');
  gl.bindTexture(gl.TEXTURE_2D, tex);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  const fbo = gl.createFramebuffer();
  if (!fbo) throw new Error('createFramebuffer failed');
  gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
  gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
  return { tex, fbo, w, h };
}

export function createCRT(
  source: HTMLCanvasElement,
  canvas: HTMLCanvasElement,
  opts: Partial<CrtSettings> = {},
): Crt {
  const ctx = canvas.getContext('webgl', { premultipliedAlpha: false, antialias: false });
  if (!ctx) throw new Error('no webgl');
  const gl: WebGLRenderingContext = ctx;

  const progs = {
    bright: program(gl, BRIGHT_FS),
    blur: program(gl, BLUR_FS),
    crt: program(gl, CRT_FS),
    phosphor: program(gl, PHOSPHOR_FS),
    blit: program(gl, BLIT_FS),
  };

  const quad = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, quad);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);

  const srcTex = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, srcTex);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);

  let a: Target, b: Target, cur: Target, prevA: Target, prevB: Target;
  let W = 0;
  let H = 0;

  function resize(): void {
    const w = source.width, h = source.height;
    if (w === W && h === H) return;
    W = w; H = h;
    canvas.width = w; canvas.height = h;
    // Bloom at quarter res: cheaper, and a wider apparent radius for free.
    a = target(gl, Math.max(1, w >> 2), Math.max(1, h >> 2));
    b = target(gl, Math.max(1, w >> 2), Math.max(1, h >> 2));
    cur = target(gl, w, h);
    prevA = target(gl, w, h);
    prevB = target(gl, w, h);
  }

  function drawQuad(p: WebGLProgram) {
    const loc = gl.getAttribLocation(p, 'a_pos');
    gl.bindBuffer(gl.ARRAY_BUFFER, quad);
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }

  const settings: CrtSettings = {
    bloom: 1.05, threshold: 0.40, chroma: 0.22, vignette: 0.26,
    scan: 0.10, mask: 0.06, aberr: 0.5,
    hum: 0.04, grain: 0.025, decay: 0.92,
    sat: 1.30, sweep: 0.80, sweepSpeed: 0.13, sweepTight: 260.0,
    ...opts,
  };

  function render(t = 0): void {
    resize();
    if (!W || !H) return;

    gl.bindTexture(gl.TEXTURE_2D, srcTex);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, true);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, source);

    // bright pass -> a
    gl.bindFramebuffer(gl.FRAMEBUFFER, a.fbo);
    gl.viewport(0, 0, a.w, a.h);
    gl.useProgram(progs.bright);
    gl.uniform1i(gl.getUniformLocation(progs.bright, 'u_src'), 0);
    gl.uniform1f(gl.getUniformLocation(progs.bright, 'u_threshold'), settings.threshold);
    gl.uniform1f(gl.getUniformLocation(progs.bright, 'u_chroma'), settings.chroma);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, srcTex);
    drawQuad(progs.bright);

    // blur a -> b (h), b -> a (v)
    gl.useProgram(progs.blur);
    gl.uniform1i(gl.getUniformLocation(progs.blur, 'u_src'), 0);
    gl.uniform2f(gl.getUniformLocation(progs.blur, 'u_texel'), 1 / a.w, 1 / a.h);
    const passes: [Target, Target, number, number][] = [
      [a, b, 1, 0],
      [b, a, 0, 1],
    ];
    for (const [from, to, dx, dy] of passes) {
      gl.bindFramebuffer(gl.FRAMEBUFFER, to.fbo);
      gl.viewport(0, 0, to.w, to.h);
      gl.uniform2f(gl.getUniformLocation(progs.blur, 'u_dir'), dx, dy);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, from.tex);
      drawQuad(progs.blur);
    }

    // composite into cur, so the phosphor pass can fold in the previous frame
    gl.bindFramebuffer(gl.FRAMEBUFFER, cur.fbo);
    gl.viewport(0, 0, W, H);
    gl.useProgram(progs.crt);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, srcTex);
    gl.uniform1i(gl.getUniformLocation(progs.crt, 'u_src'), 0);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, a.tex);
    gl.uniform1i(gl.getUniformLocation(progs.crt, 'u_bloom'), 1);
    gl.uniform2f(gl.getUniformLocation(progs.crt, 'u_res'), W, H);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_bloomAmt'), settings.bloom);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_scan'), settings.scan);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_mask'), settings.mask);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_aberr'), settings.aberr);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_vignette'), settings.vignette);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_hum'), settings.hum);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_grain'), settings.grain);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_sat'), settings.sat);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_sweep'), settings.sweep);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_sweepSpeed'), settings.sweepSpeed);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_sweepTight'), settings.sweepTight);
    gl.uniform1f(gl.getUniformLocation(progs.crt, 'u_time'), t / 1000);
    drawQuad(progs.crt);

    // phosphor: max(current, previous * decay) -> prevB
    gl.bindFramebuffer(gl.FRAMEBUFFER, prevB.fbo);
    gl.viewport(0, 0, W, H);
    gl.useProgram(progs.phosphor);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, cur.tex);
    gl.uniform1i(gl.getUniformLocation(progs.phosphor, 'u_cur'), 0);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, prevA.tex);
    gl.uniform1i(gl.getUniformLocation(progs.phosphor, 'u_prev'), 1);
    gl.uniform1f(gl.getUniformLocation(progs.phosphor, 'u_decay'), settings.decay);
    drawQuad(progs.phosphor);

    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, W, H);
    gl.useProgram(progs.blit);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, prevB.tex);
    gl.uniform1i(gl.getUniformLocation(progs.blit, 'u_src'), 0);
    drawQuad(progs.blit);

    const swap = prevA; prevA = prevB; prevB = swap;
  }

  // On-demand driver. Idle costs nothing: the loop only runs inside a pulse window, and settles
  // to one static frame with the animated terms off so nothing shimmers on a parked dashboard.
  let raf: number | null = null;
  let until = 0;

  function settle(): void {
    const { sweep, grain, hum, decay } = settings;
    settings.sweep = 0;
    settings.grain = 0;
    settings.hum = 0;
    settings.decay = 0;
    render(0);
    Object.assign(settings, { sweep, grain, hum, decay });
  }

  function frame(now: number): void {
    render(now);
    if (now < until) {
      raf = requestAnimationFrame(frame);
    } else {
      raf = null;
      settle();
    }
  }

  function pulse(ms = 2600): void {
    if (reducedMotion()) {
      settle();
      return;
    }
    until = performance.now() + ms;
    if (raf === null) raf = requestAnimationFrame(frame);
  }

  function stop(): void {
    if (raf !== null) cancelAnimationFrame(raf);
    raf = null;
    until = 0;
  }

  function reducedMotion(): boolean {
    return window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  }

  return { render, settle, pulse, stop, settings };
}
