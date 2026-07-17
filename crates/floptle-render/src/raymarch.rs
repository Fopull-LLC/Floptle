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
    /// x = time, y = blob count, z = blob↔volume blend radius k,
    /// w = uploaded volume count (patched by the renderer at draw time).
    pub params: [f32; 4],
    /// Up to [`MAX_VOLUMES`] baked volumes: each xyz camera-relative box center,
    /// w = present (1.0/0.0). Every terrain volume renders at its OWN native
    /// resolution — no shared combined grid (ADR-0015 / multi-volume terrain).
    pub vol_center: [[f32; 4]; 16],
    /// Per volume: xyz half-extent, w = volume↔volume fuse blend radius k.
    pub vol_half: [[f32; 4]; 16],
    /// Per volume: xyz voxel offset inside the shared 3D atlas (renderer-patched
    /// at draw time from the uploaded layout — callers leave this zeroed).
    pub vol_atlas: [[f32; 4]; 16],
    /// Per volume: xyz voxel dimensions (renderer-patched at draw time).
    pub vol_dims: [[f32; 4]; 16],
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
    /// Per-blob surface material (same model as `terrain_*`), indexed by blob so each
    /// blob honors its own assigned Material instead of a single hardcoded look.
    /// `blob_tint`: rgb tint × the blob's procedural color, a = unused.
    pub blob_tint: [[f32; 4]; 16],
    /// rgb emissive, a = strength.
    pub blob_emissive: [[f32; 4]; 16],
    /// rgb specular, a = strength.
    pub blob_specular: [[f32; 4]; 16],
    /// x = shininess, y = rim strength, z = unlit (0/1), w = ambient multiplier.
    pub blob_params: [[f32; 4]; 16],
    /// rgb rim/fresnel color, a = unused.
    pub blob_rim: [[f32; 4]; 16],
    /// Skybox: x = mode (0 = solid `bg`, 1 = equirect texture), y = size (unused by the
    /// shader; the sky is at infinity), zw unused.
    pub sky_params: [f32; 4],
    /// Sky texture tint (rgb × the sampled texel), a = unused.
    pub sky_tint: [f32; 4],
    /// Inverse skybox rotation as 3 column vec4s (xyz = column, w pad): world ray dir →
    /// sky-local dir before the equirect lookup, so a rotating node spins the sky.
    pub sky_rot: [[f32; 4]; 3],
    /// SDF ("true") ambient occlusion, from the scene's PostProcess node when its
    /// AO mode is `Sdf`: x = on (0/1), y = strength (0..1), z = radius (world
    /// units), w unused. Occlusion is sampled from the real distance field along
    /// the surface normal; the raster pass binds the same field, so meshes
    /// RECEIVE it too (they just don't occlude — they aren't in the field).
    pub ao_params: [f32; 4],
    /// Sun shadows (the Lighting node's knobs): x = on (0/1), y = penumbra
    /// sharpness `k` (≈2 dreamy-soft … ≈64 razor-hard), z = strength (0..1),
    /// w = max shadow-march distance (world units).
    pub shadow_params: [f32; 4],
    /// rgb = the color full shadow darkens toward (black = plain darkness),
    /// w = quantize bands (0 = smooth penumbra, 2..=8 = posterized).
    pub shadow_tint: [f32; 4],
    /// x = Bayer-dither the penumbra (0/1); yzw reserved.
    pub shadow_extra: [f32; 4],
    /// x = active proxy-occluder count (see `prox_a`); rest pad to a vec4.
    pub prox_count: [f32; 4],
    /// Up to [`MAX_SHADOW_PROXIES`] proxy occluders — collider shapes standing in
    /// for raster meshes in the shadow march (meshes aren't in the field).
    /// Per proxy: xyz = center / capsule end A (camera-relative), w = radius.
    pub prox_a: [[f32; 4]; 32],
    /// Per proxy: xyz = capsule end B / box half-extents,
    /// w = kind (0 = sphere, 1 = capsule, 2 = box).
    pub prox_b: [[f32; 4]; 32],
    /// Per proxy: the box's orientation quaternion (xyzw); unused otherwise.
    pub prox_rot: [[f32; 4]; 32],
    /// Depth fog: rgb = color (w unused). Appended at the END to stay byte-identical
    /// to `struct Globals` in field.wgsl (which this uniform feeds).
    pub fog_color: [f32; 4],
    /// Depth fog: x = start dist, y = end dist, z = enabled (0/1), w unused.
    pub fog_params: [f32; 4],
    /// Per volume: xyz = camera-relative center of the tight CONTENT box — the
    /// sub-box of the brick that actually holds surface, measured from the baked
    /// voxels at upload (renderer-patched at draw time; callers leave the default).
    /// A generous terrain box is mostly empty air above the hills; bounding the
    /// marches with the content box instead of the brick is what keeps a camera
    /// standing INSIDE the box from paying for all that air.
    pub vol_tight_c: [[f32; 4]; 16],
    /// Per volume: xyz = the tight content box's half-extent (renderer-patched).
    pub vol_tight_h: [[f32; 4]; 16],
    /// Field Shapes (ADR-0007 Sdf stage): x = active count (0..=4).
    pub shape_meta: [f32; 4],
    /// Per shape: xyz = camera-relative position, w = uniform scale.
    pub shape_pos: [[f32; 4]; 4],
    /// Per shape: the INVERSE rotation quaternion (xyzw).
    pub shape_rot: [[f32; 4]; 4],
    /// Per shape: x = bounding radius (world units — march/shadow/span bound).
    pub shape_aux: [[f32; 4]; 4],
    /// Shader-exposed uniform values, 16 vec4 slots per shape.
    pub shape_uniforms: [[f32; 4]; 64],
    /// Per-shape surface material (same model as `terrain_*` / `blob_*`).
    pub shape_tint: [[f32; 4]; 4],
    pub shape_emissive: [[f32; 4]; 4],
    pub shape_specular: [[f32; 4]; 4],
    pub shape_params: [[f32; 4]; 4],
    pub shape_rim: [[f32; 4]; 4],
    /// Sky shader: `[0]` = active (0/1); rest padding.
    pub sky_meta: [f32; 4],
    /// The Sky shader's exposed uniforms (`G.sky_uniforms[i]`), packed by the editor.
    pub sky_uniforms: [[f32; 4]; 16],
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
            vol_center: [[0.0; 4]; 16],
            vol_half: [[1.0, 1.0, 1.0, 0.5]; 16],
            vol_atlas: [[0.0; 4]; 16],
            vol_dims: [[1.0, 1.0, 1.0, 0.0]; 16],
            terrain_tint: [1.0, 1.0, 1.0, 1.0],
            terrain_emissive: [0.0; 4],
            terrain_specular: [1.0, 1.0, 1.0, 0.0],
            terrain_params: [16.0, 0.0, 0.0, 1.0],
            terrain_rim: [0.0; 4],
            blobs: [[0.0; 4]; 16],
            point_count: [0.0; 4],
            point_pos: [[0.0; 4]; 16],
            point_color: [[0.0; 4]; 16],
            blob_tint: [[1.0, 1.0, 1.0, 0.0]; 16],
            blob_emissive: [[0.0; 4]; 16],
            blob_specular: [[1.0, 1.0, 1.0, 0.0]; 16],
            blob_params: [[16.0, 0.0, 0.0, 1.0]; 16],
            blob_rim: [[0.0; 4]; 16],
            sky_params: [0.0; 4],
            sky_tint: [1.0, 1.0, 1.0, 1.0],
            sky_rot: [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]],
            ao_params: [0.0, 0.7, 0.5, 0.0],
            shadow_params: [0.0, 12.0, 1.0, 150.0],
            shadow_tint: [0.0; 4],
            shadow_extra: [0.0; 4],
            prox_count: [0.0; 4],
            prox_a: [[0.0; 4]; 32],
            prox_b: [[0.0; 4]; 32],
            prox_rot: [[0.0, 0.0, 0.0, 1.0]; 32],
            fog_color: [0.0; 4],
            fog_params: [0.0; 4],
            // Effectively unbounded until the renderer patches the real content
            // bounds — an unpatched volume behaves exactly like the full brick.
            vol_tight_c: [[0.0; 4]; 16],
            vol_tight_h: [[1e8, 1e8, 1e8, 0.0]; 16],
            shape_meta: [0.0; 4],
            shape_pos: [[0.0, 0.0, 0.0, 1.0]; 4],
            shape_rot: [[0.0, 0.0, 0.0, 1.0]; 4],
            shape_aux: [[1.0, 0.0, 0.0, 0.0]; 4],
            shape_uniforms: [[0.0; 4]; 64],
            shape_tint: [[1.0, 1.0, 1.0, 0.0]; 4],
            shape_emissive: [[0.0; 4]; 4],
            shape_specular: [[1.0, 1.0, 1.0, 0.0]; 4],
            shape_params: [[16.0, 0.0, 0.0, 1.0]; 4],
            shape_rim: [[0.0; 4]; 4],
            sky_meta: [0.0; 4],
            sky_uniforms: [[0.0; 4]; 16],
        }
    }
}

