//! The GPU half of Liquid Glass: what a frame's glass needs drawn before
//! egui's pass, and the glass itself.
//!
//! egui draws a frame in one render pass, and glass needs what was drawn
//! under it. So the first glass callback's `prepare`, before that pass,
//! draws the frame's backdrops itself (`State::run`): group by group (a
//! group is glass that sees the same backdrop: `super::finish`), it draws
//! what was painted before the group's glass into a scene texture as large
//! as the window (with a second egui renderer of its own, on egui's
//! textures), copies the group's region of it (`sharp`), and makes the
//! frost's cloud from that: a quarter size, weighted, blurred across and
//! down (`blur.wgsl`). Each group's commands are submitted before the next
//! one's, as they share the renderer's buffers. Glass of an earlier group
//! is in the scene too (drawn by the same callbacks there), so glass over
//! glass shows it. Then each callback draws its glass in egui's pass
//! (`glass.wgsl`), reading its group's two textures.
//!
//! A callback whose frame was not prepared draws nothing: the flat panel
//! painted under it shows.

use super::{INST_FLOATS, Job};
use eframe::egui::TextureId;
use eframe::egui::epaint::textures::{TextureFilter, TextureOptions, TextureWrapMode};
use eframe::egui::epaint::{ClippedPrimitive, Primitive};
use eframe::egui_wgpu::{CallbackResources, CallbackTrait, Renderer, RendererOptions, ScreenDescriptor};
use eframe::wgpu;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, OnceLock};

/// Each glass's uniforms, and each blur pass's, at this stride (the
/// dynamic offsets' alignment).
pub(super) const STRIDE: u64 = 256;
const BLUR_FLOATS: usize = 12;
/// The cloud's format: a blur of 8-bit colors bands.
const CLOUD: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The widest blur, in taps either side.
const MAX_RADIUS: f32 = 48.0;

/// One glass surface: its frame's job (set once the frame is laid out)
/// and its place in it.
pub(super) struct GlassCallback {
    pub(super) job: Arc<OnceLock<Job>>,
    pub(super) inst: usize,
}

/// Marks the offscreen renderer's resources (glass drawn into the scene).
struct Offscreen;

/// What both renderers' callbacks draw with.
struct Shared {
    main: wgpu::RenderPipeline,
    scene: wgpu::RenderPipeline,
    frame: Mutex<FrameGpu>,
}

#[derive(Default)]
struct FrameGpu {
    id: u64,
    insts: Option<wgpu::BindGroup>,
    groups: Vec<wgpu::BindGroup>,
}

struct Pooled {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    size: [u32; 2],
    format: wgpu::TextureFormat,
    last: u64,
    taken: bool,
}

/// The GPU state, in egui's renderer's callback resources.
struct State {
    shared: Arc<Shared>,
    off: Renderer,
    done: u64,
    scene_format: wgpu::TextureFormat,
    scene: Option<(wgpu::Texture, wgpu::TextureView, [u32; 2])>,
    pool: Vec<Pooled>,
    insts: (wgpu::Buffer, u64),
    blurs: (wgpu::Buffer, u64),
    inst_layout: wgpu::BindGroupLayout,
    tex_layout: wgpu::BindGroupLayout,
    blur_layout: wgpu::BindGroupLayout,
    down: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    /// egui's textures, as the offscreen renderer has them: theirs, ours,
    /// and the frame they were last used in.
    textures: HashMap<TextureId, (wgpu::Texture, TextureId, u64)>,
    tick: u64,
}

