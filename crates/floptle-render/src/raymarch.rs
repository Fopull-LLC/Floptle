//! A raymarched SDF-matter pass, composited with the raster meshes.
//!
//! It folds two kinds of matter into one field with smin: an analytic morphing
//! blob and a **baked mesh volume** — a 3D signed-distance texture + a co-located
//! color texture produced by `floptle_field::mesh2sdf`, so an imported mesh becomes
//! textured SDF matter that blends (distance *and* color) with everything else.
//! Rays are camera-relative (from inverse(view_proj)) and the fragment writes
//! frag_depth, so it shares one depth buffer with the raster meshes.

use floptle_field::BakedSdf;

use crate::device::Gpu;
use crate::mesh::TextureData;

/// Uniform driving the raymarch — matches `struct Globals` in `raymarch.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct RaymarchGlobals {
    pub view_proj: [[f32; 4]; 4],
    pub inv_view_proj: [[f32; 4]; 4],
    pub light_dir: [f32; 4],
    pub light_color: [f32; 4],
    pub ambient: [f32; 4],
    pub bg: [f32; 4],
    /// Unused legacy field (blobs now live in `blobs`).
    pub center: [f32; 4],
    /// x = time, y = blob count.
    pub params: [f32; 4],
    /// Baked volume: xyz camera-relative box center, w = present (1.0/0.0).
    pub vol_center: [f32; 4],
    /// xyz half-extent, w = blend radius k.
    pub vol_half: [f32; 4],
    /// Terrain surface material (mirrors the raster `MaterialParams`) so terrain shades
    /// with the same lighting model as the meshes instead of a hardcoded look. Ignored
    /// by blobs. `terrain_tint`: rgb tint (× painted albedo), a = unused.
    pub terrain_tint: [f32; 4],
    /// rgb emissive, a = strength.
    pub terrain_emissive: [f32; 4],
    /// rgb specular, a = strength.
    pub terrain_specular: [f32; 4],
    /// x = shininess, y = rim strength, z = unlit (0/1), w = ambient multiplier.
    pub terrain_params: [f32; 4],
    /// rgb rim/fresnel color, a = unused.
    pub terrain_rim: [f32; 4],
    /// Up to 16 blobs: each xyz camera-relative center, w = scale.
    pub blobs: [[f32; 4]; 16],
    /// x = active point-light count (rest pad to a vec4).
    pub point_count: [f32; 4],
    /// Up to 16 point lights: xyz = camera-relative position, w = range.
    pub point_pos: [[f32; 4]; 16],
    /// Each point light's rgb = color × intensity (w unused).
    pub point_color: [[f32; 4]; 16],
}

impl Default for RaymarchGlobals {
    fn default() -> Self {
        // A neutral terrain material (white tint, no emissive/specular/rim, ambient×1)
        // matching `Material::default()`; everything else zero.
        Self {
            view_proj: [[0.0; 4]; 4],
            inv_view_proj: [[0.0; 4]; 4],
            light_dir: [0.0; 4],
            light_color: [0.0; 4],
            ambient: [0.0; 4],
            bg: [0.0; 4],
            center: [0.0; 4],
            params: [0.0; 4],
            vol_center: [0.0; 4],
            vol_half: [1.0, 1.0, 1.0, 0.5],
            terrain_tint: [1.0, 1.0, 1.0, 1.0],
            terrain_emissive: [0.0; 4],
            terrain_specular: [1.0, 1.0, 1.0, 0.0],
            terrain_params: [16.0, 0.0, 0.0, 1.0],
            terrain_rim: [0.0; 4],
            blobs: [[0.0; 4]; 16],
            point_count: [0.0; 4],
            point_pos: [[0.0; 4]; 16],
            point_color: [[0.0; 4]; 16],
        }
    }
}

/// Max blobs the raymarch shader folds together in one pass.
pub const MAX_BLOBS: usize = 16;

/// Max placeable point lights accumulated in one pass (raster + raymarch).
pub const MAX_POINT_LIGHTS: usize = 16;