/// Max Field Shapes (authored SDF shaders) folded into the field per scene.
pub const MAX_FIELD_SHAPES: usize = 4;

/// Max blobs the raymarch shader folds together in one pass.
pub const MAX_BLOBS: usize = 16;

/// Max baked volumes (terrains / mesh bakes) folded together in one pass. Each keeps
/// its native voxel resolution inside a shared 3D atlas.
pub const MAX_VOLUMES: usize = 16;

/// Max placeable point lights accumulated in one pass (raster + raymarch).
pub const MAX_POINT_LIGHTS: usize = 16;

/// Max proxy shadow occluders (collider shapes cast for raster meshes) in one pass.
pub const MAX_SHADOW_PROXIES: usize = 32;

/// One uploaded volume's atlas slot: its voxel origin + dims inside the shared 3D
/// atlas, plus the tight CONTENT bounds in voxel coordinates — the sub-box that
/// actually holds surface (|distance| within ~2 voxels), scanned from the baked
/// grid at upload. The shaders bound all their marches with the content box
/// instead of the full brick (see `vol_tight_*` in field.wgsl): a generous
/// terrain box is mostly empty air above the hills, and without the tight bound
/// a camera standing inside the box marches (and texture-fetches) through all
/// of it, even for rays pointing at open sky.
struct VolSlot {
    origin: [u32; 3],
    dims: [u32; 3],
    tight_min: [f32; 3],
    tight_max: [f32; 3],
}

