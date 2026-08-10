//! Baked global illumination on the GPU: the probe texture, and the uniforms
//! that tell a shader where the volume is.
//!
//! The arithmetic — the spherical harmonics, the grid, the leak weighting, the
//! file — lives in [`floptle_gi`], with tests. This module is only the part that
//! has to talk to wgpu: turn a [`BakedGi`] into a 3D texture, and turn a volume
//! into the four uniform lanes `field.wgsl` reads.
//!
//! The texture is `Rgba32Float` and read with `textureLoad` alone. Float32 is
//! not filterable on every backend, and that costs nothing here: `gi_bounce`
//! computes its own eight-probe weights (trilinear × facing × validity), so
//! there is no hardware filtering to give up. Full float also means the values
//! that come back are exactly the values that were baked, which matters when the
//! thing you are debugging is "is this probe dark, or is it zero".

use floptle_gi::BakedGi;

use crate::device::Gpu;

/// The GI probe texture and the numbers a shader needs to find its way into it.
pub struct GiVolume {
    pub(crate) tex: wgpu::Texture,
    /// `gi_meta`, `gi_dims`, `gi_center` (WORLD, made camera-relative at draw
    /// time) and `gi_half`, ready to copy into `RaymarchGlobals`.
    pub meta: [f32; 4],
    pub dims: [f32; 4],
    pub center: [f32; 3],
    pub half: [f32; 4],
}

impl GiVolume {
    /// The 4×1×1 all-zero texture every scene starts with.
    ///
    /// A binding cannot be empty, and "no volume" has to be a *texture* rather
    /// than a branch in the pipeline layout, or every scene without baked GI
    /// would need its own pipeline. The zeroes are never read — `gi_meta.x` is
    /// 0, so `gi_bounce` returns before touching it — but they are zeroes rather
    /// than uninitialised memory on purpose, so a bug that does read them reads
    /// black instead of noise.
    pub fn empty(gpu: &Gpu) -> GiVolume {
        let tex = alloc(gpu, [4, 1, 1]);
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&[[0.0f32; 4]; 4]),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * 16),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d { width: 4, height: 1, depth_or_array_layers: 1 },
        );
        GiVolume {
            tex,
            meta: [0.0; 4],
            dims: [1.0, 1.0, 1.0, 0.0],
            center: [0.0; 3],
            half: [1.0; 4],
        }
    }

    /// Upload a bake. `center` is the volume's WORLD position (the shader gets a
    /// camera-relative copy at draw time), `leak` and `intensity` are the node's
    /// knobs — both applied here, on the CPU, so that turning either changes an
    /// upload rather than a bake, and costs a shading point nothing per pixel.
    pub fn upload(
        gpu: &Gpu,
        baked: &BakedGi,
        center: [f32; 3],
        leak: f32,
        intensity: f32,
        show_only: bool,
        normal_bias: f32,
    ) -> GiVolume {
        let grid = baked.grid;
        let [w, h, d] = grid.dims;
        let tex = alloc(gpu, [w * 4, h, d]);
        let texels = baked.texels(leak, intensity);
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4 * 16),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w * 4, height: h, depth_or_array_layers: d },
        );
        let sp = grid.spacing();
        let min_sp = sp[0].min(sp[1]).min(sp[2]).max(1e-4);
        GiVolume {
            tex,
            meta: [1.0, if show_only { 1.0 } else { 0.0 }, normal_bias, min_sp],
            dims: [w as f32, h as f32, d as f32, 0.0],
            center,
            half: [grid.half_extent[0], grid.half_extent[1], grid.half_extent[2], 0.0],
        }
    }

    /// Stamp this volume into a frame's globals, with the centre moved into the
    /// camera-relative space everything else in the field lives in (ADR-0015).
    pub fn apply(&self, g: &mut crate::RaymarchGlobals, cam_world: [f64; 3]) {
        g.gi_meta = self.meta;
        g.gi_dims = self.dims;
        g.gi_center = [
            (self.center[0] as f64 - cam_world[0]) as f32,
            (self.center[1] as f64 - cam_world[1]) as f32,
            (self.center[2] as f64 - cam_world[2]) as f32,
            0.0,
        ];
        g.gi_half = self.half;
    }

    /// Whether this volume actually lights anything.
    pub fn is_active(&self) -> bool {
        self.meta[0] > 0.5
    }
}

fn alloc(gpu: &Gpu, dims: [u32; 3]) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("gi-probes"),
        size: wgpu::Extent3d {
            width: dims[0].max(1),
            height: dims[1].max(1),
            depth_or_array_layers: dims[2].max(1),
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// The bind-group-layout entry a GI probe texture occupies, built ONCE and used
/// by both the raymarch pass's own group and the shared field group.
///
/// Float32 textures are not filterable, so this entry must say so — and it must
/// say so identically on both sides, which is the whole reason it is a function.
/// A hand-copied second entry is how two structurally-equal layouts stop being
/// equal (`floptle/0113`, and again in v0.44's post chain).
pub(crate) fn probe_tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D3,
            multisampled: false,
        },
        count: None,
    }
}