impl CallbackTrait for GlassCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(job) = self.job.get() else { return Vec::new() };
        if resources.get::<Offscreen>().is_some() {
            return Vec::new();
        }
        if resources.get::<State>().is_none() {
            let state = State::new(device, job.target);
            resources.insert(state.shared.clone());
            resources.insert(state);
        }
        let state = resources.get_mut::<State>().expect("inserted above");
        if state.done != job.id {
            state.done = job.id;
            state.run(device, queue, job);
        }
        Vec::new()
    }

    fn paint(&self, _info: eframe::egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(job) = self.job.get() else { return };
        let Some(shared) = resources.get::<Arc<Shared>>() else { return };
        let frame = shared.frame.lock().unwrap();
        let (Some(insts), Some(group)) = (frame.insts.as_ref(), job.inst_group.get(self.inst).and_then(|g| frame.groups.get(*g))) else { return };
        if frame.id != job.id {
            return;
        }
        pass.set_pipeline(if resources.get::<Offscreen>().is_some() { &shared.scene } else { &shared.main });
        pass.set_bind_group(0, insts, &[(self.inst as u64 * STRIDE) as u32]);
        pass.set_bind_group(1, group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn uniform_entry(binding: u32, size: usize) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: true,
            min_binding_size: NonZeroU64::new(size as u64),
        },
        count: None,
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

/// Premultiplied alpha over, as egui blends.
const OVER: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
};

fn pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    fragment: &str,
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState { module, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[] },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn uniform_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn bytes(floats: impl IntoIterator<Item = f32>) -> Vec<u8> {
    floats.into_iter().flat_map(f32::to_le_bytes).collect()
}

/// `floats` laid out `STRIDE` apart.
fn strided<const N: usize>(rows: &[[f32; N]]) -> Vec<u8> {
    let mut out = vec![0u8; rows.len() * STRIDE as usize];
    for (i, r) in rows.iter().enumerate() {
        let b = bytes(r.iter().copied());
        out[i * STRIDE as usize..i * STRIDE as usize + b.len()].copy_from_slice(&b);
    }
    out
}

/// egui's sampler for a texture's options.
fn sampler_of(o: &TextureOptions) -> wgpu::SamplerDescriptor<'static> {
    let filter = |f: TextureFilter| match f {
        TextureFilter::Nearest => wgpu::FilterMode::Nearest,
        TextureFilter::Linear => wgpu::FilterMode::Linear,
    };
    let address = match o.wrap_mode {
        TextureWrapMode::Repeat => wgpu::AddressMode::Repeat,
        TextureWrapMode::MirroredRepeat => wgpu::AddressMode::MirrorRepeat,
        _ => wgpu::AddressMode::ClampToEdge,
    };
    wgpu::SamplerDescriptor {
        label: Some("glass-egui-texture"),
        mag_filter: filter(o.magnification),
        min_filter: filter(o.minification),
        address_mode_u: address,
        address_mode_v: address,
        ..Default::default()
    }
}

