//! The ◈ Shaders tab's LIVE per-node previews (Unity-style thumbnails).
//!
//! `floptle_shader::preview` turns the open graph into ONE standalone WGSL
//! module that renders every node's value into its own tile of a grid atlas;
//! this module owns the GPU side — the pipeline (rebuilt only when the
//! generated WGSL actually changes), the atlas texture (registered with egui
//! so `draw_node` can blit tiles), and the per-frame uniform uploads (time,
//! knob defaults, and the live-literal lane array, so dragging any value
//! repaints thumbnails without a recompile).
//!
//! Texture slots bind the textures of the first scene Material using the
//! shader (checkerboard fallback), so `sample(...)` previews show real art.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use floptle_core::Material;
use floptle_render::{Gpu, Raster, TexId};
use floptle_shader::graph::NodeKey;
use floptle_shader::ir::{self, ExprKind, Stage};
use floptle_shader::preview::{self, CompiledPreview};

use crate::{Editor, Egui};

/// Atlas tile resolution (thumbnails draw at ~150 px on the node).
pub(crate) const TILE_PX: u32 = 128;

pub(crate) struct ShaderGraphPreview {
    /// The header's 👁 toggle — previews render + reserve node space.
    pub(crate) enabled: bool,
    /// Which atlas tile shows which node (rebuilt every visible frame).
    pub(crate) tiles: HashMap<NodeKey, usize>,
    /// The atlas as an egui texture (present once something compiled).
    pub(crate) tex_id: Option<egui::TextureId>,
    /// Grid shape + atlas pixel size, for tile uv rects.
    pub(crate) grid: (u32, u32),
    pub(crate) atlas_px: (u32, u32),
    /// Preview-only failure (the graph stays fully editable).
    pub(crate) err: Option<String>,
    wgsl_hash: u64,
    stage: Option<Stage>,
    pipeline: Option<wgpu::RenderPipeline>,
    tex_count: usize,
    atlas_view: Option<wgpu::TextureView>,
    bufs: Option<Bufs>,
    binds: Vec<wgpu::BindGroup>,
    /// What the current bind groups have bound (slot textures, base texture).
    bound_tex: Option<(Vec<Option<TexId>>, Option<TexId>)>,
    checker: Option<wgpu::TextureView>,
    sampler: Option<wgpu::Sampler>,
}

impl Default for ShaderGraphPreview {
    fn default() -> Self {
        Self {
            enabled: true,
            tiles: HashMap::new(),
            tex_id: None,
            grid: (1, 1),
            atlas_px: (0, 0),
            err: None,
            wgsl_hash: 0,
            stage: None,
            pipeline: None,
            tex_count: 0,
            atlas_view: None,
            bufs: None,
            binds: Vec::new(),
            bound_tex: None,
            checker: None,
            sampler: None,
        }
    }
}

/// The preview's uniform buffers, sized for the WORST case so they never
/// reallocate: `g` mirrors RasterGlobals (640 B), `globals` the bigger of the
/// two `G` stand-ins (sdf: 1056 B), `pv` grid+nums (1040 B), `p` the max
/// param block (16 uniforms + 8 tiling pairs = 512 B).
struct Bufs {
    g: wgpu::Buffer,
    globals: wgpu::Buffer,
    pv: wgpu::Buffer,
    p: wgpu::Buffer,
}

fn f32_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

