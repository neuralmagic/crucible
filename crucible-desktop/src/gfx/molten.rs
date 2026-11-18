//! The molten logo on the GPU: the controller UI's particle-lava vessel, stepped in the frame's
//! command encoder and drawn into the brand slot.
//!
//! At rest it settles once and holds that frame. It sloshes while a run is live, for a few seconds
//! after a click, and when the window moves: the lava lags the vessel, so a move to the right
//! throws it left.

const G: usize = 96;
const SCALE: usize = G / 16;
const PARTICLES: u32 = 8192;
const ART: [&str; 16] = [
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

const GRAVITY: f32 = 70.0;
const STIFFNESS: f32 = 1.5;
const DAMPING: f32 = 0.6;
const FILL: f32 = 0.95;
const SLOSH_AMP: f32 = 0.35;
const SLOSH_PERIOD: f32 = 3.2;
const KICK: f32 = 30.0;
const FLECK: f32 = 0.35;
/// Seconds a click or a window move keeps it moving.
const AWAKE: f32 = 4.0;
/// Velocity per point of window travel, and its cap per step.
const INERTIA: f32 = 1.4;
const MAX_IMPULSE: f32 = 60.0;

fn class(c: u8) -> u32 {
    match c {
        b'W' => 1,
        b'R' => 2,
        b'C' => 3,
        b'M' => 4,
        b'A' => 5,
        _ => 0,
    }
}

/// The vessel's cell classes on the simulation grid.
pub fn mask() -> Vec<u32> {
    let mut mask = vec![0u32; G * G];
    for y in 0..G {
        let row = ART[y / SCALE].as_bytes();
        for x in 0..G {
            mask[y * G + x] = class(row[x / SCALE]);
        }
    }
    mask
}

fn whole(buf: &wgpu::Buffer, binding: u32) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buf.as_entire_binding(),
    }
}

fn f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A small deterministic generator for the particles' starting places.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

struct Gpu {
    params: wgpu::Buffer,
    enc: wgpu::Buffer,
    compute: wgpu::BindGroup,
    render: wgpu::BindGroup,
    render_layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    clear: wgpu::ComputePipeline,
    splat: wgpu::ComputePipeline,
    integrate: wgpu::ComputePipeline,
    shade: wgpu::ComputePipeline,
    draws: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
}

/// The lava's state between frames.
pub struct Molten {
    gpu: Option<Gpu>,
    rest: f32,
    t: f32,
    frame: u32,
    settled: bool,
    awake_until: f32,
    clicks: f32,
    /// Window travel (points) since the last step, x then y.
    pending: [f32; 2],
    last_screen: Option<[f32; 2]>,
    kick_every: f32,
    last_kick: f32,
    rng: Lcg,
}

impl Default for Molten {
    fn default() -> Self {
        let melt = mask().iter().filter(|m| **m == 4).count().max(1);
        Molten {
            gpu: None,
            rest: PARTICLES as f32 / (melt as f32 * FILL),
            t: 0.0,
            frame: 0,
            settled: false,
            awake_until: 0.0,
            clicks: 0.0,
            pending: [0.0; 2],
            last_screen: None,
            kick_every: 7.0,
            last_kick: 0.0,
            rng: Lcg(0x5eed),
        }
    }
}

/// The frame's clock and step in seconds, and whether its target holds linear colour.
#[derive(Clone, Copy, Debug)]
pub struct Tick {
    pub clock: f32,
    pub dt: f32,
    pub linear: bool,
}

/// What the brand slot asks for this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ask {
    /// A run is live: slosh on its own.
    pub live: bool,
    /// Clicks so far; a change kicks the lava.
    pub clicks: f32,
    /// The slot's top-left on screen, in points, to feel the window move.
    pub screen: [f32; 2],
}

impl Molten {
    /// Whether it needs frames: live work, or a recent click or move.
    pub fn moving(&self, ask: Ask, clock: f32) -> bool {
        ask.live || clock < self.awake_until
    }

