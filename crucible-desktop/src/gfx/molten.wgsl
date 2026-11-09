// The crucible mark as particle lava, ported from crucible-controller/ui/src/moltenSim.ts.
// Compute: clear the density grid, splat particles, integrate; then shade each cell. Render:
// one triangle samples the shaded grid.
struct Params {
  a: vec4f, // time, dt, gravity, sloshAmp
  b: vec4f, // sloshPeriod, stiffness, damping, rest density
  c: vec4f, // kick impulse x and y (this step), dither, particle count
  d: vec4f, // frame, fleck, 0, 0
};
@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read_write> parts: array<vec4f>;
@group(0) @binding(2) var<storage, read_write> dens: array<atomic<u32>>;
@group(0) @binding(3) var<storage, read> mask: array<u32>;
@group(0) @binding(4) var<storage, read_write> shade: array<u32>;

const G: i32 = 96;
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
    pos = vec2f(36.0 + r * 24.0, 48.0 + r2 * 18.0);
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
  vel += P.c.xy;
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
// x: 1 when the target holds linear colour, so the sRGB palette is decoded first.
@group(0) @binding(1) var<uniform> enc: vec4f;

fn decode(c: vec3f) -> vec3f {
  return select(pow((c + 0.055) / 1.055, vec3f(2.4)), c / 12.92, c <= vec3f(0.04045));
}

@fragment
fn fs(in: VOut) -> @location(0) vec4f {
  let cell = vec2i(clamp(floor(in.uv * f32(G)), vec2f(0.0), vec2f(f32(G) - 1.0)));
  let v = shadeR[u32(cell.y * G + cell.x)];
  let a = f32((v >> 24u) & 255u) / 255.0;
  var c = vec3f(f32((v >> 16u) & 255u), f32((v >> 8u) & 255u), f32(v & 255u)) / 255.0;
  if (enc.x > 0.5) { c = decode(c); }
  return vec4f(c * a, a);
}