impl Editor {
    /// Per-frame driver (before the main GPU destructure): while the ◈
    /// Shaders tab is visible with a checked shader open, keep the preview
    /// atlas compiled, bound and rendered. The graph re-transpiles every
    /// frame (cheap string work) so live literal/knob drags stream through
    /// the lane array; the PIPELINE only rebuilds when the WGSL changes.
    pub(crate) fn update_shader_graph_preview(&mut self, elapsed: f32) {
        let visible = std::mem::take(&mut self.shader_graph.tab_visible);
        if !visible || !self.shader_preview.enabled {
            return;
        }
        let Some(path) = self.shader_graph.path.clone() else { return };
        let Some(ir) = self.shader_graph.ir.clone() else { return };
        // A fresh check every time: live edits mutate the arena, so the
        // reload-time `Checked` may be stale. Mid-edit type breaks simply
        // keep the last good atlas on screen.
        let Ok(ck) = ir::check(&ir) else { return };
        let pairs = preview::preview_targets(&ir, &self.shader_graph.view);
        let targets: Vec<_> = pairs.iter().map(|(_, t)| t.clone()).collect();
        let compiled = match preview::transpile_preview(&ir, &ck, &targets) {
            Ok(c) => c,
            Err(e) => {
                self.shader_preview.err = Some(e.message);
                return;
            }
        };

        // Slot + base textures from the first scene material using this
        // shader — previews show the art the artist actually assigned.
        let mat = self
            .world
            .query::<Material>()
            .find(|(_, m)| m.shader.as_deref() == Some(path.as_str()))
            .map(|(_, m)| m.clone());
        let slot_paths: Vec<Option<String>> = compiled
            .textures
            .iter()
            .map(|slot| mat.as_ref().and_then(|m| m.shader_textures.get(slot).cloned()))
            .collect();
        let slot_tex: Vec<Option<TexId>> =
            slot_paths.iter().map(|p| p.as_deref().and_then(|p| self.ensure_texture(p))).collect();
        let base_tex = mat
            .as_ref()
            .and_then(|m| m.texture.clone())
            .and_then(|p| self.ensure_texture(&p));

        self.shader_preview.tiles =
            pairs.into_iter().enumerate().map(|(i, (k, _))| (k, i)).collect();

        let (Some(gpu), Some(raster), Some(egui)) =
            (self.gpu.as_ref(), self.raster.as_ref(), self.egui.as_mut())
        else {
            return;
        };
        self.shader_preview.render(gpu, raster, egui, &ir, &compiled, &slot_tex, base_tex, elapsed);
    }
}