    fn build(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Gpu {
        let mask = mask();
        let melt: Vec<usize> = (0..G * G).filter(|i| mask[*i] == 4).collect();
        let mut seed = vec![0f32; PARTICLES as usize * 4];
        for p in seed.chunks_mut(4) {
            let pick = (self.rng.next() * melt.len() as f32) as usize;
            let Some(&cell) = melt.get(pick.min(melt.len().saturating_sub(1))) else {
                break;
            };
            p[0] = (cell % G) as f32 + self.rng.next();
            p[1] = (cell / G) as f32 + self.rng.next();
        }
        let storage = |label, size: u64, init: Option<&[u8]>| {
            let buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            if let Some(bytes) = init {
                queue.write_buffer(&buf, 0, bytes);
            }
            buf
        };
        let parts = storage("molten parts", PARTICLES as u64 * 16, Some(&f32s(&seed)));
        let dens = storage("molten density", (G * G * 4) as u64, None);
        let shade = storage("molten shade", (G * G * 4) as u64, None);
        let mask_buf = storage("molten mask", (G * G * 4) as u64, Some(&u32s(&mask)));
        let uniform = |label, size| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let params = uniform("molten params", 64);
        let enc = uniform("molten encoding", 16);

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("molten"),
            source: wgpu::ShaderSource::Wgsl(include_str!("molten.wgsl").into()),
        });
        let entry = |binding, vis, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: vis,
            ty,
            count: None,
        };
        let buffer = |ty| wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let rw = wgpu::BufferBindingType::Storage { read_only: false };
        let ro = wgpu::BufferBindingType::Storage { read_only: true };
        let cs = wgpu::ShaderStages::COMPUTE;
        let fs = wgpu::ShaderStages::FRAGMENT;
        let compute_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("molten compute"),
            entries: &[
                entry(0, cs, buffer(wgpu::BufferBindingType::Uniform)),
                entry(1, cs, buffer(rw)),
                entry(2, cs, buffer(rw)),
                entry(3, cs, buffer(ro)),
                entry(4, cs, buffer(rw)),
            ],
        });
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("molten draw"),
            entries: &[
                entry(0, fs, buffer(ro)),
                entry(1, fs, buffer(wgpu::BufferBindingType::Uniform)),
            ],
        });
        let compute_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("molten compute"),
            bind_group_layouts: &[Some(&compute_layout)],
            immediate_size: 0,
        });
        let render_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("molten draw"),
            bind_group_layouts: &[Some(&draw_layout)],
            immediate_size: 0,
        });
        let pipe = |entry_point| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry_point),
                layout: Some(&compute_pl),
                module: &module,
                entry_point: Some(entry_point),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let compute = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("molten compute"),
            layout: &compute_layout,
            entries: &[
                whole(&params, 0),
                whole(&parts, 1),
                whole(&dens, 2),
                whole(&mask_buf, 3),
                whole(&shade, 4),
            ],
        });
        let render = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("molten draw"),
            layout: &draw_layout,
            entries: &[whole(&shade, 0), whole(&enc, 1)],
        });
        Gpu {
            clear: pipe("clearDens"),
            splat: pipe("splat"),
            integrate: pipe("integrate"),
            shade: pipe("shadeCells"),
            params,
            enc,
            compute,
            render,
            render_layout,
            module,
            draws: Vec::new(),
        }
    }

    fn step(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        queue: &wgpu::Queue,
        dt: f32,
        kick: [f32; 2],
    ) {
        self.t += dt;
        let params = [
            self.t,
            dt,
            GRAVITY,
            SLOSH_AMP,
            SLOSH_PERIOD,
            STIFFNESS,
            DAMPING,
            self.rest,
            kick[0],
            kick[1],
            1.0,
            PARTICLES as f32,
            self.frame as f32,
            FLECK,
            0.0,
            0.0,
        ];
        self.frame += 1;
        let Some(gpu) = &self.gpu else {
            return;
        };
        queue.write_buffer(&gpu.params, 0, &f32s(&params));
        let groups = |n: u32| n.div_ceil(64);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("molten step"),
            timestamp_writes: None,
        });
        pass.set_bind_group(0, &gpu.compute, &[]);
        pass.set_pipeline(&gpu.clear);
        pass.dispatch_workgroups(groups((G * G) as u32), 1, 1);
        pass.set_pipeline(&gpu.splat);
        pass.dispatch_workgroups(groups(PARTICLES), 1, 1);
        pass.set_pipeline(&gpu.integrate);
        pass.dispatch_workgroups(groups(PARTICLES), 1, 1);
    }

    fn shade(&self, encoder: &mut wgpu::CommandEncoder) {
        let Some(gpu) = &self.gpu else {
            return;
        };
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("molten shade"),
            timestamp_writes: None,
        });
        pass.set_bind_group(0, &gpu.compute, &[]);
        pass.set_pipeline(&gpu.shade);
        pass.dispatch_workgroups((G / 8) as u32, (G / 8) as u32, 1);
    }

    /// Steps the lava for this frame, before the UI pass opens.
    pub fn advance(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        ask: Ask,
        tick: Tick,
    ) {
        let Tick { clock, dt, linear } = tick;
        if self.gpu.is_none() {
            self.gpu = Some(self.build(device, queue));
        }
        if let Some(gpu) = &self.gpu {
            queue.write_buffer(
                &gpu.enc,
                0,
                &f32s(&[f32::from(u8::from(linear)), 0.0, 0.0, 0.0]),
            );
        }
        if !self.settled {
            for _ in 0..180 {
                self.step(encoder, queue, 1.0 / 120.0, [0.0; 2]);
            }
            self.settled = true;
            self.shade(encoder);
        }

        let mut kick = [0.0f32; 2];
        if ask.clicks != self.clicks {
            let side = if self.rng.next() < 0.5 { -1.0 } else { 1.0 };
            kick[0] += side * KICK;
            self.clicks = ask.clicks;
            self.awake_until = clock + AWAKE;
        }
        if let Some(last) = self.last_screen {
            let moved = [ask.screen[0] - last[0], ask.screen[1] - last[1]];
            if moved[0].abs() + moved[1].abs() > 0.5 {
                self.pending[0] += moved[0];
                self.pending[1] += moved[1];
                self.awake_until = clock + AWAKE;
            }
        }
        self.last_screen = Some(ask.screen);
        // The vessel moved; the lava stays put, so it moves the other way inside it.
        for (k, p) in kick.iter_mut().zip(self.pending) {
            *k += (-p * INERTIA).clamp(-MAX_IMPULSE, MAX_IMPULSE);
        }
        self.pending = [0.0; 2];

        if !self.moving(ask, clock) && kick == [0.0; 2] {
            return;
        }
        if ask.live && self.t - self.last_kick > self.kick_every {
            self.last_kick = self.t;
            kick[0] += if self.rng.next() < 0.5 { -KICK } else { KICK };
        }
        let dt = dt.clamp(0.0, 0.04);
        const SUB: usize = 3;
        for s in 0..SUB {
            self.step(
                encoder,
                queue,
                dt / SUB as f32,
                if s == 0 { kick } else { [0.0; 2] },
            );
        }
        self.shade(encoder);
    }

    /// Draws the shaded grid into the slot's device-pixel rect, inside the open UI pass.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        pass: &mut wgpu::RenderPass<'_>,
        format: wgpu::TextureFormat,
        rect: [f32; 4],
    ) {
        let Some(gpu) = &mut self.gpu else {
            return;
        };
        if !gpu.draws.iter().any(|(f, _)| *f == format) {
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("molten draw"),
                layout: Some(&gpu.render_layout),
                vertex: wgpu::VertexState {
                    module: &gpu.module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &gpu.module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            gpu.draws.push((format, pipeline));
        }
        let Some((_, pipeline)) = gpu.draws.iter().find(|(f, _)| *f == format) else {
            return;
        };
        let [x, y, w, h] = rect;
        if w < 1.0 || h < 1.0 {
            return;
        }
        pass.set_viewport(x, y, w, h, 0.0, 1.0);
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &gpu.render, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use crate::gfx::molten::{ART, G, mask};

    #[test]
    fn the_mask_scales_the_art_to_the_grid() {
        let mask = mask();
        assert_eq!(mask.len(), G * G);
        assert!(ART.iter().all(|row| row.len() == 16));
        let at = |x: usize, y: usize| mask[y * G + x];
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(G / 2, 0), 5);
        assert_eq!(at(G / 2, G / 2), 4);
        let melt = mask.iter().filter(|m| **m == 4).count();
        assert_eq!(melt, (8 * 4 + 6 + 4) * 36);
    }
}