/// The voxel-space AABB of the content inside `baked`'s sub-box `[min, max)`
/// (`None` when the region is all air). "Content" is any voxel within two voxel
/// widths of the surface — generous enough that trilinear reads and the smin
/// fuse bulge stay inside once the shader adds its own bound margin.
fn content_bounds(baked: &BakedSdf, min: [u32; 3], max: [u32; 3]) -> Option<([f32; 3], [f32; 3])> {
    let [w, h, _d] = baked.dims;
    let voxel = |a: usize| 2.0 * baked.half_extent[a] / baked.dims[a].max(1) as f32;
    let thr = 2.0 * voxel(0).max(voxel(1)).max(voxel(2));
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    let mut any = false;
    for z in min[2]..max[2] {
        for y in min[1]..max[1] {
            let row = ((z * h + y) * w) as usize;
            for x in min[0]..max[0] {
                if baked.distance[row + x as usize] <= thr {
                    let v = [x as f32, y as f32, z as f32];
                    for a in 0..3 {
                        lo[a] = lo[a].min(v[a]);
                        hi[a] = hi[a].max(v[a]);
                    }
                    any = true;
                }
            }
        }
    }
    if !any {
        return None;
    }
    // One voxel of margin each side (a content voxel's trilinear cell reaches its
    // neighbours; +1 because voxel centers sit at index + 0.5), clamped to the grid.
    for ((l, h), dim) in lo.iter_mut().zip(hi.iter_mut()).zip(baked.dims) {
        *l = (*l - 1.0).max(0.0);
        *h = (*h + 2.0).min(dim as f32);
    }
    Some((lo, hi))
}

/// `content_bounds` over the whole grid, as the degenerate center point when the
/// volume is all air (nothing to march — a zero-size tight box skips everything).
fn full_content_bounds(baked: &BakedSdf) -> ([f32; 3], [f32; 3]) {
    content_bounds(baked, [0, 0, 0], baked.dims).unwrap_or_else(|| {
        let c = [
            baked.dims[0] as f32 * 0.5,
            baked.dims[1] as f32 * 0.5,
            baked.dims[2] as f32 * 0.5,
        ];
        (c, c)
    })
}

pub struct Raymarch {
    pipeline: wgpu::RenderPipeline,
    /// Silhouette-mask pipeline (writes 1.0 where the blob is hit, no depth).
    mask_pipeline: wgpu::RenderPipeline,
    /// Kept so [`set_custom_field`](Self::set_custom_field) can rebuild the
    /// pipelines with spliced Field Shape code (the bind layout never changes).
    pipeline_layout: wgpu::PipelineLayout,
    globals_buf: wgpu::Buffer,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    tile_sampler: wgpu::Sampler,
    tile_sampler_nearest: wgpu::Sampler,
    _dist_tex: wgpu::Texture,
    _color_tex: wgpu::Texture,
    /// Atlas layout: each uploaded volume's (voxel offset, voxel dims) inside the
    /// shared 3D textures. Patched into the globals at draw time so callers only
    /// provide world data (`vol_center`/`vol_half`).
    slots: Vec<VolSlot>,
    terrain_tex: wgpu::Texture,
    /// Equirectangular sky texture (1×1 white until a skybox texture is set).
    sky_tex: wgpu::Texture,
    bind: wgpu::BindGroup,
    /// The SHARED field bind group (globals uniform + distance atlas + sampler)
    /// the raster pass binds at group(2), so mesh fragments march the same field
    /// (shadows received + true SDF AO). Rebuilt with the atlas.
    field_layout: wgpu::BindGroupLayout,
    field_bind: wgpu::BindGroup,
    /// The 1x1 "no mesh anywhere" fallback prime view (R32Float = 1.0) that
    /// `bind` always carries — unprimed draws (offscreen previews, probes) and
    /// the mask pass march uncapped.
    prime_fallback: wgpu::TextureView,
    /// The real depth-prepass view, when the editor's surface path primed one —
    /// kept so atlas/sky/palette rebinds can rebuild `bind_primed`.
    prime_view: Option<wgpu::TextureView>,
    /// `bind` with the real prepass depth at binding 7 — what
    /// [`draw_into_primed`](Self::draw_into_primed) uses.
    bind_primed: Option<wgpu::BindGroup>,
    /// The scene's spliced custom code, KEPT so either half can change independently and
    /// the pipeline rebuild still carries both. `field_code` = Field Shapes `(field, color)`;
    /// `sky_fn` = a Sky shader's `flsl_sky`. Both splice into the ONE raymarch module, so a
    /// change to either re-assembles from both. `custom_support` is the shared stdlib the
    /// editor supplies (same string for both — appended once).
    field_code: Option<(String, String)>,
    sky_fn: Option<String>,
    custom_support: String,
}