pub struct Raymarch {
    pipeline: wgpu::RenderPipeline,
    /// Silhouette-mask pipeline (writes 1.0 where the blob is hit, no depth).
    mask_pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    tile_sampler: wgpu::Sampler,
    _dist_tex: wgpu::Texture,
    _color_tex: wgpu::Texture,
    terrain_tex: wgpu::Texture,
    bind: wgpu::BindGroup,
}

/// Layers in the terrain texture palette + the size each is stored at.
pub const TERRAIN_SLOTS: u32 = 6;
const TERRAIN_TEX_SIZE: u32 = 256;

impl Raymarch {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raymarch"),
            source: wgpu::ShaderSource::Wgsl(include_str!("raymarch.wgsl").into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("raymarch"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                vol_tex_entry(1),
                vol_tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // The terrain texture palette (2D array, triplanar-mapped).
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                // A REPEAT sampler for the terrain palette so triplanar textures tile
                // (the volume sampler is ClampToEdge for the [0,1] 3D field).
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raymarch"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raymarch"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu.surface_format(),
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        // Silhouette-mask pipeline: same march, but writes 1.0 (no depth) into a
        // single-channel mask for the selection-outline post-pass.
        let mask_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raymarch-mask"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_mask"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: crate::outline::MASK_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raymarch-globals"),
            size: std::mem::size_of::<RaymarchGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Trilinear sampling of the distance/color volumes, clamped at the border.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raymarch-vol"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        // Repeating sampler for the triplanar terrain palette, so textures TILE
        // across the surface instead of stretching once over the whole terrain.
        let tile_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raymarch-terrain-tile"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // A 1³ "empty" volume so the bindings are valid before a mesh is baked.
        let empty = BakedSdf {
            dims: [1, 1, 1],
            center: [0.0; 3],
            half_extent: [1.0; 3],
            distance: vec![1.0e9],
            color: vec![[255, 255, 255, 255]],
        };
        let (dist_tex, color_tex) = upload_volume(gpu, &empty);
        let terrain_tex = make_terrain_array(gpu, &[]);
        let bind = make_bind(
            device, &bind_layout, &globals_buf, &dist_tex, &color_tex, &sampler, &terrain_tex,
            &tile_sampler,
        );

        Self {
            pipeline,
            mask_pipeline,
            globals_buf,
            bind_layout,
            sampler,
            tile_sampler,
            _dist_tex: dist_tex,
            _color_tex: color_tex,
            terrain_tex,
            bind,
        }
    }

    /// Upload the terrain texture palette (up to [`TERRAIN_SLOTS`] layers, each
    /// already resized to 256×256 RGBA8 by the caller). Slot order maps to the
    /// painted alpha index (slot n = palette layer n).
    pub fn set_terrain_textures(&mut self, gpu: &Gpu, layers: &[TextureData]) {
        self.terrain_tex = make_terrain_array(gpu, layers);
        self.bind = make_bind(
            &gpu.device,
            &self.bind_layout,
            &self.globals_buf,
            &self._dist_tex,
            &self._color_tex,
            &self.sampler,
            &self.terrain_tex,
            &self.tile_sampler,
        );
    }

    /// Upload a baked mesh as the volume matter (replaces any previous one). The
    /// runtime still drives `vol_center`/`vol_half`/present via `RaymarchGlobals`.
    ///
    /// Fast path: when the grid dimensions are unchanged (e.g. sculpting/painting a
    /// fixed-detail terrain), the existing GPU textures are reused and only their
    /// data is re-written — no texture allocation or bind-group rebuild, which is
    /// what made per-stroke editing lag.
    pub fn set_volume(&mut self, gpu: &Gpu, baked: &BakedSdf) {
        let [w, h, d] = baked.dims;
        let cur = self._dist_tex.size();
        if cur.width == w && cur.height == h && cur.depth_or_array_layers == d {
            write_volume_data(gpu, &self._dist_tex, &self._color_tex, baked);
            return;
        }
        let (dist_tex, color_tex) = upload_volume(gpu, baked);
        self.bind = make_bind(
            &gpu.device,
            &self.bind_layout,
            &self.globals_buf,
            &dist_tex,
            &color_tex,
            &self.sampler,
            &self.terrain_tex,
            &self.tile_sampler,
        );
        self._dist_tex = dist_tex;
        self._color_tex = color_tex;
    }

    /// Clear `color`/`depth` and draw the SDF matter into them (with true depth).
    pub fn draw_into(
        &self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: RaymarchGlobals,
    ) {
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raymarch") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raymarch"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.bind, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }

    /// Render the SDF matter's silhouette as 1.0 into a single-channel mask (clearing
    /// it first) — the selection-outline source for the blob.
    pub fn draw_mask(&self, gpu: &Gpu, mask: &wgpu::TextureView, globals: RaymarchGlobals) {
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raymarch-mask") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raymarch-mask"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: mask,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.mask_pipeline);
            rp.set_bind_group(0, &self.bind, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

fn vol_tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D3,
            multisampled: false,
        },
        count: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn make_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    dist: &wgpu::Texture,
    color: &wgpu::Texture,
    sampler: &wgpu::Sampler,
    terrain: &wgpu::Texture,
    tile_sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    let dist_view = dist.create_view(&wgpu::TextureViewDescriptor::default());
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let terrain_view = terrain.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("raymarch"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&dist_view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&color_view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&terrain_view) },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(tile_sampler) },
        ],
    })
}