impl State {
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> State {
        let scene_format = target.remove_srgb_suffix();
        let glass = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glass"),
            source: wgpu::ShaderSource::Wgsl(include_str!("glass.wgsl").into()),
        });
        let blur = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glass-blur"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blur.wgsl").into()),
        });
        let inst_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glass-inst"),
            entries: &[uniform_entry(0, INST_FLOATS * 4)],
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glass-backdrop"),
            entries: &[texture_entry(0), texture_entry(1), sampler_entry(2)],
        });
        let blur_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glass-blur"),
            entries: &[uniform_entry(0, BLUR_FLOATS * 4), texture_entry(1), sampler_entry(2)],
        });
        let glass_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glass"),
            bind_group_layouts: &[Some(&inst_layout), Some(&tex_layout)],
            immediate_size: 0,
        });
        let blur_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glass-blur"),
            bind_group_layouts: &[Some(&blur_layout)],
            immediate_size: 0,
        });
        let main_fs = if target.is_srgb() { "fs_linear" } else { "fs_gamma" };
        let shared = Arc::new(Shared {
            main: pipeline(device, "glass-main", &glass_layout, &glass, main_fs, target, Some(OVER)),
            scene: pipeline(device, "glass-scene", &glass_layout, &glass, "fs_gamma", scene_format, Some(OVER)),
            frame: Mutex::new(FrameGpu::default()),
        });
        let mut off = Renderer::new(device, scene_format, RendererOptions::default());
        off.callback_resources.insert(Offscreen);
        off.callback_resources.insert(shared.clone());
        State {
            down: pipeline(device, "glass-down", &blur_pipeline_layout, &blur, "fs_down", CLOUD, None),
            blur: pipeline(device, "glass-blur", &blur_pipeline_layout, &blur, "fs_blur", CLOUD, None),
            shared,
            off,
            done: 0,
            scene_format,
            scene: None,
            pool: Vec::new(),
            insts: (uniform_buffer(device, "glass-insts", STRIDE * 16), STRIDE * 16),
            blurs: (uniform_buffer(device, "glass-blurs", STRIDE * 24), STRIDE * 24),
            inst_layout,
            tex_layout,
            blur_layout,
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("glass"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            textures: HashMap::new(),
            tick: 0,
        }
    }

    /// A texture of `size` and `format` from the pool (one not taken this
    /// frame), its index.
    fn take(&mut self, device: &wgpu::Device, size: [u32; 2], format: wgpu::TextureFormat, usage: wgpu::TextureUsages) -> usize {
        let tick = self.tick;
        if let Some(i) = self.pool.iter().position(|p| !p.taken && p.size == size && p.format == format) {
            self.pool[i].taken = true;
            self.pool[i].last = tick;
            return i;
        }
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glass-backdrop"),
            size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        self.pool.push(Pooled { tex, view, size, format, last: tick, taken: true });
        self.pool.len() - 1
    }

    /// egui's textures the frame's backdrops use, given to the offscreen
    /// renderer; the ids it knows them by.
    fn sync_textures(&mut self, device: &wgpu::Device, job: &Job) -> HashMap<TextureId, TextureId> {
        let mut ids = HashMap::new();
        for (id, tex, options) in &job.textures {
            let view = tex.create_view(&Default::default());
            let ours = match self.textures.get_mut(id) {
                Some((t, ours, last)) => {
                    if t != tex {
                        self.off.update_egui_texture_from_wgpu_texture_with_sampler_options(device, &view, sampler_of(options), *ours);
                        *t = tex.clone();
                    }
                    *last = self.tick;
                    *ours
                }
                None => {
                    let ours = self.off.register_native_texture_with_sampler_options(device, &view, sampler_of(options));
                    self.textures.insert(*id, (tex.clone(), ours, self.tick));
                    ours
                }
            };
            ids.insert(*id, ours);
        }
        // Textures unused for a while are let go.
        let tick = self.tick;
        let stale: Vec<TextureId> = self.textures.iter().filter(|(_, (_, _, last))| tick - last > 600).map(|(id, _)| *id).collect();
        for id in stale {
            if let Some((_, ours, _)) = self.textures.remove(&id) {
                self.off.free_texture(&ours);
            }
        }
        ids
    }

    fn run(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, job: &Job) {
        self.tick += 1;
        for p in &mut self.pool {
            p.taken = false;
        }
        let tick = self.tick;
        self.pool.retain(|p| tick - p.last < 120);
        let ids = self.sync_textures(device, job);

        // The scene: the window's size.
        if self.scene.as_ref().is_none_or(|s| s.2 != job.screen) {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("glass-scene"),
                size: wgpu::Extent3d { width: job.screen[0], height: job.screen[1], depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.scene_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = tex.create_view(&Default::default());
            self.scene = Some((tex, view, job.screen));
        }

        // Each group's textures: its region of the scene, the cloud, and
        // the cloud's other half for the blur across.
        let mut slots = Vec::with_capacity(job.groups.len());
        for g in &job.groups {
            let sharp = self.take(device, g.tex, self.scene_format, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
            let small = [g.tex[0] / 4, g.tex[1] / 4];
            let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT;
            let cloud = self.take(device, small, CLOUD, usage);
            let other = self.take(device, small, CLOUD, usage);
            slots.push((sharp, cloud, other));
        }

        // The uniforms: each glass's, and each group's three blur passes.
        let need = job.insts.len().max(1) as u64 * STRIDE;
        if self.insts.1 < need {
            self.insts = (uniform_buffer(device, "glass-insts", need.next_power_of_two()), need.next_power_of_two());
        }
        queue.write_buffer(&self.insts.0, 0, &strided(&job.insts));
        let mut blurs: Vec<[f32; BLUR_FLOATS]> = Vec::new();
        for g in &job.groups {
            let small = [(g.tex[0] / 4) as f32, (g.tex[1] / 4) as f32];
            let valid_small = [(g.region[2] as f32 / 4.0).ceil(), (g.region[3] as f32 / 4.0).ceil()];
            let (base, slope) = if g.weight > 1.0 { (1.0 / g.weight, 1.0 - 1.0 / g.weight) } else { (1.0, g.weight - 1.0) };
            let radius = (g.sigma * 3.0).ceil().clamp(1.0, MAX_RADIUS);
            let sharp_texel = [1.0 / g.tex[0] as f32, 1.0 / g.tex[1] as f32];
            blurs.push([sharp_texel[0], sharp_texel[1], 0.0, 0.0, g.sigma, radius, g.region[2] as f32, g.region[3] as f32, base, slope, 0.0, 0.0]);
            for dir in [[1.0, 0.0], [0.0, 1.0]] {
                blurs.push([1.0 / small[0], 1.0 / small[1], dir[0], dir[1], g.sigma.max(0.3), radius, valid_small[0], valid_small[1], 1.0, 0.0, 0.0, 0.0]);
            }
        }
        let need = blurs.len().max(1) as u64 * STRIDE;
        if self.blurs.1 < need {
            self.blurs = (uniform_buffer(device, "glass-blurs", need.next_power_of_two()), need.next_power_of_two());
        }
        queue.write_buffer(&self.blurs.0, 0, &strided(&blurs));

        // What the callbacks draw with this frame.
        let insts = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glass-insts"),
            layout: &self.inst_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: &self.insts.0, offset: 0, size: NonZeroU64::new((INST_FLOATS * 4) as u64) }),
            }],
        });
        let groups: Vec<wgpu::BindGroup> = slots
            .iter()
            .map(|(sharp, cloud, _)| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("glass-backdrop"),
                    layout: &self.tex_layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&self.pool[*sharp].view) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&self.pool[*cloud].view) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    ],
                })
            })
            .collect();
        *self.shared.frame.lock().unwrap() = FrameGpu { id: job.id, insts: Some(insts), groups };

        let screen = ScreenDescriptor { size_in_pixels: job.screen, pixels_per_point: job.ppp };
        for (gi, g) in job.groups.iter().enumerate() {
            // What was painted before the group's glass, on egui's textures
            // as the offscreen renderer knows them (those it doesn't have
            // are left out).
            let prims: Vec<ClippedPrimitive> = g
                .prims
                .iter()
                .filter_map(|p| match &p.primitive {
                    Primitive::Mesh(m) => ids.get(&m.texture_id).map(|ours| {
                        let mut m = m.clone();
                        m.texture_id = *ours;
                        ClippedPrimitive { clip_rect: p.clip_rect, primitive: Primitive::Mesh(m) }
                    }),
                    Primitive::Callback(_) => Some(p.clone()),
                })
                .collect();
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("glass") });
            let mut commands = self.off.update_buffers(device, queue, &mut encoder, &prims, &screen);
            let (scene_tex, scene_view, _) = self.scene.as_ref().expect("made above");
            {
                let load = if gi == 0 { wgpu::LoadOp::Clear(wgpu::Color::BLACK) } else { wgpu::LoadOp::Load };
                let mut pass = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("glass-scene"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: scene_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    })
                    .forget_lifetime();
                self.off.render(&mut pass, &prims, &screen);
            }
            let (sharp, cloud, other) = slots[gi];
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo { texture: scene_tex, mip_level: 0, origin: wgpu::Origin3d { x: g.region[0], y: g.region[1], z: 0 }, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyTextureInfo { texture: &self.pool[sharp].tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::Extent3d { width: g.region[2], height: g.region[3], depth_or_array_layers: 1 },
            );
            // The cloud: down and weighted, then blurred across and down.
            for (k, (src, dst, pipe)) in [(sharp, cloud, &self.down), (cloud, other, &self.blur), (other, cloud, &self.blur)].into_iter().enumerate() {
                let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("glass-blur"),
                    layout: &self.blur_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: &self.blurs.0, offset: 0, size: NonZeroU64::new((BLUR_FLOATS * 4) as u64) }),
                        },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&self.pool[src].view) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    ],
                });
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("glass-frost"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.pool[dst].view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(pipe);
                pass.set_bind_group(0, &bind, &[((gi * 3 + k) as u64 * STRIDE) as u32]);
                pass.draw(0..3, 0..1);
            }
            commands.push(encoder.finish());
            queue.submit(commands);
        }
    }
}