/// Layers in the terrain texture palette + the size each is stored at.
pub const TERRAIN_SLOTS: u32 = 6;
const TERRAIN_TEX_SIZE: u32 = 256;

impl Raymarch {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;

        // The shared distance-field module (Globals struct, map_d, AO, shadows) is
        // concatenated on — module-scope WGSL declarations are order-independent.
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raymarch"),
            source: wgpu::ShaderSource::Wgsl(Self::assembled_source(None, None, "").into()),
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
                // (the volume sampler is ClampToEdge for the [0,1] 3D field). The sky
                // texture reuses it so an equirect sky wraps seamlessly.
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // The same palette sampled NEAREST — `triplanar` selects per slot from
                // the mask in terrain_tint.w, so a texture marked Pixelated in the
                // Assets panel looks pixelated on terrain, exactly as it does on a mesh.
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // The equirectangular skybox texture (sampled for background pixels).
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // The opaque-mesh depth prepass (march cap). Depth32Float binds as
                // an UNFILTERABLE float texture; the fallback is a 1x1 R32Float
                // holding 1.0 ("no mesh anywhere") for standalone draws/probes.
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raymarch"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let (pipeline, mask_pipeline) = Self::build_pipelines(gpu, &layout, &module);

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

        // Same wrap as `tile_sampler`, nearest filtering — the per-slot alternative.
        let tile_sampler_nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raymarch-terrain-tile-nearest"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // A 1³ "empty" atlas so the bindings are valid before any volume is baked.
        let empty = BakedSdf {
            dims: [1, 1, 1],
            center: [0.0; 3],
            half_extent: [1.0; 3],
            distance: vec![1.0e9],
            color: vec![[255, 255, 255, 255]],
        };
        let (dist_tex, color_tex) = alloc_volume_textures(gpu, [1, 1, 1]);
        write_volume_data(gpu, &dist_tex, &color_tex, &empty, [0, 0, 0]);
        let terrain_tex = make_terrain_array(gpu, &[]);
        let sky_tex = make_sky_texture(gpu, None);
        let prime_fallback = make_prime_fallback(gpu);
        let bind = make_bind(
            device, &bind_layout, &globals_buf, &dist_tex, &color_tex, &sampler, &terrain_tex,
            &tile_sampler, &tile_sampler_nearest, &sky_tex, &prime_fallback,
        );
        let field_layout = field_bind_layout(device);
        let field_bind = make_field_bind(device, &field_layout, &globals_buf, &dist_tex, &sampler);