/// Create the terrain palette as a `TERRAIN_SLOTS`-layer 256² sRGB array. Provided
/// layers are uploaded (caller pre-resizes to 256²); the rest default to white.
fn make_terrain_array(gpu: &Gpu, layers: &[TextureData]) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width: TERRAIN_TEX_SIZE,
        height: TERRAIN_TEX_SIZE,
        depth_or_array_layers: TERRAIN_SLOTS,
    };
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("terrain-palette"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let white = vec![255u8; (TERRAIN_TEX_SIZE * TERRAIN_TEX_SIZE * 4) as usize];
    for layer in 0..TERRAIN_SLOTS {
        let data = layers
            .get(layer as usize)
            .filter(|t| t.width == TERRAIN_TEX_SIZE && t.height == TERRAIN_TEX_SIZE)
            .map(|t| t.pixels.as_slice())
            .unwrap_or(&white);
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TERRAIN_TEX_SIZE * 4),
                rows_per_image: Some(TERRAIN_TEX_SIZE),
            },
            wgpu::Extent3d {
                width: TERRAIN_TEX_SIZE,
                height: TERRAIN_TEX_SIZE,
                depth_or_array_layers: 1,
            },
        );
    }
    tex
}

/// Create the distance (R16Float) + color (Rgba8Unorm) 3D textures from a bake.
fn upload_volume(gpu: &Gpu, baked: &BakedSdf) -> (wgpu::Texture, wgpu::Texture) {
    let [w, h, d] = baked.dims;
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: d };
    let dist = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sdf-distance"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::R16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sdf-color"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    write_volume_data(gpu, &dist, &color, baked);
    (dist, color)
}

/// Write a bake's distance + color into already-allocated 3D textures (same dims).
/// This is the cheap per-edit path — no allocation, no bind-group rebuild.
fn write_volume_data(gpu: &Gpu, dist: &wgpu::Texture, color: &wgpu::Texture, baked: &BakedSdf) {
    let [w, h, d] = baked.dims;
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: d };
    let dist_f16: Vec<u16> = baked.distance.iter().map(|&v| f32_to_f16(v)).collect();
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: dist,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&dist_f16),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 2), rows_per_image: Some(h) },
        size,
    );
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: color,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&baked.color),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        size,
    );
}

/// Minimal `f32` → IEEE-754 half (`f16` bits). Flushes denormals to ±0 and clamps
/// overflow to ±inf — fine for distance volumes (small magnitudes).
fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = (((bits >> 23) & 0xff) as i32) - 127 + 15;
    let mant = ((bits >> 13) & 0x3ff) as u16;
    if exp <= 0 {
        sign
    } else if exp >= 0x1f {
        sign | 0x7c00
    } else {
        sign | ((exp as u16) << 10) | mant
    }
}
