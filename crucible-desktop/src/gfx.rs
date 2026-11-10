//! The compositor: each `El::effect` slot the view marks becomes one rune-fx layer, or one of
//! the app's own passes: the molten logo and the CRT tube. The low byte
//! of the slot's mark names the effect, its `effect_data` carries a linear-light colour and one
//! number, and its presented rect and corner radius are the shape.
//!
//! The `hot` dev build and the browser build run without a compositor, so their slots draw nothing.
//!
//! Only effects that move keep the window drawing: pulses and shimmer on running work, the glow
//! on a drop target, the CRT beam and the lava while a run is live, and the lava for a moment
//! after a click or a window move. At rest the window draws nothing.

#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
mod crt;
#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
mod molten;

use rune::Color;
#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
use rune::compose::{Compositor, Frame};
#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
use rune::render::{EffectPhase, EffectSite, KIND_EFFECT};
#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
use rune_fx::{
    Fx, FxFrame, FxLayer, FxOptions, FxRenderer, FxSlot, GlowParams, GlowStyle, GrainParams,
    PulseParams, PulseStyle, Rgb, Shape, ShimmerParams,
};

#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
use crate::App;

/// A slow breathing ring around something running.
pub const PULSE: u32 = 2;
/// A light band sweeping across something running.
pub const SHIMMER: u32 = 3;
/// A neon edge: where a dragged pane would land.
pub const GLOW: u32 = 4;
/// Fine static film grain over the window. `data[3]` is the amount.
pub const GRAIN: u32 = 5;

/// The crucible mark as particle lava. `data`: clicks so far, 1 while a run is live, then the
/// slot's screen position in points.
pub const MOLTEN: u32 = 6;
/// A CRT tube over the slot's backdrop. `data[0]` is 1 while its beam and hum should move.
pub const CRT: u32 = 7;

/// `color` as an effect slot's payload: linear-light RGB, then `value`.
pub fn data(color: Color, value: f32) -> [f32; 4] {
    let [r, g, b] = [color.r, color.g, color.b].map(rune::color::srgb_decode);
    [r, g, b, value]
}

#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
/// The layer for one slot, and whether it moves on its own.
fn layer(kind: u32, d: [f32; 4], shape: Shape, t: f32) -> Option<(FxLayer, bool)> {
    let color = Rgb([d[0], d[1], d[2]]);
    let (fx, moving) = match kind {
        PULSE => (
            Fx::pulse(PulseParams {
                style: PulseStyle::Breathe,
                color,
                period: 2.6,
                reach: 10.0,
                intensity: d[3].max(0.2),
                ..Default::default()
            }),
            true,
        ),
        SHIMMER => (
            Fx::shimmer(ShimmerParams {
                color,
                intensity: d[3].max(0.05),
                period: 2.4,
                band: 0.22,
                ..Default::default()
            }),
            true,
        ),
        GLOW => (
            Fx::glow(GlowParams {
                style: GlowStyle::Neon,
                color,
                intensity: d[3].max(0.3),
                radius: 12.0,
                ..Default::default()
            }),
            true,
        ),
        GRAIN => (
            Fx::grain(GrainParams {
                amount: d[3],
                fps: 0.0,
                ..Default::default()
            }),
            false,
        ),
        _ => return None,
    };
    Some((fx.on(shape).at(if moving { t } else { 0.0 }), moving))
}

#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
#[derive(Default)]
pub struct Gfx {
    fx: Option<FxRenderer>,
    molten: molten::Molten,
    crt: crt::Crt,
    moving: bool,
}

#[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
impl Compositor<App> for Gfx {
    fn animating(&self) -> bool {
        self.moving
    }

