//! The CRT pass: redraws a backdrop slot's pixels through `crt.wgsl`.

/// Pipelines per target format, and the layout they share.
#[derive(Default)]
pub struct Crt {
    gpu: Option<Gpu>,
}

struct Gpu {
    module: wgpu::ShaderModule,
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
}

/// One slot's draw: its rect in pass pixels, the clock, whether the beam moves, and its scale.
pub struct Slot<'a> {
    pub backdrop: &'a wgpu::TextureView,
    pub format: wgpu::TextureFormat,
    pub rect: [f32; 4],
    pub time: f32,
    pub animate: bool,
    pub scale: f32,
}

impl Crt {
    fn build(device: &wgpu::Device) -> Gpu {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("crt"),
            source: wgpu::ShaderSource::Wgsl(include_str!("crt.wgsl").into()),
        });
        let fs = wgpu::ShaderStages::FRAGMENT;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("crt"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: fs,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: fs,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("crt"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        Gpu {
            module,
            layout,
            pipeline_layout,
            pipelines: Vec::new(),
        }
    }

    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        slot: Slot<'_>,
    ) {
        let gpu = self.gpu.get_or_insert_with(|| Crt::build(device));
        if !gpu.pipelines.iter().any(|(f, _)| *f == slot.format) {
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("crt"),
                layout: Some(&gpu.pipeline_layout),
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
                        format: slot.format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            gpu.pipelines.push((slot.format, pipeline));
        }
        let Some((_, pipeline)) = gpu.pipelines.iter().find(|(f, _)| *f == slot.format) else {
            return;
        };
        let [x, y, w, h] = slot.rect;
        if w < 1.0 || h < 1.0 {
            return;
        }
        let values = [
            x,
            y,
            w,
            h,
            slot.time,
            f32::from(u8::from(slot.animate)),
            slot.scale,
            0.0,
        ];
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("crt slot"),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&uniform, 0, &bytes);
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("crt slot"),
            layout: &gpu.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(slot.backdrop),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        pass.set_viewport(x, y, w, h, 0.0, 1.0);
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..3, 0..1);
    }
}