        Self {
            pipeline,
            mask_pipeline,
            pipeline_layout: layout,
            globals_buf,
            bind_layout,
            sampler,
            tile_sampler,
            tile_sampler_nearest,
            _dist_tex: dist_tex,
            _color_tex: color_tex,
            slots: Vec::new(),
            terrain_tex,
            sky_tex,
            bind,
            field_layout,
            field_bind,
            prime_fallback,
            prime_view: None,
            bind_primed: None,
            field_code: None,
            sky_fn: None,
            custom_support: String::new(),
        }
    }

    /// The pass module's WGSL: `raymarch.wgsl + field.wgsl`, with the Field
    /// Shape stub blocks spliced when code is given — `(field_code, color_code,
    /// support)`. `None` returns the byte-identical baseline concatenation.
    fn assembled_source(
        field_code: Option<(&str, &str)>,
        sky_fn: Option<&str>,
        support: &str,
    ) -> String {
        let base = concat!(include_str!("raymarch.wgsl"), "\n", include_str!("field.wgsl"));
        let mut s = base.to_string();
        if let Some((field, color)) = field_code {
            s = crate::raster::splice_block(
                &s,
                "//[flsl-color-custom-begin]",
                "//[flsl-color-custom-end]",
                color,
            );
            s = crate::raster::splice_block(
                &s,
                "//[flsl-field-custom-begin]",
                "//[flsl-field-custom-end]",
                field,
            );
        }
        if let Some(sky) = sky_fn {
            s = crate::raster::splice_block(
                &s,
                "//[flsl-sky-custom-begin]",
                "//[flsl-sky-custom-end]",
                sky,
            );
        }
        if field_code.is_some() || sky_fn.is_some() {
            format!("{s}\n{support}")
        } else {
            s
        }
    }

    /// Rebuild the raymarch pipelines from the CURRENTLY stored field + sky code. Both
    /// splice into one module, so a change to either re-assembles from both.
    fn rebuild_custom(&mut self, gpu: &Gpu) {
        let field = self.field_code.as_ref().map(|(f, c)| (f.as_str(), c.as_str()));
        let src = Self::assembled_source(field, self.sky_fn.as_deref(), &self.custom_support);
        let module = gpu.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raymarch"),
            source: wgpu::ShaderSource::Wgsl(src.into()),
        });
        let (pipeline, mask_pipeline) = Self::build_pipelines(gpu, &self.pipeline_layout, &module);
        self.pipeline = pipeline;
        self.mask_pipeline = mask_pipeline;
    }

    fn build_pipelines(
        gpu: &Gpu,
        layout: &wgpu::PipelineLayout,
        module: &wgpu::ShaderModule,
    ) -> (wgpu::RenderPipeline, wgpu::RenderPipeline) {
        let device = &gpu.device;
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raymarch"),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                // LessEqual (not Always): when the depth prepass primed the buffer,
                // field hits behind a mesh (and the sky at depth 1.0 under mesh
                // pixels) must lose to the mesh depth. Unprimed frames clear the
                // depth to 1.0 first, where LessEqual accepts everything - the
                // original behavior.
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
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
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
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
        (pipeline, mask_pipeline)
    }

    /// Splice (or clear, with `None`) the scene's Field Shape code into this
    /// pass: `(custom_d distance functions, custom_col/nearest_shape color
    /// functions, the stdlib support library)`. The caller MUST have
    /// naga-validated the assembled source (same recipe as
    /// [`assembled_source`]) first; per-shape transforms/uniforms then ride
    /// the globals — no further rebuilds on edits.
    pub fn set_custom_field(&mut self, gpu: &Gpu, code: Option<(&str, &str, &str)>) {
        match code {
            Some((field, color, support)) => {
                self.field_code = Some((field.to_string(), color.to_string()));
                self.custom_support = support.to_string();
            }
            None => self.field_code = None,
        }
        self.rebuild_custom(gpu);
    }

    /// Splice (or clear, with `None`) a Sky shader's `flsl_sky` into the raymarch's
    /// `sky_color` — the procedural skybox. `support` is the same stdlib as `set_custom_field`.
    /// Composes with Field Shapes: both live in the one module and re-assemble together.
    pub fn set_sky_shader(&mut self, gpu: &Gpu, code: Option<(&str, &str)>) {
        match code {
            Some((sky, support)) => {
                self.sky_fn = Some(sky.to_string());
                if !support.is_empty() {
                    self.custom_support = support.to_string();
                }
            }
            None => self.sky_fn = None,
        }
        self.rebuild_custom(gpu);
    }

    /// The exact source [`set_custom_field`](Self::set_custom_field) would
    /// build — for the editor to naga-validate BEFORE swapping pipelines.
    pub fn preview_custom_source(code: Option<(&str, &str, &str)>) -> String {
        Self::assembled_source(code.map(|(f, c, _)| (f, c)), None, code.map_or("", |(_, _, s)| s))
    }

    /// The source a Sky shader would build — for naga-validation before swapping pipelines.
    pub fn preview_sky_source(sky_fn: &str, support: &str) -> String {
        Self::assembled_source(None, Some(sky_fn), support)
    }

    /// Rebuild `bind` (fallback prime) and, when primed, `bind_primed` — after
    /// any bound resource (atlas, palette, sky, prime) changes.
    fn rebuild_binds(&mut self, device: &wgpu::Device) {
        self.bind = make_bind(
            device,
            &self.bind_layout,
            &self.globals_buf,
            &self._dist_tex,
            &self._color_tex,
            &self.sampler,
            &self.terrain_tex,
            &self.tile_sampler,
            &self.tile_sampler_nearest,
            &self.sky_tex,
            &self.prime_fallback,
        );
        self.bind_primed = self.prime_view.as_ref().map(|v| {
            make_bind(
                device,
                &self.bind_layout,
                &self.globals_buf,
                &self._dist_tex,
                &self._color_tex,
                &self.sampler,
                &self.terrain_tex,
                &self.tile_sampler,
                &self.tile_sampler_nearest,
                &self.sky_tex,
                v,
            )
        });
    }

    /// The field bind group the raster pass binds at group(2) — the same globals
    /// buffer + distance atlas this pass marches, so meshes receive field shadows
    /// and SDF AO. `draw_into` (or [`upload_globals`](Self::upload_globals) on
    /// frames with nothing to raymarch) must run first so the buffer holds this
    /// frame's data — the raymarch pass draws before the raster pass anyway.
    pub fn field_bind(&self) -> &wgpu::BindGroup {
        &self.field_bind
    }

    /// Write `globals` (atlas slots patched) WITHOUT drawing — for frames where no
    /// SDF matter renders but the raster pass still marches the field via
    /// [`field_bind`](Self::field_bind) (mesh-only scenes casting proxy shadows).
    pub fn upload_globals(&self, gpu: &Gpu, mut globals: RaymarchGlobals) {
        self.patch_globals(&mut globals);
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
    }

    /// Bind (or drop, with `None`) the opaque-mesh depth prepass as
    /// [`draw_into_primed`](Self::draw_into_primed)'s per-pixel march cap. Call
    /// when the prepass target is (re)created — a size change needs a new bind
    /// group (they're immutable), not per frame. Plain [`draw_into`]
    /// (Self::draw_into) is unaffected: offscreen previews and probes stay
    /// unprimed with their own cleared depth.
    pub fn set_depth_prime(&mut self, gpu: &Gpu, view: Option<&wgpu::TextureView>) {
        self.prime_view = view.cloned();
        self.rebuild_binds(&gpu.device);
    }

    /// Upload the equirectangular skybox texture (`None` resets to solid / white). The
    /// runtime selects solid vs. texture and the tint/rotation via `RaymarchGlobals`.
    pub fn set_sky_texture(&mut self, gpu: &Gpu, tex: Option<&TextureData>) {
        self.sky_tex = make_sky_texture(gpu, tex);
        self.rebuild_binds(&gpu.device);
    }

    /// Upload the terrain texture palette (up to [`TERRAIN_SLOTS`] layers, each
    /// already resized to 256×256 RGBA8 by the caller). Slot order maps to the
    /// painted alpha index (slot n = palette layer n).
    pub fn set_terrain_textures(&mut self, gpu: &Gpu, layers: &[TextureData]) {
        self.terrain_tex = make_terrain_array(gpu, layers);
        self.rebuild_binds(&gpu.device);
    }

    /// Upload a single baked volume (replaces all previous ones) — the common case
    /// for probes / the runtime demo. See [`set_volumes`](Self::set_volumes).
    pub fn set_volume(&mut self, gpu: &Gpu, baked: &BakedSdf) {
        self.set_volumes(gpu, &[baked]);
    }

    /// Upload a set of baked volumes into one shared 3D atlas, EACH at its native
    /// voxel resolution — far-apart terrains no longer share a coarse combined grid
    /// (the old resolution-spread limit). Volumes stack along the atlas Z axis; the
    /// per-slot offsets/dims are patched into the globals at draw time. Returns how
    /// many volumes were accepted (the rest exceeded the device's 3D-texture limit —
    /// callers should surface that instead of silently dropping content).
    ///
    /// Fast path: a single volume with unchanged dims rewrites the existing texture
    /// data in place (no allocation / bind-group rebuild — keeps sculpting smooth).
    pub fn set_volumes(&mut self, gpu: &Gpu, volumes: &[&BakedSdf]) -> usize {
        let limit = gpu.device.limits().max_texture_dimension_3d;
        // Accept volumes until the Z stack or the XY footprint would exceed the limit.
        let mut accepted = Vec::new();
        let (mut aw, mut ah, mut ad) = (1u32, 1u32, 0u32);
        for &b in volumes.iter().take(MAX_VOLUMES) {
            let [w, h, d] = b.dims;
            if w.max(aw) > limit || h.max(ah) > limit || ad + d > limit {
                break;
            }
            aw = aw.max(w);
            ah = ah.max(h);
            accepted.push((b, [0u32, 0, ad]));
            ad += d;
        }
        ad = ad.max(1);

        // Single unchanged-dims volume: rewrite in place (per-stroke editing path).
        if let [(b, _)] = accepted[..] {
            let cur = self._dist_tex.size();
            if self.slots.len() == 1
                && cur.width == b.dims[0]
                && cur.height == b.dims[1]
                && cur.depth_or_array_layers == b.dims[2]
            {
                write_volume_data(gpu, &self._dist_tex, &self._color_tex, b, [0, 0, 0]);
                let (tight_min, tight_max) = full_content_bounds(b);
                self.slots[0].tight_min = tight_min;
                self.slots[0].tight_max = tight_max;
                return 1;
            }
        }

        let (dist_tex, color_tex) = alloc_volume_textures(gpu, [aw, ah, ad]);
        self.slots.clear();
        for (b, origin) in &accepted {
            write_volume_data(gpu, &dist_tex, &color_tex, b, *origin);
            let (tight_min, tight_max) = full_content_bounds(b);
            self.slots.push(VolSlot { origin: *origin, dims: b.dims, tight_min, tight_max });
        }
        self.field_bind =
            make_field_bind(&gpu.device, &self.field_layout, &self.globals_buf, &dist_tex, &self.sampler);
        self._dist_tex = dist_tex;
        self._color_tex = color_tex;
        self.rebuild_binds(&gpu.device);
        accepted.len()
    }

    /// Upload only the sub-box `[min, max)` (voxel coords) of `baked` into atlas slot
    /// `slot` — the fast path for a brush dab, so painting/editing a huge terrain
    /// doesn't re-convert and re-upload the whole volume every frame. `baked.dims`
    /// MUST match the slot's dims (caller falls back to [`set_volumes`] on a resize).
    pub fn set_volume_region(
        &mut self,
        gpu: &Gpu,
        slot: usize,
        baked: &BakedSdf,
        min: [u32; 3],
        max: [u32; 3],
    ) {
        let Some(s) = self.slots.get(slot) else { return };
        let (off, dims) = (s.origin, s.dims);
        if dims != baked.dims {
            return; // resized since upload — the caller's dirty flag takes the full path
        }
        let [w, h, d] = baked.dims;
        let x0 = min[0].min(w);
        let y0 = min[1].min(h);
        let z0 = min[2].min(d);
        let x1 = max[0].clamp(x0, w);
        let y1 = max[1].clamp(y0, h);
        let z1 = max[2].clamp(z0, d);
        let (rw, rh, rd) = (x1 - x0, y1 - y0, z1 - z0);
        if rw == 0 || rh == 0 || rd == 0 {
            return;
        }
        // Expand the tight content bounds over the dab (raising terrain can push
        // content past the recorded box). Expand-only: lowering leaves the bounds
        // conservatively large until the next full upload re-tightens them.
        if let Some((lo, hi)) = content_bounds(baked, [x0, y0, z0], [x1, y1, z1]) {
            let s = &mut self.slots[slot];
            for a in 0..3 {
                s.tight_min[a] = s.tight_min[a].min(lo[a]);
                s.tight_max[a] = s.tight_max[a].max(hi[a]);
            }
        }
        // Pack the sub-box tightly (x-fastest), converting distance to f16.
        let mut dist = Vec::with_capacity((rw * rh * rd) as usize);
        let mut col = Vec::with_capacity((rw * rh * rd) as usize);
        for z in z0..z1 {
            for y in y0..y1 {
                let row = ((z * h + y) * w) as usize;
                for x in x0..x1 {
                    let i = row + x as usize;
                    dist.push(f32_to_f16(baked.distance[i]));
                    col.push(baked.color[i]);
                }
            }
        }
        let origin = wgpu::Origin3d { x: off[0] + x0, y: off[1] + y0, z: off[2] + z0 };
        let extent = wgpu::Extent3d { width: rw, height: rh, depth_or_array_layers: rd };
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self._dist_tex,
                mip_level: 0,
                origin,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&dist),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(rw * 2), rows_per_image: Some(rh) },
            extent,
        );
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self._color_tex,
                mip_level: 0,
                origin,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&col),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(rw * 4), rows_per_image: Some(rh) },
            extent,
        );
    }

    /// Fill the renderer-owned globals fields: each slot's atlas offset + dims,
    /// the uploaded volume count (`params.w`), and the tight content box — mapped
    /// from voxel coordinates into the caller's (camera-relative) world box, so
    /// it stays consistent under the floating origin. Callers only provide world
    /// data.
    fn patch_globals(&self, globals: &mut RaymarchGlobals) {
        for (i, s) in self.slots.iter().enumerate().take(MAX_VOLUMES) {
            let (off, dims) = (s.origin, s.dims);
            globals.vol_atlas[i] = [off[0] as f32, off[1] as f32, off[2] as f32, 0.0];
            globals.vol_dims[i] = [dims[0] as f32, dims[1] as f32, dims[2] as f32, 0.0];
            for (a, &dim) in dims.iter().enumerate() {
                let (c, h) = (globals.vol_center[i][a], globals.vol_half[i][a]);
                let scale = 2.0 * h / dim.max(1) as f32; // world units per voxel
                let lo = c - h + s.tight_min[a] * scale;
                let hi = c - h + s.tight_max[a] * scale;
                globals.vol_tight_c[i][a] = 0.5 * (lo + hi);
                globals.vol_tight_h[i][a] = 0.5 * (hi - lo);
            }
        }
        globals.params[3] = self.slots.len() as f32;
    }

    /// Clear `color`/`depth` and draw the SDF matter into them (with true depth) —
    /// the unprimed path (offscreen previews, probes, standalone draws).
    pub fn draw_into(
        &self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: RaymarchGlobals,
    ) {
        self.draw_scene_pass(gpu, color, depth, globals, false);
    }

    /// Like [`draw_into`](Self::draw_into) but marching against the depth prepass
    /// set via [`set_depth_prime`](Self::set_depth_prime): the depth buffer is
    /// LOADED (the prepass copy already primed + cleared it) and each ray stops
    /// at the nearest opaque mesh. Falls back to the unprimed draw when no prime
    /// is bound.
    pub fn draw_into_primed(
        &self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: RaymarchGlobals,
    ) {
        self.draw_scene_pass(gpu, color, depth, globals, self.bind_primed.is_some());
    }

    fn draw_scene_pass(
        &self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: RaymarchGlobals,
        primed: bool,
    ) {
        self.upload_globals(gpu, globals);

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
                        // Primed: the depth prepass already cleared + filled this
                        // buffer with the opaque mesh depths — keep them so field
                        // hits behind meshes (and sky under them) depth-reject.
                        load: if primed { wgpu::LoadOp::Load } else { wgpu::LoadOp::Clear(1.0) },
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(
                0,
                if primed { self.bind_primed.as_ref().unwrap_or(&self.bind) } else { &self.bind },
                &[],
            );
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }

    /// Render the SDF matter's silhouette as 1.0 into a single-channel mask (clearing
    /// it first) — the selection-outline source for the blob.
    pub fn draw_mask(&self, gpu: &Gpu, mask: &wgpu::TextureView, globals: RaymarchGlobals) {
        self.upload_globals(gpu, globals);

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

/// The bind group layout for the SHARED distance field (uniform globals +
/// distance atlas + sampler) — what `field.wgsl` declares. Created identically by
/// the raymarch pass (which owns the resources) and the raster pipeline (which
/// binds them at group(2)); wgpu deduplicates structurally-equal layouts, so the
/// two are compatible.
pub(crate) fn field_bind_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("sdf-field"),
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
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

pub(crate) fn make_field_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    dist: &wgpu::Texture,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    let dist_view = dist.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("sdf-field"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&dist_view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
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
    tile_sampler_nearest: &wgpu::Sampler,
    sky: &wgpu::Texture,
    prime: &wgpu::TextureView,
) -> wgpu::BindGroup {
    let dist_view = dist.create_view(&wgpu::TextureViewDescriptor::default());
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let terrain_view = terrain.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let sky_view = sky.create_view(&wgpu::TextureViewDescriptor::default());
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
            wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::Sampler(tile_sampler_nearest) },
            wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&sky_view) },
            wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(prime) },
        ],
    })
}