    fn render(&mut self, f: &mut Frame<'_, App>) {
        let (device, queue, size) = (f.device, f.queue, f.size);
        let t = f.rt.t as f32;
        let mut at: Vec<u32> = Vec::new();
        let mut layers: Vec<FxLayer> = Vec::new();
        let mut slots: Vec<FxSlot> = Vec::new();
        let mut moving = false;
        let mut own: Vec<(u32, u32)> = Vec::new();
        let mut lava: Option<molten::Ask> = None;
        for (n, i) in f.rt.instances.iter().enumerate() {
            if i.params[1] != KIND_EFFECT {
                continue;
            }
            let kind = f.rt.marks.get(n).copied().unwrap_or(0) & 0xff;
            if kind == MOLTEN || kind == CRT {
                own.push((n as u32, kind));
                if kind == MOLTEN {
                    lava = Some(molten::Ask {
                        clicks: i.uv[0],
                        live: i.uv[1] > 0.5,
                        screen: [i.uv[2], i.uv[3]],
                    });
                } else {
                    moving |= i.uv[0] > 0.5;
                }
                continue;
            }
            let k = i.params[3];
            let [x, y, w, h] = i.rect.map(|v| v / k);
            let r = i.radii.iter().fold(0.0f32, |a, v| a.max(v.abs())) / k;
            let Some((l, m)) = layer(kind, i.uv, Shape::rounded(x, y, w, h, r), t) else {
                continue;
            };
            moving |= m;
            at.push(n as u32);
            layers.push(l);
            slots.push(FxSlot::default().scale(k).clip(i.clip, i.fade));
        }
        let linear = !f.srgb_blend();
        if let Some(ask) = lava {
            self.molten.advance(
                device,
                queue,
                f.encoder,
                ask,
                molten::Tick {
                    clock: t,
                    dt: f.dt as f32,
                    linear,
                },
            );
            moving |= self.molten.moving(ask, t);
        }
        self.moving = moving;
        let (lava_gpu, tube) = (&mut self.molten, &mut self.crt);
        let fx = self
            .fx
            .get_or_insert_with(|| FxRenderer::new(device, f.format, FxOptions { dither: true }));
        fx.set_srgb_blend(f.srgb_blend());
        fx.add_format(device, wgpu::TextureFormat::Rgba16Float);
        fx.prepare_with_slots(
            device,
            queue,
            &FxFrame {
                viewport: [size.0, size.1],
                scale: 1.0,
                layers: &layers,
                backdrop: None,
                sources: &[],
            },
            &slots,
        );
        let clear = f.rt.clear_color();
        f.ui.prepare(device, queue, &mut f.rt.text.atlas, &f.rt.instances, size);
        f.ui.draw_with_backdrop(
            f.encoder,
            f.target,
            clear,
            &mut |phase, s: &EffectSite<'_>| {
                if let Some((_, kind)) = own.iter().find(|(n, _)| *n == s.index) {
                    let EffectPhase::Draw(pass) = phase else {
                        return;
                    };
                    let [x, y, w, h] = s.instance.rect;
                    let rect = [x - s.origin[0], y - s.origin[1], w, h];
                    if *kind == MOLTEN {
                        lava_gpu.draw(device, pass, s.format, rect);
                    } else if let Some(backdrop) = s.backdrop {
                        tube.draw(
                            device,
                            queue,
                            pass,
                            crt::Slot {
                                backdrop,
                                format: s.format,
                                rect,
                                time: t,
                                animate: s.data[0] > 0.5,
                                scale: s.scale,
                            },
                        );
                    }
                    pass.set_viewport(0.0, 0.0, s.size[0] as f32, s.size[1] as f32, 0.0, 1.0);
                    return;
                }
                let Some(k) = at.iter().position(|n| *n == s.index) else {
                    return;
                };
                match phase {
                    EffectPhase::Backdrop(enc) => {
                        if let Some(bd) = s.backdrop {
                            fx.blur_backdrop(device, queue, enc, bd, k);
                        }
                    }
                    EffectPhase::Draw(pass) => fx.draw_layer_in(pass, s.format, s.origin, k),
                }
            },
        );
    }
}