impl ShaderGraphPreview {
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        gpu: &Gpu,
        raster: &Raster,
        egui: &mut Egui,
        ir: &floptle_shader::ShaderIr,
        compiled: &CompiledPreview,
        slot_tex: &[Option<TexId>],
        base_tex: Option<TexId>,
        elapsed: f32,
    ) {
        self.ensure_static(gpu);

        // ---- pipeline (only when the generated WGSL actually changed) ----
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        compiled.wgsl.hash(&mut hasher);
        let h = hasher.finish();
        let mut rebuilt = false;
        if self.pipeline.is_none()
            || h != self.wgsl_hash
            || self.stage != Some(compiled.stage)
            || self.tex_count != compiled.textures.len()
        {
            // naga-gate before wgpu sees it (a rejected module must never
            // panic the device) — unit tests cover every stdlib path, this
            // guards the long tail.
            if let Err(d) = floptle_shader::validate_module(&compiled.wgsl) {
                self.err = Some(format!("preview shader rejected: {}", d.message));
                return;
            }
            self.build_pipeline(gpu, compiled);
            self.wgsl_hash = h;
            self.stage = Some(compiled.stage);
            self.tex_count = compiled.textures.len();
            rebuilt = true;
        }

        // ---- atlas (recreate + re-register with egui on grid change) ----
        let px = (compiled.cols * TILE_PX, compiled.rows * TILE_PX);
        if self.atlas_view.is_none() || self.atlas_px != px {
            let srgb = gpu.surface_format();
            let linear = srgb.remove_srgb_suffix();
            let view_formats: &[wgpu::TextureFormat] =
                if linear != srgb { std::slice::from_ref(&linear) } else { &[] };
            let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("shader-preview-atlas"),
                size: wgpu::Extent3d { width: px.0, height: px.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: srgb,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats,
            });
            self.atlas_view = Some(tex.create_view(&wgpu::TextureViewDescriptor::default()));
            // Same sRGB-render / linear-sample split as every offscreen view
            // (see `make_offscreen_target`) so tiles aren't double-decoded.
            let egui_view = tex.create_view(&wgpu::TextureViewDescriptor {
                format: Some(linear),
                ..Default::default()
            });
            if let Some(old) = self.tex_id.take() {
                egui.renderer.free_texture(&old);
            }
            self.tex_id = Some(egui.renderer.register_native_texture(
                &gpu.device,
                &egui_view,
                wgpu::FilterMode::Linear,
            ));
            self.atlas_px = px;
        }
        self.grid = (compiled.cols, compiled.rows);

        // ---- bind groups (pipeline change or texture set change) ----
        let key = (slot_tex.to_vec(), base_tex);
        if rebuilt || self.bound_tex.as_ref() != Some(&key) {
            self.build_binds(gpu, raster, compiled, slot_tex, base_tex);
            self.bound_tex = Some(key);
        }

        // ---- per-frame uniforms ----
        let Some(bufs) = self.bufs.as_ref() else { return };
        // g: the raster-globals stand-in — a soft key light from the front.
        let mut g = [0f32; 160];
        let l = [0.30f32, 0.45, 0.84]; // ~normalized, +z toward the viewer
        g[16..19].copy_from_slice(&l);
        g[20..23].copy_from_slice(&[1.0, 0.98, 0.92]);
        g[24..27].copy_from_slice(&[0.32, 0.32, 0.36]);
        gpu.queue.write_buffer(&bufs.g, 0, &f32_bytes(&g));
        // G: time (+ knob values for sdf shaders, which read shape_uniforms).
        let mut gl = [0f32; 264];
        gl[0] = elapsed;
        if compiled.stage == Stage::Sdf {
            for (i, u) in compiled.uniforms.iter().enumerate().take(16) {
                gl[8 + i * 4..8 + i * 4 + 4].copy_from_slice(&u.default);
            }
        }
        gpu.queue.write_buffer(&bufs.globals, 0, &f32_bytes(&gl));
        // PV: the tile grid + every live literal's current value.
        let mut pv = [0f32; 260];
        pv[0] = compiled.cols as f32;
        pv[1] = compiled.rows as f32;
        pv[2] = compiled.tiles as f32;
        pv[3] = TILE_PX as f32;
        let mut lane = 4usize;
        for (id, lanes) in &compiled.dyn_slots {
            match &ir.expr(*id).kind {
                ExprKind::Num(n) if *lanes == 1 => pv[lane] = *n as f32,
                ExprKind::ColorLit(c) if *lanes == 4 => pv[lane..lane + 4].copy_from_slice(c),
                _ => {}
            }
            lane += *lanes as usize;
            if lane >= pv.len() {
                break;
            }
        }
        gpu.queue.write_buffer(&bufs.pv, 0, &f32_bytes(&pv));
        // P: knob defaults + neutral tiling (fragment + sky stages both read it).
        if matches!(compiled.stage, Stage::Fragment | Stage::Sky) {
            let mut p = [0f32; 128];
            for (i, u) in compiled.uniforms.iter().enumerate().take(16) {
                p[i * 4..i * 4 + 4].copy_from_slice(&u.default);
            }
            gpu.queue.write_buffer(&bufs.p, 0, &f32_bytes(&p));
        }

        // ---- draw ----
        let (Some(pipeline), Some(view)) = (self.pipeline.as_ref(), self.atlas_view.as_ref())
        else {
            return;
        };
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("shader-preview") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shader-preview"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.06,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(pipeline);
            for (i, bg) in self.binds.iter().enumerate() {
                rp.set_bind_group(i as u32, bg, &[]);
            }
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
        self.err = None;
    }

    /// Buffers, the checkerboard fallback texture and the shared sampler.
    fn ensure_static(&mut self, gpu: &Gpu) {
        if self.bufs.is_none() {
            let mk = |label: &str, size: u64| {
                gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            };
            self.bufs = Some(Bufs {
                g: mk("pv-g", 640),
                globals: mk("pv-globals", 1056),
                pv: mk("pv-pv", 1040),
                p: mk("pv-p", 512),
            });
        }
        if self.checker.is_none() {
            let n = 64u32;
            let mut px = Vec::with_capacity((n * n * 4) as usize);
            for y in 0..n {
                for x in 0..n {
                    let on = ((x / 8) + (y / 8)) % 2 == 0;
                    let c: [u8; 4] =
                        if on { [190, 190, 195, 255] } else { [120, 120, 126, 255] };
                    px.extend_from_slice(&c);
                }
            }
            let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("pv-checker"),
                size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &px,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(n * 4),
                    rows_per_image: Some(n),
                },
                wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
            );
            self.checker = Some(tex.create_view(&wgpu::TextureViewDescriptor::default()));
        }
        if self.sampler.is_none() {
            self.sampler = Some(gpu.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("pv-sampler"),
                address_mode_u: wgpu::AddressMode::Repeat,
                address_mode_v: wgpu::AddressMode::Repeat,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }));
        }
    }

    fn build_pipeline(&mut self, gpu: &Gpu, compiled: &CompiledPreview) {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shader-preview"),
            source: wgpu::ShaderSource::Wgsl(compiled.wgsl.as_str().into()),
        });
        let ubo = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let texture = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let mk = |label: &str, entries: &[wgpu::BindGroupLayoutEntry]| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(label),
                entries,
            })
        };
        let layouts: Vec<wgpu::BindGroupLayout> = match compiled.stage {
            // Sky previews through the Fragment tile path (see preview.rs).
            Stage::Fragment | Stage::Sky => {
                let mut g2 = vec![ubo(0)];
                for i in 0..compiled.textures.len() as u32 {
                    g2.push(texture(1 + 2 * i));
                    g2.push(sampler(2 + 2 * i));
                }
                vec![
                    mk("pv-g0", &[ubo(0), ubo(1), ubo(2)]),
                    mk("pv-g1", &[texture(0), sampler(1)]),
                    mk("pv-g2", &g2),
                ]
            }
            Stage::Sdf => vec![mk("pv-g0", &[ubo(0), ubo(1)])],
        };
        let refs: Vec<Option<&wgpu::BindGroupLayout>> = layouts.iter().map(Some).collect();
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shader-preview"),
            bind_group_layouts: &refs,
            immediate_size: 0,
        });
        self.pipeline = Some(device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shader-preview"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_pv"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_pv"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu.surface_format(),
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        }));
    }

    fn build_binds(
        &mut self,
        gpu: &Gpu,
        raster: &Raster,
        compiled: &CompiledPreview,
        slot_tex: &[Option<TexId>],
        base_tex: Option<TexId>,
    ) {
        let (Some(pipeline), Some(bufs), Some(checker), Some(sampler)) =
            (self.pipeline.as_ref(), self.bufs.as_ref(), self.checker.as_ref(), self.sampler.as_ref())
        else {
            return;
        };
        let view_of = |t: Option<TexId>| -> &wgpu::TextureView {
            t.and_then(|t| raster.texture_view(t)).unwrap_or(checker)
        };
        fn buf(binding: u32, b: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry { binding, resource: b.as_entire_binding() }
        }
        let mut binds = Vec::new();
        match compiled.stage {
            Stage::Fragment | Stage::Sky => {
                binds.push(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("pv-b0"),
                    layout: &pipeline.get_bind_group_layout(0),
                    entries: &[buf(0, &bufs.g), buf(1, &bufs.globals), buf(2, &bufs.pv)],
                }));
                binds.push(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("pv-b1"),
                    layout: &pipeline.get_bind_group_layout(1),
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(view_of(base_tex)),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                    ],
                }));
                let mut entries = vec![buf(0, &bufs.p)];
                for (i, t) in slot_tex.iter().enumerate() {
                    entries.push(wgpu::BindGroupEntry {
                        binding: 1 + 2 * i as u32,
                        resource: wgpu::BindingResource::TextureView(view_of(*t)),
                    });
                    entries.push(wgpu::BindGroupEntry {
                        binding: 2 + 2 * i as u32,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    });
                }
                binds.push(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("pv-b2"),
                    layout: &pipeline.get_bind_group_layout(2),
                    entries: &entries,
                }));
            }
            Stage::Sdf => {
                binds.push(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("pv-b0"),
                    layout: &pipeline.get_bind_group_layout(0),
                    entries: &[buf(0, &bufs.globals), buf(1, &bufs.pv)],
                }));
            }
        }
        self.binds = binds;
    }

    /// The atlas uv rect of tile `i` (half-pixel inset against bleed).
    pub(crate) fn tile_uv(&self, i: usize) -> egui::Rect {
        let (cols, _rows) = self.grid;
        let (w, h) = (self.atlas_px.0.max(1) as f32, self.atlas_px.1.max(1) as f32);
        let (cx, cy) = ((i as u32 % cols.max(1)) as f32, (i as u32 / cols.max(1)) as f32);
        let t = TILE_PX as f32;
        egui::Rect::from_min_max(
            egui::pos2((cx * t + 0.5) / w, (cy * t + 0.5) / h),
            egui::pos2(((cx + 1.0) * t - 0.5) / w, ((cy + 1.0) * t - 0.5) / h),
        )
    }
}
