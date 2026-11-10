// A CRT tube over what an effect slot covers, after crucible-controller/ui/src/charts/crt.ts:
// radial misconvergence, a chroma-gated glow, a saturating beam sweep, scanlines, an aperture
// grille and a vignette. It reads the slot's backdrop copy and replaces those pixels.

struct U {
  // The slot in pass pixels: x, y, w, h.
  rect: vec4f,
  // time, animate (0 or 1), device pixels per point, 0
  p: vec4f,
};
@group(0) @binding(0) var bd: texture_2d<f32>;
@group(0) @binding(1) var<uniform> u: U;

struct VOut { @builtin(position) pos: vec4f };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
  var xy = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
  var o: VOut;
  o.pos = vec4f(xy[i], 0.0, 1.0);
  return o;
}

fn tap(p: vec2f) -> vec4f {
  let size = vec2i(textureDimensions(bd));
  let q = clamp(vec2i(p), vec2i(0), size - vec2i(1));
  return textureLoad(bd, q, 0);
}

fn luma(c: vec3f) -> f32 { return dot(c, vec3f(0.2126, 0.7152, 0.0722)); }

// Saturated marks glow; neutral text and panels don't.
fn bright(c: vec3f) -> vec3f {
  let hi = max(c.r, max(c.g, c.b));
  let sat = (hi - min(c.r, min(c.g, c.b))) / max(hi, 0.001);
  let k = max(luma(c) - 0.18, 0.0) / 0.82;
  return c * k * smoothstep(0.22, 0.40, sat);
}

fn hash(p: vec2f) -> f32 { return fract(sin(dot(p, vec2f(127.1, 311.7))) * 43758.5453); }

@fragment
fn fs(in: VOut) -> @location(0) vec4f {
  let px = in.pos.xy;
  let s = u.p.z;
  let uv = (px - u.rect.xy) / max(u.rect.zw, vec2f(1.0));
  let c = uv - 0.5;
  let r2 = dot(c, c);
  let dir = c / max(length(c), 1e-4);

  let shift = dir * r2 * 2.0 * 0.6 * s;
  let mid = tap(px);
  var col = vec3f(tap(px + shift).r, mid.g, tap(px - shift).b);

  var glow = vec3f(0.0);
  for (var i = 0; i < 8; i++) {
    let a = f32(i) * 0.785398;
    let o = vec2f(cos(a), sin(a));
    glow += bright(tap(px + o * 3.0 * s).rgb) + bright(tap(px + o * 7.0 * s).rgb) * 0.6;
  }
  col += glow / 12.8 * 1.1;

  let lum = luma(col);
  let on = smoothstep(0.10, 0.30, lum);
  let head = fract(u.p.x * 0.13);
  var dx = abs(uv.x - head);
  dx = min(dx, 1.0 - dx);
  let beam = exp(-dx * dx * 260.0) * u.p.y;
  col = clamp(mix(vec3f(lum), col, 1.25 + 0.8 * beam * on), vec3f(0.0), vec3f(1.0));

  let line = sin(px.y * 3.14159 / max(s, 1.0));
  col *= 1.0 - 0.10 * (0.5 - 0.5 * line);

  let m = px.x - 3.0 * floor(px.x / 3.0);
  var grille = vec3f(0.94);
  if (m < 1.0) { grille.r = 1.0; } else if (m < 2.0) { grille.g = 1.0; } else { grille.b = 1.0; }
  col *= grille;

  col *= 1.0 - 0.26 * r2 * 2.2;

  let hum = fract(u.p.x * 0.05);
  let band = abs(fract(uv.y - hum + 0.5) - 0.5);
  col *= 1.0 + 0.04 * u.p.y * smoothstep(0.3, 0.0, band);

  col += (hash(px + fract(u.p.x) * 137.0 * u.p.y) - 0.5) * 0.02;
  return vec4f(col, 1.0);
}