/// The 1x1 R32Float "no mesh anywhere" depth-prime fallback, holding 1.0. Any
/// unfilterable-float 2D texture satisfies the binding; the shader gates the cap
/// on the texture being larger than 1x1. (The view keeps its texture alive.)
fn make_prime_fallback(gpu: &Gpu) -> wgpu::TextureView {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("raymarch-prime-fallback"),
        size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
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
        bytemuck::bytes_of(&1.0f32),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: Some(1) },
        wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
    );
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

/// Upload an equirectangular sky texture (RGBA8 sRGB), or a 1×1 white texture when
/// `tex` is `None` (the default / solid-color case).
fn make_sky_texture(gpu: &Gpu, tex: Option<&TextureData>) -> wgpu::Texture {
    let (w, h, pixels): (u32, u32, std::borrow::Cow<[u8]>) = match tex {
        Some(t) if t.width >= 1 && t.height >= 1 => {
            (t.width, t.height, std::borrow::Cow::Borrowed(t.pixels.as_slice()))
        }
        _ => (1, 1, std::borrow::Cow::Owned(vec![255u8, 255, 255, 255])),
    };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("skybox"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    texture
}

/// Create the terrain palette as a `TERRAIN_SLOTS`-layer 256² sRGB array. Provided
/// layers are uploaded (caller pre-resizes to 256²); the rest default to white.
pub(crate) fn make_terrain_array(gpu: &Gpu, layers: &[TextureData]) -> wgpu::Texture {
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

/// Allocate the distance (R16Float) + color (Rgba8Unorm) 3D atlas textures.
fn alloc_volume_textures(gpu: &Gpu, dims: [u32; 3]) -> (wgpu::Texture, wgpu::Texture) {
    let size = wgpu::Extent3d { width: dims[0], height: dims[1], depth_or_array_layers: dims[2] };
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
    (dist, color)
}

/// Write a bake's distance + color into the atlas textures at voxel `origin`.
/// This is also the cheap per-edit path — no allocation, no bind-group rebuild.
fn write_volume_data(
    gpu: &Gpu,
    dist: &wgpu::Texture,
    color: &wgpu::Texture,
    baked: &BakedSdf,
    origin: [u32; 3],
) {
    let [w, h, d] = baked.dims;
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: d };
    let origin = wgpu::Origin3d { x: origin[0], y: origin[1], z: origin[2] };
    let dist_f16: Vec<u16> = baked.distance.iter().map(|&v| f32_to_f16(v)).collect();
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: dist,
            mip_level: 0,
            origin,
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
            origin,
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
