// The crucible mark, molten: a WebGPU particle-lava sim ported from the controller UI's
// masthead (crucible-controller/ui/src/moltenSim.ts). Plain script, no bundler, so mdbook can
// load it through `additional-js`.
//
// Mount by putting `data-molten` on a canvas with a sibling `[data-molten-fallback]` image.
// No WebGPU, a device that will not start, or reduced motion each degrade on their own terms:
// the first two reveal the still mark, the third draws one settled frame and stops.
(function () {
// The crucible logo as particle lava: a WebGPU port of scratchpad/crucible-molten tuned for a
// masthead-sized canvas. Compute passes per step: clear grid, splat particles, integrate; one
// shade pass per frame packs a palette per cell; ground cells are transparent so the canvas
// sits on any app surface.

const ART = [
  "...AAAAAAAAAA...",
  "...AAAAAAAAAA...",
  "..RRRRRRRRRRRR..",
  "..WCCCCCCCCCCW..",
  "...WCCCCCCCCW...",
  "...WCCCCCCCCW...",
  "...WMMMMMMMMW...",
  "...WMMMMMMMMW...",
  "...WMMMMMMMMW...",
  "...WMMMMMMMMW...",
  "....WMMMMMMW....",
  ".....WMMMMW.....",
  ".....WWWWWW.....",
  ".....WW..WW.....",
  ".....WW..WW.....",
  "................",
];
const CLASS = { ".": 0, W: 1, R: 2, C: 3, M: 4, A: 5 };
const G = 96;
const SCALE = G / 16;

const MASTHEAD_DEFAULTS = {
  particles: 8192,
  fps: 30,
  gravity: 70,
  stiffness: 1.5,
  damping: 0.6,
  fill: 0.95,
  sloshAmp: 0.35,
  sloshPeriod: 3.2,
  kick: 30,
  kickEvery: 7,
  fleck: 0.35,
  animate: true,
};

const SHADER = /* wgsl */ `
struct Params {
  a: vec4f, // time, dt, gravity, sloshAmp
  b: vec4f, // sloshPeriod, stiffness, damping, rest density
  c: vec4f, // kick impulse (this step), 0, dither, particle count
  d: vec4f, // frame, fleck, 0, 0
};
@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read_write> parts: array<vec4f>;
@group(0) @binding(2) var<storage, read_write> dens: array<atomic<u32>>;
@group(0) @binding(3) var<storage, read> mask: array<u32>;
@group(0) @binding(4) var<storage, read_write> shade: array<u32>;

const G: i32 = ${G};
const FIX: f32 = 256.0;

fn cidx(x: i32, y: i32) -> u32 { return u32(y * G + x); }
fn inb(x: i32, y: i32) -> bool { return x >= 0 && y >= 0 && x < G && y < G; }
fn fluidCell(x: i32, y: i32) -> bool { return inb(x, y) && mask[cidx(x, y)] >= 3u; }
fn fluidAt(p: vec2f) -> bool { return fluidCell(i32(floor(p.x)), i32(floor(p.y))); }
fn densAt(x: i32, y: i32, fallback: f32) -> f32 {
  if (!fluidCell(x, y)) { return fallback; }
  return f32(atomicLoad(&dens[cidx(x, y)])) / FIX;
}

fn hash(p: vec3i) -> f32 {
  var h: u32 = u32(p.x) * 374761393u + u32(p.y) * 668265263u + u32(p.z) * 2147483647u;
  h = (h ^ (h >> 13u)) * 1274126177u;
  h = h ^ (h >> 16u);
  return f32(h & 0xffffffu) / f32(0xffffffu);
}

@compute @workgroup_size(64)
fn clearDens(@builtin(global_invocation_id) gid: vec3u) {
  let i = gid.x;
  if (i < u32(G * G)) { atomicStore(&dens[i], 0u); }
}

@compute @workgroup_size(64)
fn splat(@builtin(global_invocation_id) gid: vec3u) {
  let i = gid.x;
  if (i >= u32(P.c.w)) { return; }
  let p = parts[i].xy - 0.5;
  let c = vec2i(floor(p));
  let f = fract(p);
  for (var dy = 0; dy < 2; dy++) {
    for (var dx = 0; dx < 2; dx++) {
      let x = c.x + dx; let y = c.y + dy;
      if (!inb(x, y)) { continue; }
      let w = select(1.0 - f.x, f.x, dx == 1) * select(1.0 - f.y, f.y, dy == 1);
      atomicAdd(&dens[cidx(x, y)], u32(w * FIX));
    }
  }
}

@compute @workgroup_size(64)
fn integrate(@builtin(global_invocation_id) gid: vec3u) {
  let i = gid.x;
  if (i >= u32(P.c.w)) { return; }
  let t = P.a.x; let dt = P.a.y; let g = P.a.z;
  let stiff = P.b.y; let damp = P.b.z; let rest = P.b.w;
  var pos = parts[i].xy;
  var vel = parts[i].zw;

  let cx = i32(floor(pos.x)); let cy = i32(floor(pos.y));
  if (!fluidCell(cx, cy)) {
    let r = hash(vec3i(i32(i), i32(P.d.x), 11));
    let r2 = hash(vec3i(i32(i), i32(P.d.x), 12));
    pos = vec2f(${6 * SCALE}.0 + r * ${4 * SCALE}.0, ${8 * SCALE}.0 + r2 * ${3 * SCALE}.0);
    vel = vec2f(0.0);
  }

  let dc = densAt(cx, cy, rest);
  let over = max(dc - rest, 0.0);
  let pr = vec2f(
    max(densAt(cx + 1, cy, dc) - rest, 0.0) - max(densAt(cx - 1, cy, dc) - rest, 0.0),
    max(densAt(cx, cy + 1, dc) - rest, 0.0) - max(densAt(cx, cy - 1, dc) - rest, 0.0)
  ) * 0.5;
  var force = -stiff * g * f32(G) * pr / rest;
  let j = vec2f(hash(vec3i(i32(i), cx, cy)) - 0.5, hash(vec3i(cy, i32(i), cx)) - 0.5);
  force += j * (over / rest) * stiff * g * 2.0;

  let ax = P.a.w * g * sin(6.2831853 * t / P.b.x);
  vel += (vec2f(ax, g) + force) * dt;
  vel.x += P.c.x;
  vel *= max(0.0, 1.0 - damp * dt);
  let step = vel * dt;
  let sl = length(step);
  if (sl > 0.9) { vel *= 0.9 / sl; }

  var np = pos + vec2f(vel.x * dt, 0.0);
  if (!fluidAt(np)) { vel.x = -vel.x * 0.35; np.x = pos.x; }
  np.y = pos.y + vel.y * dt;
  if (!fluidAt(np)) { vel.y = -vel.y * 0.25; np.y = pos.y; }
  parts[i] = vec4f(np, vel);
}

fn rgb(hex: u32) -> vec3f {
  return vec3f(f32((hex >> 16u) & 255u), f32((hex >> 8u) & 255u), f32(hex & 255u)) / 255.0;
}
fn pack(c: vec3f) -> u32 {
  let q = vec3u(clamp(c, vec3f(0.0), vec3f(1.0)) * 255.0 + 0.5);
  return 0xff000000u | (q.r << 16u) | (q.g << 8u) | q.b;
}
fn bayer(x: i32, y: i32) -> f32 {
  let m = array<f32, 4>(0.0, 0.5, 0.75, 0.25);
  return m[(y & 1) * 2 + (x & 1)];
}
fn band(v: f32, levels: f32, x: i32, y: i32) -> f32 {
  let d = select(0.5, bayer(x, y), P.c.z > 0.5);
  return floor(v * levels + d) / levels;
}

@compute @workgroup_size(8, 8)
fn shadeCells(@builtin(global_invocation_id) gid: vec3u) {
  let x = i32(gid.x); let y = i32(gid.y);
  if (x >= G || y >= G) { return; }
  let id = cidx(x, y);
  let m = mask[id];
  let rest = P.b.w;
  let frame = i32(P.d.x);
  let wall = rgb(0x241F1Cu);
  let rim = rgb(0x837D74u);
  let cavity = rgb(0x2A2622u);
  let ember = rgb(0x8A2C06u);
  let melt = rgb(0xE8590Cu);
  let hot = rgb(0xFFB020u);
  let white = rgb(0xFFF1B8u);

  // Ground cells stay transparent: the canvas sits on the app's own surface.
  var v = 0u;
  if (m == 1u) { v = pack(wall); }
  else if (m == 2u) { v = pack(rim); }
  else if (m >= 3u) {
    let d = f32(atomicLoad(&dens[id])) / FIX / rest;
    let above = densAt(x, y - 1, 0.0) / rest;
    var c = cavity;
    if (d >= 0.45) {
      let fl = hash(vec3i(x / 2, y / 2 + frame / 14, 5));
      c = melt;
      if (d > 2.4) { c = ember; }
      if (fl > 1.0 - P.d.y * 0.35) { c = hot; }
      if (above < 0.25) { c = hot; if (hash(vec3i(x, frame / 6, 9)) > 0.85) { c = white; } }
    } else if (d > 0.06) {
      c = select(melt, hot, band(d, 2.0, x, y) > 0.5);
      if (hash(vec3i(x, y, frame / 4)) > 0.8) { c = white; }
    }
    v = pack(c);
  }
  shade[id] = v;
}

struct VOut { @builtin(position) pos: vec4f, @location(0) uv: vec2f };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
  var xy = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
  var o: VOut;
  o.pos = vec4f(xy[i], 0.0, 1.0);
  o.uv = vec2f(xy[i].x * 0.5 + 0.5, 0.5 - xy[i].y * 0.5);
  return o;
}

@group(0) @binding(0) var<storage, read> shadeR: array<u32>;

@fragment
fn fs(in: VOut) -> @location(0) vec4f {
  let cell = vec2i(clamp(floor(in.uv * f32(G)), vec2f(0.0), vec2f(f32(G) - 1.0)));
  let v = shadeR[u32(cell.y * G + cell.x)];
  let a = f32((v >> 24u) & 255u) / 255.0;
  let c = vec3f(f32((v >> 16u) & 255u), f32((v >> 8u) & 255u), f32(v & 255u)) / 255.0;
  return vec4f(c * a, a);
}
`;

async function startMolten(canvas, opts) {
  const gpu = navigator.gpu;
  if (gpu === undefined) return null;
  const adapter = await gpu.requestAdapter();
  if (adapter === null) return null;
  const device = await adapter.requestDevice();
  const ctx = canvas.getContext('webgpu');
  if (ctx === null) return null;
  const surface = ctx;
  const format = gpu.getPreferredCanvasFormat();
  surface.configure({ device, format, alphaMode: 'premultiplied' });

  const mask = new Uint32Array(G * G);
  for (let y = 0; y < G; y++) {
    for (let x = 0; x < G; x++) {
      const row = ART[Math.floor(y / SCALE)] ?? '';
      mask[y * G + x] = CLASS[row[Math.floor(x / SCALE)] ?? '.'] ?? 0;
    }
  }
  const meltCells = [];
  for (let i = 0; i < G * G; i++) if (mask[i] === 4) meltCells.push(i);

  const max = opts.particles;
  const partBuf = device.createBuffer({
    size: max * 16,
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
  });
  const densBuf = device.createBuffer({ size: G * G * 4, usage: GPUBufferUsage.STORAGE });
  const shadeBuf = device.createBuffer({ size: G * G * 4, usage: GPUBufferUsage.STORAGE });
  const maskBuf = device.createBuffer({
    size: mask.byteLength,
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
  });
  device.queue.writeBuffer(maskBuf, 0, mask);
  const paramBuf = device.createBuffer({
    size: 64,
    usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
  });
  const pdata = new Float32Array(16);

  const seed = new Float32Array(max * 4);
  for (let i = 0; i < max; i++) {
    const cell = meltCells[Math.floor(Math.random() * meltCells.length)] ?? 0;
    seed[i * 4] = (cell % G) + Math.random();
    seed[i * 4 + 1] = Math.floor(cell / G) + Math.random();
  }
  device.queue.writeBuffer(partBuf, 0, seed);

  const module = device.createShaderModule({ code: SHADER });
  const info = await module.getCompilationInfo();
  if (info.messages.some((m) => m.type === 'error')) return null;

  const computeLayout = device.createBindGroupLayout({
    entries: [
      { binding: 0, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'uniform' } },
      { binding: 1, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
      { binding: 2, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
      { binding: 3, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
      { binding: 4, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
    ],
  });
  const renderLayout = device.createBindGroupLayout({
    entries: [{ binding: 0, visibility: GPUShaderStage.FRAGMENT, buffer: { type: 'read-only-storage' } }],
  });
  const cl = device.createPipelineLayout({ bindGroupLayouts: [computeLayout] });
  const pipeOf = (entryPoint) =>
    device.createComputePipeline({ layout: cl, compute: { module, entryPoint } });
  const pipes = {
    clearDens: pipeOf('clearDens'),
    splat: pipeOf('splat'),
    integrate: pipeOf('integrate'),
    shadeCells: pipeOf('shadeCells'),
  };
  const renderPipe = device.createRenderPipeline({
    layout: device.createPipelineLayout({ bindGroupLayouts: [renderLayout] }),
    vertex: { module, entryPoint: 'vs' },
    fragment: {
      module,
      entryPoint: 'fs',
      targets: [
        {
          format,
          blend: {
            color: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha' },
            alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha' },
          },
        },
      ],
    },
    primitive: { topology: 'triangle-list' },
  });
  const computeGroup = device.createBindGroup({
    layout: computeLayout,
    entries: [
      { binding: 0, resource: { buffer: paramBuf } },
      { binding: 1, resource: { buffer: partBuf } },
      { binding: 2, resource: { buffer: densBuf } },
      { binding: 3, resource: { buffer: maskBuf } },
      { binding: 4, resource: { buffer: shadeBuf } },
    ],
  });
  const renderGroup = device.createBindGroup({
    layout: renderLayout,
    entries: [{ binding: 0, resource: { buffer: shadeBuf } }],
  });

  let t = 0;
  let frame = 0;
  let last = performance.now();
  let lastKick = 0;
  let pendingKick = 0;
  let rafId = 0;
  let stopped = false;
  let animate = opts.animate;
  const rest = opts.particles / (meltCells.length * opts.fill);
  const wg = (n) => Math.ceil(n / 64);

  function simStep(encoder, dt, kick) {
    t += dt;
    pdata.set([
      t, dt, opts.gravity, opts.sloshAmp,
      opts.sloshPeriod, opts.stiffness, opts.damping, rest,
      kick, 0, 1, opts.particles,
      frame++, opts.fleck, 0, 0,
    ]);
    device.queue.writeBuffer(paramBuf, 0, pdata);
    const pass = encoder.beginComputePass();
    pass.setBindGroup(0, computeGroup);
    pass.setPipeline(pipes.clearDens);
    pass.dispatchWorkgroups(wg(G * G));
    pass.setPipeline(pipes.splat);
    pass.dispatchWorkgroups(wg(opts.particles));
    pass.setPipeline(pipes.integrate);
    pass.dispatchWorkgroups(wg(opts.particles));
    pass.end();
  }
  function shadeAndDraw(encoder) {
    const cp = encoder.beginComputePass();
    cp.setBindGroup(0, computeGroup);
    cp.setPipeline(pipes.shadeCells);
    cp.dispatchWorkgroups(G / 8, G / 8);
    cp.end();
    const rp = encoder.beginRenderPass({
      colorAttachments: [
        {
          view: surface.getCurrentTexture().createView(),
          loadOp: 'clear',
          storeOp: 'store',
          clearValue: { r: 0, g: 0, b: 0, a: 0 },
        },
      ],
    });
    rp.setPipeline(renderPipe);
    rp.setBindGroup(0, renderGroup);
    rp.draw(3);
    rp.end();
  }
  function drawStill() {
    const encoder = device.createCommandEncoder();
    for (let i = 0; i < 180; i++) simStep(encoder, 1 / 120, 0);
    simStep(encoder, 0, 0);
    shadeAndDraw(encoder);
    device.queue.submit([encoder.finish()]);
  }
  function tick(now) {
    rafId = 0;
    if (stopped || !animate) return;
    const minDt = 1000 / opts.fps - 2;
    if (now - last < minDt) {
      rafId = requestAnimationFrame(tick);
      return;
    }
    const dt = Math.min(0.04, (now - last) / 1000);
    last = now;
    let kick = pendingKick;
    pendingKick = 0;
    if (opts.kickEvery > 0 && t - lastKick > opts.kickEvery) {
      lastKick = t;
      kick = (Math.random() < 0.5 ? -1 : 1) * opts.kick;
    }
    const encoder = device.createCommandEncoder();
    const sub = 3;
    for (let s = 0; s < sub; s++) simStep(encoder, dt / sub, s === 0 ? kick : 0);
    shadeAndDraw(encoder);
    device.queue.submit([encoder.finish()]);
    rafId = requestAnimationFrame(tick);
  }
  function start() {
    if (stopped) return;
    if (animate) {
      last = performance.now();
      if (rafId === 0) rafId = requestAnimationFrame(tick);
    } else {
      drawStill();
    }
  }
  const onVisibility = () => {
    if (!document.hidden) start();
  };
  document.addEventListener('visibilitychange', onVisibility);
  start();

  return {
    kick(side) {
      pendingKick = side * opts.kick;
      if (!animate) {
        animate = true;
        start();
      }
    },
    setAnimate(on) {
      animate = on;
      start();
    },
    stop() {
      stopped = true;
      if (rafId !== 0) cancelAnimationFrame(rafId);
      document.removeEventListener('visibilitychange', onVisibility);
      device.destroy();
    },
  };
}

  function mount(canvas) {
    var fallback = canvas.parentElement && canvas.parentElement.querySelector('[data-molten-fallback]');
    var still = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    var declared = {};
    try {
      declared = JSON.parse(canvas.getAttribute('data-molten') || '{}');
    } catch (e) {
      declared = {};
    }
    var options = Object.assign({}, MASTHEAD_DEFAULTS, declared, { animate: !still });
    startMolten(canvas, options).then(function (handle) {
      if (handle === null) return;
      canvas.style.opacity = '1';
      if (fallback !== null && fallback !== undefined) fallback.style.display = 'none';
      canvas.addEventListener('pointerdown', function (event) {
        var box = canvas.getBoundingClientRect();
        handle.kick((event.clientX - box.left) / box.width < 0.5 ? 1 : -1);
      });
    }).catch(function () {
      /* No adapter, no device, no shader: the still mark is already showing. */
    });
  }

  function mountAll() {
    var canvases = document.querySelectorAll('canvas[data-molten]');
    for (var i = 0; i < canvases.length; i++) mount(canvases[i]);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', mountAll);
  } else {
    mountAll();
  }
})();
