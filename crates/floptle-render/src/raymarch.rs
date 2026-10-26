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
    /// w = present (1.0/0.0). Every terrain volume renders at its own native
    /// resolution — no shared combined grid (multi-volume terrain).
    pub vol_center: [[f32; 4]; MAX_VOLUMES],
    /// Per volume: xyz half-extent, w = volume↔volume fuse blend radius k.
    pub vol_half: [[f32; 4]; MAX_VOLUMES],
    /// Per volume: xyz voxel offset inside the shared 3D atlas (renderer-patched
    /// at draw time from the uploaded layout — callers leave this zeroed).
    pub vol_atlas: [[f32; 4]; MAX_VOLUMES],
    /// Per volume: xyz voxel dimensions (renderer-patched at draw time).
    pub vol_dims: [[f32; 4]; MAX_VOLUMES],
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
    /// shader; the sky is at infinity), z = an orthographic camera's sky spread
    /// (`Projection::sky_spread`; 0 = perspective, the view's own rays), w unused.
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
    /// x = Bayer-dither the penumbra (0/1), y = most steps a sun-shadow ray
    /// takes (0 reads as 64); zw reserved.
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
    /// Depth fog: rgb = color (w unused). Appended at the end to stay byte-identical
    /// to `struct Globals` in field.wgsl (which this uniform feeds).
    pub fog_color: [f32; 4],
    /// Depth fog: x = start dist, y = end dist, z = enabled (0/1), w unused.
    pub fog_params: [f32; 4],
    /// Per volume: xyz = camera-relative center of the tight CONTENT box — the
    /// sub-box of the brick that actually holds surface, measured from the baked
    /// voxels at upload (renderer-patched at draw time; callers leave the default).
    /// A generous terrain box is mostly empty air above the hills; bounding the
    /// marches with the content box instead of the brick is what keeps a camera
    /// standing inside the box from paying for all that air.
    pub vol_tight_c: [[f32; 4]; MAX_VOLUMES],
    /// Per volume: xyz = the tight content box's half-extent (renderer-patched).
    pub vol_tight_h: [[f32; 4]; MAX_VOLUMES],
    /// Field Shapes (the SDF stage): x = active count (0..=4).
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
    /// S8 atmospheres: x = active body count (0..=4). Up to four bodies get
    /// full shell scattering — visible from inside (tinted sky) and from
    /// SPACE (limb halo, aerial haze + clouds over the disc).
    pub atmo_meta: [f32; 4],
    /// Per body: rgb = sky color, w = density (0 = none).
    pub atmo_color: [[f32; 4]; 4],
    /// Per body: xyz = CAMERA-RELATIVE center, w = surface radius.
    pub atmo_body: [[f32; 4]; 4],
    /// Per body: x = shell height above the surface, y = cloud coverage 0..1,
    /// z = the optical depth of the air straight overhead at the camera (0 at
    /// and above the top of the shell) — what the sky seen from inside is
    /// drawn from. Worked out on the CPU in f64 so it is exact far from the
    /// origin.
    pub atmo_params: [[f32; 4]; 4],
    /// Stars mode (Lighting.stars): x = active star count. 0 = the legacy
    /// single key light in `light_dir`/`light_color` (directional or
    /// positional via light_dir.w).
    pub star_meta: [f32; 4],
    /// Per star: xyz = CAMERA-RELATIVE position, w unused.
    pub star_pos: [[f32; 4]; 4],
    /// Per star: rgb = light color, w = K where irradiance = K / d²
    /// (K = luminosity × 1e6; capped in-shader near the star).
    pub star_color: [[f32; 4]; 4],
    /// Volumetric fog (Lighting.fog_volumetric): x = density/unit, y = layer top
    /// (world y), z = top falloff, w = noise amount 0..1. Appended at the end to
    /// keep the WGSL `Globals` byte-identical.
    pub vol_fog_a: [f32; 4],
    /// Volumetric fog: x = noise scale (world units), y = time (s, drifts the
    /// noise), z = camera world y, w = enabled (0/1).
    pub vol_fog_b: [f32; 4],
    /// Baked GI (`Matter::LightProbes`): x = on (0/1), y = leak rejection
    /// (multiples of the probe spacing, 0 = off), z = normal bias (ditto),
    /// w = the smallest probe spacing in world units — the unit both of those
    /// are measured in, resolved once here so the shader never has to derive it
    /// from the grid.
    ///
    /// Intensity is not here: it is baked into the probe texels on upload, so a
    /// shading point pays for it zero times per pixel instead of once.
    pub gi_meta: [f32; 4],
    /// Baked GI: xyz = the probe lattice's counts, w = unused.
    pub gi_dims: [f32; 4],
    /// Baked GI: xyz = the volume's CAMERA-RELATIVE center, w unused.
    pub gi_center: [f32; 4],
    /// Baked GI: xyz = the volume's half-extent (world units), w unused.
    pub gi_half: [f32; 4],
    /// Volumetric light injection: x = amount (0 = the flat fog colour, the
    /// pre-injection look, reached exactly and not approximately), y = phase
    /// anisotropy g (+ = forward-scattering, the bloom around the sun), z = march
    /// steps, w = march the sun shadow at every step (0/1) — the shafts, and
    /// essentially the whole cost of the effect.
    pub vol_fog_c: [f32; 4],
    /// Each point light's EMITTER: `[kind, a, b, flags]` — 0 point, 1 sphere
    /// (a = radius), 2 rect (a/b = half width/height, flag 1 = two-sided),
    /// 3 disk (a = radius, flag 1 = two-sided), 4 tube (a = half length,
    /// b = radius). Zero is a point, which is the light every scene already had.
    pub point_shape: [[f32; 4]; 16],
    /// Each point light's world orientation (xyzw quaternion).
    pub point_rot: [[f32; 4]; 16],
    /// Contact shadows: x = on (0/1), y = how far the screen-space ray reaches
    /// in world units, z = steps, w = strength. The occluder-thickness window
    /// and the start bias are derived from the reach rather than exposed —
    /// there is one number here somebody would ever want to drag.
    pub contact: [f32; 4],
    /// Screen-space reflections: the matrix that maps a point in **this**
    /// frame's camera-relative space into the stored scene-colour picture's clip
    /// space. Built by [`SceneHistory::prev_view_proj`](crate::ssr::SceneHistory::prev_view_proj),
    /// which folds in the camera's movement — the world is camera-relative, so
    /// the previous view-projection alone would answer a question about the
    /// origin rather than about the scene.
    pub ssr_prev_vp: [[f32; 4]; 4],
    /// x = on (0/1, and 0 whenever there is no stored picture — the first frame
    /// after a load, a resize or a camera cut), y = how far a reflected ray
    /// reaches in world units, z = march steps, w = the thickness a surface in
    /// the depth buffer is assumed to have, which is what separates "the ray hit
    /// this" from "the ray passed behind it".
    pub ssr: [f32; 4],
    /// Local (point-light) shadows: x = march steps, y = how dark a shadowed
    /// pixel gets. Whether an individual lamp casts is a flag in its own
    /// `point_shape` lane; these are the scene-wide quality knobs, so cost is
    /// tuned in one place instead of on every lamp in the level.
    pub point_steps: [f32; 4],
    /// Reflection probes. x = how many are live; the rest is spare.
    pub probe_meta: [f32; 4],
    /// Per probe: xyz = the capture point, CAMERA-RELATIVE like everything else
    /// in this struct, w = how much of it to apply.
    pub probe_pos: [[f32; 4]; crate::reflect::MAX_PROBES],
    /// Per probe: xyz = the box half-extents that make the reflection land on
    /// the walls and mark which surfaces the probe covers, w = how far outside
    /// that box its influence fades before the sky takes over.
    pub probe_half: [[f32; 4]; crate::reflect::MAX_PROBES],
    /// Each point light's CONE, when it is aimed. Appended at the end so this
    /// struct stays byte-identical to the WGSL one.
    ///
    /// `x` = cosine of the half angle where the light reaches zero, `y` =
    /// cosine of the half angle where it is still at full brightness. **`x = -1`
    /// means no cone**, which is every light that was ever authored before spots
    /// existed and is the value the default fills.
    ///
    /// Cosines rather than angles because the shader compares against
    /// `dot(spot_dir, -l)` and would otherwise pay an `acos` per lit pixel per
    /// light. The conversion is done once, where the light is packed.
    pub point_cone: [[f32; 4]; 16],
    /// Depth fog, the lanes that did not fit in `fog_color`/`fog_params`:
    /// **x = how much of the flat ramp the SKY also takes at the horizon**
    /// (0..1), yzw spare. Appended at the end so this struct stays
    /// byte-identical to the WGSL one.
    ///
    /// Zero is fog that stops at the geometry — surfaces tinted, background
    /// untouched — and it is worth being able to ask for, but it is not the
    /// default: see `fog_sky` in `field.wgsl` for why a
    /// ramp that stops at the geometry makes a dark fog colour unusable.
    pub fog_extra: [f32; 4],
    /// **Per-slot terrain texture scale**, [`TERRAIN_SLOTS`] of them packed four
    /// to a `vec4` — a multiplier on the palette's base triplanar scale.
    ///
    /// One scale for the whole palette meant matching a fine gravel to a broad
    /// rock face was a job for an image editor. `0.0` reads as `1.0` in the
    /// shader, so a globals block built from [`Default`] tiles exactly as it
    /// always did. Appended at the end so this struct stays byte-identical to
    /// the WGSL one.
    pub terrain_scale: [[f32; 4]; 8],
    /// The body fog is measured from (`Lighting.fog_follows_body`): xyz = its
    /// CAMERA-RELATIVE centre, w = its surface radius. `w <= 0` is a flat world,
    /// where fog heights are world y. Appended at the end so this struct stays
    /// byte-identical to the WGSL one.
    pub fog_body: [f32; 4],
    /// Each atmosphere's cloud deck, in `atmo_*` order: rgb = its daylight
    /// colour (white = as before), w = darkness 0..1. Appended at the end so
    /// this struct stays byte-identical to the WGSL one.
    pub atmo_cloud: [[f32; 4]; 4],
    /// Each atmosphere's star aureole: x = spread, y = strength, multiples of
    /// natural (1, 1 = as before). Appended at the end, like everything above.
    pub atmo_sun: [[f32; 4]; 4],
    /// x = 1 when this view's volumetric fog was marched at half resolution
    /// ([`Raymarch::march_fog`]) and every surface and the sky read it, 0 and
    /// each fragment marches its own; y, z = the half-resolution size, w = the
    /// row stride of the texels in the buffer. Stamped by the renderer, never
    /// by the caller. Appended at the end, like everything above.
    pub fog_cache: [f32; 4],
}

impl RaymarchGlobals {
    /// Whether these globals draw the volumetric fog (the fog on, in its
    /// volumetric mode): what [`Raymarch::march_fog`] is for.
    pub fn volumetric_fog(&self) -> bool {
        self.fog_params[2] > 0.5 && self.vol_fog_b[3] > 0.5
    }
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
            vol_center: [[0.0; 4]; MAX_VOLUMES],
            vol_half: [[1.0, 1.0, 1.0, 0.5]; MAX_VOLUMES],
            vol_atlas: [[0.0; 4]; MAX_VOLUMES],
            vol_dims: [[1.0, 1.0, 1.0, 0.0]; MAX_VOLUMES],
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
            vol_tight_c: [[0.0; 4]; MAX_VOLUMES],
            vol_tight_h: [[1e8, 1e8, 1e8, 0.0]; MAX_VOLUMES],
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
            atmo_meta: [0.0; 4],
            atmo_color: [[0.0; 4]; 4],
            atmo_body: [[0.0, 0.0, 0.0, 1.0]; 4],
            atmo_params: [[0.0; 4]; 4],
            star_meta: [0.0; 4],
            star_pos: [[0.0; 4]; 4],
            star_color: [[0.0; 4]; 4],
            vol_fog_a: [0.0; 4],
            vol_fog_b: [0.0; 4],
            gi_meta: [0.0; 4],
            gi_dims: [0.0; 4],
            gi_center: [0.0; 4],
            gi_half: [0.0; 4],
            vol_fog_c: [0.0, 0.0, 16.0, 0.0],
            point_shape: [[0.0; 4]; 16],
            point_rot: [[0.0, 0.0, 0.0, 1.0]; 16],
            contact: [0.0, 0.3, 12.0, 1.0],
            ssr_prev_vp: [[0.0; 4]; 4],
            // Off, and off is what a caller that never heard of this gets. The
            // reach is metres of room rather than a scene-scale guess: past that
            // the march's fixed step count spreads too thin to find anything and
            // starts reporting the gaps between its own samples as sky.
            ssr: [0.0, 30.0, 32.0, 0.5],
            point_steps: [16.0, 1.0, 0.0, 0.0],
            probe_meta: [0.0; 4],
            // -1 is "no cone": every direction is inside it.
            point_cone: [[-1.0, -1.0, 0.0, 0.0]; 16],
            fog_extra: [0.0; 4],
            terrain_scale: [[1.0; 4]; 8],
            fog_body: [0.0; 4],
            atmo_cloud: [[1.0, 1.0, 1.0, 0.0]; 4],
            atmo_sun: [[1.0, 1.0, 0.0, 0.0]; 4],
            fog_cache: [0.0; 4],
            probe_pos: [[0.0; 4]; crate::reflect::MAX_PROBES],
            probe_half: [[1.0; 4]; crate::reflect::MAX_PROBES],
        }
    }
}

/// Max Field Shapes (authored SDF shaders) folded into the field per scene.
pub const MAX_FIELD_SHAPES: usize = 4;

/// Max blobs the raymarch shader folds together in one pass.
pub const MAX_BLOBS: usize = 16;

/// Max baked volumes (terrains / mesh bakes) folded together in one pass. Each keeps
/// its native voxel resolution inside a shared 3D atlas.
///
/// These are *instances*: several can draw from one baked region of the atlas
/// (seven copies of one building are one bake), so this bounds what the
/// shader walks per step, not what the atlas holds. A caller shares a region
/// by writing `region + 1` into an instance's `vol_atlas[i].w`; zero means the
/// instance's own slot, in upload order.
pub const MAX_VOLUMES: usize = 32;

/// Max distinct bakes the atlas holds at once. Each costs VRAM for its voxels
/// (6 bytes each: a 16-bit distance and an RGBA8 colour — about 12 MB for a
/// 128³ brick), and the stack is bounded by the device's 3D texture limit too.
pub const MAX_ATLAS_VOLUMES: usize = 64;

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
    /// What the caller said this slot holds (see [`AtlasVolume::key`]); 0 is
    /// "unnamed", which matches nothing.
    key: u64,
}

/// One volume for [`Raymarch::set_volumes_keyed`]: its bake, and a key that
/// names that bake's content. The same key promises the same voxels, so a
/// volume whose key is already in the atlas is copied on the GPU, never
/// converted and uploaded again. A key is never 0.
#[derive(Clone, Copy)]
pub struct AtlasVolume<'a> {
    pub baked: &'a BakedSdf,
    pub key: u64,
}

/// A slot of an atlas being built: where it goes, what it holds, and how many
/// of its Z layers are written.
struct BuildSlot {
    origin: [u32; 3],
    dims: [u32; 3],
    key: u64,
    done_z: u32,
    /// The content box of the layers written so far.
    tight: Option<([f32; 3], [f32; 3])>,
}

/// A slot table made over frames, drawn in place of the current one once
/// every slot is written. Until then the current one is drawn as it was.
///
/// Its new volumes go where nothing drawn reads: free Z ranges of the drawn
/// atlas when they fit (`textures` is `None`), else a new, larger atlas that
/// replaces the drawn one when whole.
struct AtlasBuild {
    textures: Option<(wgpu::Texture, wgpu::Texture)>,
    slots: Vec<BuildSlot>,
    refused: Option<AtlasRefused>,
}

/// An atlas's distance and colour textures.
type AtlasTextures = (wgpu::Texture, wgpu::Texture);

/// What [`Raymarch::grown`] has for a build that needs a larger atlas. (A
/// browser makes it on the frame, so there it is only ever `Failed`.)
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
enum Grown {
    /// Made, holding the first this many of the volumes asked for: all of
    /// them unless memory was short.
    Made(AtlasTextures, usize),
    Waiting,
    /// Not even one of the volumes fits in what memory there is.
    Refused,
    /// The worker could not run: the frame makes it.
    Failed,
}

/// What the atlas worker was asked for: the roomy size, and each volume's
/// dims in order, from which it works out the exact size and the smaller
/// ones it falls back to.
#[derive(Clone, PartialEq, Debug)]
struct GrowAsk {
    roomy: [u32; 3],
    plan: Vec<[u32; 3]>,
}

/// What the atlas worker made: the textures, their dims, and how many of the
/// volumes asked for they hold. `None` when not even one fit.
type GrowMade = Option<(AtlasTextures, [u32; 3], usize)>;

/// The atlas the first `n` volumes of `plan` need: stacked along Z.
fn stacked_dims(plan: &[[u32; 3]], n: usize) -> [u32; 3] {
    let mut dims = [1u32, 1, 0];
    for d in &plan[..n.min(plan.len())] {
        dims = [dims[0].max(d[0]), dims[1].max(d[1]), dims[2] + d[2]];
    }
    dims[2] = dims[2].max(1);
    dims
}

/// The atlases [`grow_atlas`] tries, in order, and how many of the volumes
/// asked for each holds: the roomy size, the exact one, then the first half
/// of the volumes, a quarter, down to one.
#[cfg(any(not(target_arch = "wasm32"), test))]
fn grow_tries(ask: &GrowAsk) -> Vec<([u32; 3], usize)> {
    let mut tries = vec![(ask.roomy, ask.plan.len())];
    let mut n = ask.plan.len();
    while n > 0 {
        tries.push((stacked_dims(&ask.plan, n), n));
        n /= 2;
    }
    if ask.plan.is_empty() {
        tries.clear();
    }
    tries
}

/// Make the atlas `ask` wants, as the frame would have: the roomy size, or
/// short of memory for that the exact one, or failing that the first half
/// of the volumes, then a quarter, down to none. Each one made is zeroed
/// here: wgpu zeroes a new texture on the first submit that writes part of
/// it, which is otherwise the frame's.
#[cfg(not(target_arch = "wasm32"))]
fn grow_atlas(device: &wgpu::Device, queue: &wgpu::Queue, ask: &GrowAsk, cap: Option<u64>) -> GrowMade {
    for (dims, n) in grow_tries(ask) {
        let Some((d, c)) = try_alloc_volume_textures(device, dims, cap) else { continue };
        for (t, texel) in [(&d, &[0u8; 2][..]), (&c, &[0u8; 4][..])] {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: t, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                texel,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: None, rows_per_image: None },
                wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            );
        }
        queue.submit(None);
        return Some(((d, c), dims, n));
    }
    None
}

impl AtlasBuild {
    fn plan_matches(&self, plan: &[([u32; 3], u64)]) -> bool {
        self.slots.len() == plan.len() && self.slots.iter().zip(plan).all(|(s, &(d, k))| s.dims == d && s.key == k)
    }
}

fn merge_box(a: Option<([f32; 3], [f32; 3])>, b: Option<([f32; 3], [f32; 3])>) -> Option<([f32; 3], [f32; 3])> {
    match (a, b) {
        (Some((al, ah)), Some((bl, bh))) => Some((
            [al[0].min(bl[0]), al[1].min(bl[1]), al[2].min(bl[2])],
            [ah[0].max(bh[0]), ah[1].max(bh[1]), ah[2].max(bh[2])],
        )),
        (x, None) | (None, x) => x,
    }
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
    /// The fog pass ([`march_fog`](Self::march_fog)): `fs_fog` into the
    /// half-resolution fog and distance targets.
    fog_pipeline: wgpu::RenderPipeline,
    /// The half-resolution fog targets, one per view size (a frame draws the
    /// window, a docked Game panel and render targets at different sizes),
    /// and which of them every bind group carries now.
    fog_slots: Vec<FogSlot>,
    fog_active: Option<usize>,
    /// Whether the bound fog targets hold this view's fog: set by
    /// [`march_fog`](Self::march_fog), cleared whenever a view's frame
    /// targets are bound, and stamped into the globals as `fog_cache.x`.
    fog_ready: std::sync::atomic::AtomicBool,
    /// What the fog bindings hold while no fog pass has run: one texel.
    fog_fallback: wgpu::Buffer,
    /// Whether [`march_fog`](Self::march_fog) runs at all (on unless a test
    /// or a probe compares against every fragment marching its own).
    fog_pass_on: bool,
    /// Draw every fragment that reads the fog pass magenta.
    fog_debug: bool,
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
    /// The atlas size the GPU last refused for lack of memory, in bytes, and
    /// how many volumes were dropped to fit. `None` once one fits again.
    atlas_refused: Option<AtlasRefused>,
    /// The atlas [`set_volumes_keyed`](Self::set_volumes_keyed) is making
    /// over frames, drawn in place of this one once it is whole.
    building: Option<AtlasBuild>,
    /// A larger atlas the driver is making on a worker: handing over one of
    /// hundreds of megabytes takes it milliseconds, which the frame must not
    /// wait for. What it was asked for, and where it arrives.
    growing: Option<(GrowAsk, std::sync::mpsc::Receiver<GrowMade>)>,
    /// Atlases made on the frame's own thread since the renderer was made:
    /// none natively, where the worker makes every one (a browser makes
    /// them on the frame). Each is an allocation of up to hundreds of
    /// megabytes and its zeroing, waited for by the frame.
    frame_allocs: u64,
    /// Refuse an atlas larger than this many bytes, as a GPU out of memory
    /// would: for tests.
    alloc_cap: Option<u64>,
    /// Volume cells written from the CPU since the renderer was made (each
    /// one 6 bytes: an f16 distance and an RGBA8 colour). GPU-side copies are
    /// not counted.
    uploaded_cells: u64,
    terrain_tex: wgpu::Texture,
    /// Equirectangular sky texture (1×1 white until a skybox texture is set).
    sky_tex: wgpu::Texture,
    bind: wgpu::BindGroup,
    /// The shared field bind group (globals uniform + distance atlas + sampler)
    /// the raster pass binds at group(2), so mesh fragments march the same field
    /// (shadows received + true SDF AO). Rebuilt with the atlas.
    field_layout: wgpu::BindGroupLayout,
    field_bind: wgpu::BindGroup,
    /// The scene's baked GI, if any — owned here because it rides the shared
    /// field bind group, which is what lets one upload light the raymarch pass,
    /// every raster mesh and every `.flsl` material at once.
    gi: crate::gi::GiVolume,
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
    /// The scene's spliced custom code, kept so either half can change independently and
    /// the pipeline rebuild still carries both. `field_code` = Field Shapes `(field, color)`;
    /// `sky_fn` = a Sky shader's `flsl_sky`. Both splice into the one raymarch module, so a
    /// change to either re-assembles from both. `custom_support` is the shared stdlib the
    /// editor supplies (same string for both — appended once).
    field_code: Option<(String, String)>,
    sky_fn: Option<String>,
    custom_support: String,
    /// The captured sky + its roughness chain. Owned here for the same reason
    /// the baked GI is: it rides the shared field bind group, so one capture
    /// reaches every raster mesh and every `.flsl` material at once.
    env: crate::env::EnvMap,
    env_pipeline: wgpu::RenderPipeline,
    /// The scene colour history a screen-space reflection reads, and the 1×1
    /// stand-in that stands for "no history" — bound the moment nobody has
    /// called [`set_scene_history`](Self::set_scene_history), which is every
    /// offscreen preview and every probe. The fallback's SIZE is the signal, so
    /// there is no flag here that could disagree with what is actually bound.
    scene_fallback: (wgpu::TextureView, wgpu::Sampler),
    scene_view: Option<(wgpu::TextureView, wgpu::Sampler)>,
    /// The reflection probes' map, and the 1×1 stand-in for a scene with none.
    /// Same shape and same reason as `scene_fallback` above: what is bound is
    /// the only record of whether there is anything to read, so no flag can
    /// disagree with it.
    probe_fallback: (wgpu::TextureView, wgpu::Sampler),
    probe_view: Option<(wgpu::TextureView, wgpu::Sampler)>,
}

/// Layers in the terrain texture palette + the size each is stored at. 12 layers:
/// enough for a planet's biomes + strata + cave materials and a moon's, in one scene
/// palette (the palette is per-scene, shared by every terrain volume in it).
pub const TERRAIN_SLOTS: u32 = 32;
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
                // The same palette sampled nearest — `triplanar` selects per slot from
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
                // Baked GI probes — the same entry the shared field group uses.
                crate::gi::probe_tex_entry(9),
                // Binding 10: the terrain palette again, so binding 8's nearest
                // sampler has an image of its own. OpenGL's combined sampler
                // type is the reason; see `raymarch.wgsl`.
                wgpu::BindGroupLayoutEntry {
                    binding: 10,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                // The half-resolution fog (`fog_cached`).
                fog_buf_entry(11),
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raymarch"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let (pipeline, mask_pipeline, fog_pipeline) = Self::build_pipelines(gpu, &layout, &module);
        let env = crate::env::EnvMap::new(device);
        let env_pipeline = Self::build_env_pipeline(gpu, &layout, &module);

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
        // Repeating sampler for the triplanar terrain palette, so textures tile
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
        let (dist_tex, color_tex) = alloc_volume_textures(&gpu.device, [1, 1, 1]);
        write_volume_data(gpu, &dist_tex, &color_tex, &empty, [0, 0, 0]);
        let terrain_tex = make_terrain_array(gpu, &[]);
        let sky_tex = make_sky_texture(gpu, None);
        let prime_fallback = make_prime_fallback(gpu);
        // No bake yet: a 4x1x1 of zeroes, so the binding is valid and reads black.
        let gi = crate::gi::GiVolume::empty(gpu);
        let fog_fallback = fog_fallback_buffer(device);
        let bind = make_bind(
            device, &bind_layout, &globals_buf, &dist_tex, &color_tex, &sampler, &terrain_tex,
            &tile_sampler, &tile_sampler_nearest, &sky_tex, &prime_fallback, &gi.tex, &fog_fallback,
        );
        let field_layout = field_bind_layout(device);
        let scene_fallback = crate::ssr::SceneHistory::empty(device);
        let probe_fallback = crate::reflect::ReflectionProbes::empty(device);
        let field_bind = make_field_bind(
            device, &field_layout, &globals_buf, &dist_tex, &color_tex, &sampler, &gi.tex,
            &prime_fallback, env.view(), env.sampler(), &scene_fallback.0, &scene_fallback.1,
            &probe_fallback.0, &probe_fallback.1, &fog_fallback,
        );

        Self {
            pipeline,
            mask_pipeline,
            fog_pipeline,
            fog_slots: Vec::new(),
            fog_active: None,
            fog_ready: std::sync::atomic::AtomicBool::new(false),
            fog_fallback,
            fog_pass_on: true,
            fog_debug: false,
            pipeline_layout: layout,
            globals_buf,
            bind_layout,
            sampler,
            tile_sampler,
            tile_sampler_nearest,
            _dist_tex: dist_tex,
            _color_tex: color_tex,
            slots: Vec::new(),
            atlas_refused: None,
            building: None,
            growing: None,
            frame_allocs: 0,
            alloc_cap: None,
            uploaded_cells: 0,
            terrain_tex,
            sky_tex,
            bind,
            field_layout,
            field_bind,
            gi,
            prime_fallback,
            prime_view: None,
            bind_primed: None,
            field_code: None,
            sky_fn: None,
            custom_support: String::new(),
            env,
            env_pipeline,
            scene_fallback,
            scene_view: None,
            probe_fallback,
            probe_view: None,
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
        let base = prelude();
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
        let (pipeline, mask_pipeline, fog_pipeline) = Self::build_pipelines(gpu, &self.pipeline_layout, &module);
        self.pipeline = pipeline;
        self.mask_pipeline = mask_pipeline;
        self.fog_pipeline = fog_pipeline;
        // …and the capture, from the same module: a spliced Sky shader that did
        // not reach the environment map would leave every reflection showing
        // the previous sky.
        self.env_pipeline = Self::build_env_pipeline(gpu, &self.pipeline_layout, &module);
    }

    /// The sky-capture pipeline: the same module and the same bind group as the
    /// march, drawing `fs_env` into the environment map's top level. Built here
    /// so a spliced Sky shader rebuilds the capture along with everything else —
    /// a reflection of last week's sky would be a very quiet bug.
    fn build_env_pipeline(
        gpu: &Gpu,
        layout: &wgpu::PipelineLayout,
        module: &wgpu::ShaderModule,
    ) -> wgpu::RenderPipeline {
        gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("env-capture"),
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
                entry_point: Some("fs_env"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: crate::env::ENV_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: gpu.pipeline_cache.as_ref(),
        })
    }

    fn build_pipelines(
        gpu: &Gpu,
        layout: &wgpu::PipelineLayout,
        module: &wgpu::ShaderModule,
    ) -> (wgpu::RenderPipeline, wgpu::RenderPipeline, wgpu::RenderPipeline) {
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
                    format: gpu.scene_format(),
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: gpu.pipeline_cache.as_ref(),
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
            cache: gpu.pipeline_cache.as_ref(),
        });
        // The fog pass: no depth, two targets (the fog, the distance marched).
        let fog_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raymarch-fog"),
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
                entry_point: Some("fs_fog"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[
                    Some(wgpu::ColorTargetState { format: FOG_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL }),
                    Some(wgpu::ColorTargetState { format: FOG_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL }),
                ],
            }),
            multiview_mask: None,
            cache: gpu.pipeline_cache.as_ref(),
        });
        (pipeline, mask_pipeline, fog_pipeline)
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
    /// build — for the editor to naga-validate before swapping pipelines.
    pub fn preview_custom_source(code: Option<(&str, &str, &str)>) -> String {
        Self::assembled_source(code.map(|(f, c, _)| (f, c)), None, code.map_or("", |(_, _, s)| s))
    }

    /// The source a Sky shader would build — for naga-validation before swapping pipelines.
    pub fn preview_sky_source(sky_fn: &str, support: &str) -> String {
        Self::assembled_source(None, Some(sky_fn), support)
    }

    /// Rebuild `bind` (fallback prime), `bind_primed` when primed, and the shared
    /// field bind — after any bound resource (atlas, palette, sky, prime, GI)
    /// changes.
    ///
    /// The field bind is rebuilt here rather than at each caller because it now
    /// carries the depth prepass, which changes on window resize through a path
    /// (`set_depth_prime`) that has nothing to do with the atlas or the probes.
    /// Three callers each remembering to rebuild it is the kind of rule that
    /// holds until the fourth one.
    fn rebuild_binds(&mut self, device: &wgpu::Device) {
        let fog = self.fog_active.and_then(|i| self.fog_slots.get(i)).map_or(&self.fog_fallback, |f| &f.buf);
        self.field_bind = make_field_bind(
            device,
            &self.field_layout,
            &self.globals_buf,
            &self._dist_tex,
            &self._color_tex,
            &self.sampler,
            &self.gi.tex,
            self.prime_view.as_ref().unwrap_or(&self.prime_fallback),
            self.env.view(),
            self.env.sampler(),
            self.scene_view.as_ref().map_or(&self.scene_fallback.0, |s| &s.0),
            self.scene_view.as_ref().map_or(&self.scene_fallback.1, |s| &s.1),
            self.probe_view.as_ref().map_or(&self.probe_fallback.0, |s| &s.0),
            self.probe_view.as_ref().map_or(&self.probe_fallback.1, |s| &s.1),
            fog,
        );
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
            &self.gi.tex,
            fog,
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
                &self.gi.tex,
                fog,
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

    /// Capture the scene's sky into the environment map and build its roughness
    /// chain, so surfaces have something to reflect this frame.
    ///
    /// Call it after [`upload_globals`](Self::upload_globals) and before the
    /// raster pass. One 256×128 draw plus eight smaller ones, so it runs every
    /// frame — skies animate. The map is bound into the shared field group at a
    /// fixed size, so only its contents change and nothing is rebound.
    pub fn capture_env(&self, gpu: &Gpu) {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("env-capture") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("env-capture"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.env.top(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.env_pipeline);
            // The unprimed bind: the sky does not care about the depth prepass,
            // and using it here would tie the capture to the frame's draw order.
            rp.set_bind_group(0, &self.bind, &[]);
            rp.draw(0..3, 0..1);
        }
        self.env.build_chain(&mut encoder);
        gpu.queue.submit([encoder.finish()]);
    }

    /// How many roughness levels the environment map carries — the shader needs
    /// it to map roughness onto a mip.
    pub fn env_mips(&self) -> u32 {
        self.env.mips()
    }

    /// Swap in a baked GI volume (or clear it back to none).
    ///
    /// This is an UPLOAD, not a bake: `leak`, `intensity` and the debug view are
    /// resolved into the texels on the way in, so dragging any of those sliders
    /// re-uploads a few hundred kilobytes and re-lights the scene immediately,
    /// while the expensive thing — the probes themselves — is untouched.
    pub fn set_gi(&mut self, gpu: &Gpu, volume: crate::gi::GiVolume) {
        self.gi = volume;
        self.rebuild_binds(&gpu.device);
    }

    /// The scene's baked GI volume — for stamping its uniforms into a frame's
    /// globals ([`GiVolume::apply`](crate::gi::GiVolume::apply)).
    pub fn gi(&self) -> &crate::gi::GiVolume {
        &self.gi
    }

    /// Write `globals` (atlas slots patched) without drawing — for frames where no
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
        self.fog_ready.store(false, std::sync::atomic::Ordering::Relaxed);
        self.prime_view = view.cloned();
        self.rebuild_binds(&gpu.device);
    }

    /// Bind both per-view frame targets at once: the depth prepass and the scene
    /// colour history.
    ///
    /// One call because a bind group is immutable and rebuilding it is the whole
    /// cost here — setting the two separately rebuilds twice for one frame's
    /// worth of change. A frame draws the scene from more than one view (the
    /// window, a docked Game panel, a render target), and each of those swaps
    /// both of these together.
    pub fn bind_frame_targets(
        &mut self,
        gpu: &Gpu,
        prime: Option<&wgpu::TextureView>,
        scene: Option<(&wgpu::TextureView, &wgpu::Sampler)>,
    ) {
        // A new view: whatever fog the targets hold is another view's.
        self.fog_ready.store(false, std::sync::atomic::Ordering::Relaxed);
        self.prime_view = prime.cloned();
        self.scene_view = scene.map(|(v, s)| (v.clone(), s.clone()));
        self.rebuild_binds(&gpu.device);
    }

    /// Bind (or drop, with `None`) the scene colour history that screen-space
    /// reflections read — see [`crate::ssr::SceneHistory`].
    ///
    /// Same shape and same rule as [`set_depth_prime`](Self::set_depth_prime):
    /// call it when the texture is (re)created, not per frame, because a bind
    /// group is immutable and rebuilding one every frame would allocate a bind
    /// group per frame forever. Left unset — every offscreen preview, every
    /// probe, every runtime that has not opted in — the 1×1 fallback stands and
    /// shading reflects the sky alone.
    pub fn set_scene_history(
        &mut self,
        gpu: &Gpu,
        view: Option<(&wgpu::TextureView, &wgpu::Sampler)>,
    ) {
        self.scene_view = view.map(|(v, s)| (v.clone(), s.clone()));
        self.rebuild_binds(&gpu.device);
    }

    /// Bind (or drop, with `None`) the captured reflection probes.
    ///
    /// Same rule as the depth prepass and the scene history: call it when the
    /// texture is created, not per frame — a bind group is immutable, and the
    /// probe texture is allocated once and written into thereafter, so one call
    /// covers every capture that follows. Left unset, the 1×1 stand-in holds and
    /// every surface falls back to the sky, which is what the engine did before
    /// probes existed.
    pub fn set_reflection_probes(
        &mut self,
        gpu: &Gpu,
        view: Option<(&wgpu::TextureView, &wgpu::Sampler)>,
    ) {
        self.probe_view = view.map(|(v, s)| (v.clone(), s.clone()));
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

    /// Upload a set of baked volumes into one shared 3D atlas, each at its native
    /// voxel resolution — far-apart terrains do not share a coarse combined grid.
    /// Volumes stack along the atlas Z axis; the
    /// per-slot offsets/dims are patched into the globals at draw time. Returns how
    /// many volumes were accepted (the rest exceeded the device's 3D-texture limit —
    /// callers should surface that instead of silently dropping content).
    ///
    /// Fast path: a single volume with unchanged dims rewrites the existing texture
    /// data in place (no allocation / bind-group rebuild — keeps sculpting smooth).
    pub fn set_volumes(&mut self, gpu: &Gpu, volumes: &[&BakedSdf]) -> usize {
        self.building = None;
        self.growing = None;
        let limit = gpu.device.limits().max_texture_dimension_3d;
        // Accept volumes until the Z stack or the XY footprint would exceed the limit.
        let mut accepted = Vec::new();
        let (mut aw, mut ah, mut ad) = (1u32, 1u32, 0u32);
        for &b in volumes.iter().take(MAX_ATLAS_VOLUMES) {
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
                self.uploaded_cells += b.distance.len() as u64;
                let (tight_min, tight_max) = full_content_bounds(b);
                self.slots[0].tight_min = tight_min;
                self.slots[0].tight_max = tight_max;
                self.slots[0].key = 0;
                return 1;
            }
        }

        // Out of video memory, a smaller atlas is better than none: drop
        // volumes from the end (occluders come after the terrains) until one
        // fits, down to an empty atlas. Every one dropped casts no field shadow
        // or AO; the world still draws, which a texture that failed to be made
        // would have stopped.
        let wanted = volume_bytes([aw, ah, ad]);
        let mut refused = None;
        let (dist_tex, color_tex) = loop {
            let dims = atlas_dims(&accepted);
            if let Some(t) = try_alloc_volume_textures(&gpu.device, dims, self.alloc_cap) {
                self.frame_allocs += 1;
                break t;
            }
            refused.get_or_insert(wanted);
            if accepted.is_empty() {
                // Not even one voxel: nothing to fall back to but the
                // textures we already have, which are valid.
                self.slots.clear();
                self.atlas_refused = Some(AtlasRefused { bytes: wanted, dropped: volumes.len() });
                self.rebuild_binds(&gpu.device);
                return 0;
            }
            let keep = accepted.len() / 2;
            accepted.truncate(keep);
        };
        self.atlas_refused = refused.map(|bytes| AtlasRefused { bytes, dropped: volumes.len() - accepted.len() });
        self.slots.clear();
        for (b, origin) in &accepted {
            write_volume_data(gpu, &dist_tex, &color_tex, b, *origin);
            self.uploaded_cells += b.distance.len() as u64;
            let (tight_min, tight_max) = full_content_bounds(b);
            self.slots.push(VolSlot { origin: *origin, dims: b.dims, tight_min, tight_max, key: 0 });
        }
        self._dist_tex = dist_tex;
        self._color_tex = color_tex;
        self.rebuild_binds(&gpu.device);
        accepted.len()
    }

    /// The last atlas the GPU refused for lack of memory, while the atlas is
    /// still short of what was asked: see [`set_volumes`](Self::set_volumes).
    pub fn atlas_refused(&self) -> Option<AtlasRefused> {
        self.atlas_refused
    }

    /// Upload only the sub-box `[min, max)` (voxel coords) of `baked` into atlas slot
    /// `slot` — the fast path for a brush dab, so painting/editing a huge terrain
    /// doesn't re-convert and re-upload the whole volume every frame. `baked.dims`
    /// must match the slot's dims (caller falls back to [`set_volumes`] on a resize).
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
        self.uploaded_cells += write_sub_box(gpu, &self._dist_tex, &self._color_tex, off, baked, [x0, y0, z0], [x1, y1, z1]);
    }

    /// Make the atlas hold exactly `volumes`, spending at most about
    /// `budget_cells` cells of CPU upload on this call (each cell is 6
    /// bytes), and say how many were accepted once it does.
    ///
    /// A volume whose key the drawn atlas already holds stays where it is,
    /// so a terrain arriving, leaving or being re-laid uploads only what is
    /// new. What is new is written over as many calls as the budget asks,
    /// into Z ranges of the atlas that nothing drawn reads (or, when they do
    /// not fit, into a larger atlas made beside it, the volumes it keeps
    /// copied on the GPU), while the slots drawn stay as they were: `None`
    /// means "still building, keep drawing with the slots you had", and the
    /// caller passes the same list each call until it says `Some`. A
    /// different list starts again from what is already written.
    ///
    /// Slot `i` is `volumes[i]`, as in [`set_volumes`](Self::set_volumes),
    /// whose limits this keeps.
    pub fn set_volumes_keyed(&mut self, gpu: &Gpu, volumes: &[AtlasVolume], budget_cells: u64) -> Option<usize> {
        let limit = gpu.device.limits().max_texture_dimension_3d;
        let mut plan: Vec<([u32; 3], u64)> = Vec::new();
        let (mut aw, mut ah, mut ad) = (1u32, 1u32, 0u32);
        for v in volumes.iter().take(MAX_ATLAS_VOLUMES) {
            let [w, h, d] = v.baked.dims;
            if w.max(aw) > limit || h.max(ah) > limit || ad + d > limit {
                break;
            }
            aw = aw.max(w);
            ah = ah.max(h);
            plan.push((v.baked.dims, v.key));
            ad += d;
        }
        // Already there (and anything being built for another list is moot).
        if self.slots.len() == plan.len()
            && self.slots.iter().zip(&plan).all(|(s, &(d, k))| s.dims == d && s.key == k && k != 0)
            && self.atlas_refused.is_none()
        {
            // A larger atlas still on its way is not needed any more.
            self.building = None;
            self.growing = None;
            return Some(plan.len());
        }
        // The same shapes, and what changed fits now: rewritten in place.
        if self.building.is_none()
            && self.atlas_refused.is_none()
            && self.slots.len() == plan.len()
            && self.slots.iter().zip(&plan).all(|(s, &(d, _))| s.dims == d)
        {
            let changed: u64 = self
                .slots
                .iter()
                .zip(&plan)
                .filter(|(s, (_, k))| s.key != *k || *k == 0)
                .map(|(_, (d, _))| d.iter().map(|&n| n as u64).product::<u64>())
                .sum();
            if changed <= budget_cells {
                for (i, v) in volumes.iter().take(plan.len()).enumerate() {
                    if self.slots[i].key == v.key && v.key != 0 {
                        continue;
                    }
                    write_volume_data(gpu, &self._dist_tex, &self._color_tex, v.baked, self.slots[i].origin);
                    self.uploaded_cells += v.baked.distance.len() as u64;
                    let (lo, hi) = full_content_bounds(v.baked);
                    let s = &mut self.slots[i];
                    s.tight_min = lo;
                    s.tight_max = hi;
                    s.key = v.key;
                }
                return Some(plan.len());
            }
        }
        if !self.building.as_ref().is_some_and(|b| b.plan_matches(&plan)) {
            if self.plan_build(gpu, plan, [aw, ah, ad.max(1)], volumes.len()) {
                // Waiting for a larger atlas: nothing to write into yet.
                return None;
            }
            if self.building.is_none() {
                // Refused outright: what there is cannot be trusted to match.
                self.slots.clear();
                self.rebuild_binds(&gpu.device);
                return Some(0);
            }
        }
        // Write what is left, layer by layer, while the budget lasts: at least
        // one layer a call, so a budget smaller than a layer still finishes.
        let mut left = budget_cells.min(i64::MAX as u64) as i64;
        let mut wrote = 0u64;
        let b = self.building.as_mut()?;
        let (dist, color) = b.textures.as_ref().map_or((&self._dist_tex, &self._color_tex), |(d, c)| (d, c));
        for (s, v) in b.slots.iter_mut().zip(volumes) {
            let [w, h, d] = s.dims;
            while s.done_z < d && (left > 0 || wrote == 0) {
                let layer = (w * h) as i64;
                let n = ((left.max(layer) / layer) as u32).clamp(1, d - s.done_z);
                let (z0, z1) = (s.done_z, s.done_z + n);
                write_slab(gpu, dist, color, s.origin, v.baked, z0, z1);
                s.tight = merge_box(s.tight, content_bounds(v.baked, [0, 0, z0], [w, h, z1]));
                s.done_z = z1;
                let cells = layer as u64 * n as u64;
                wrote += cells;
                left -= cells as i64;
            }
        }
        self.uploaded_cells += wrote;
        if b.slots.iter().any(|s| s.done_z < s.dims[2]) {
            return None;
        }
        let b = self.building.take()?;
        self.slots = b
            .slots
            .iter()
            .map(|s| {
                let c = [s.dims[0] as f32 * 0.5, s.dims[1] as f32 * 0.5, s.dims[2] as f32 * 0.5];
                let (tight_min, tight_max) = s.tight.unwrap_or((c, c));
                VolSlot { origin: s.origin, dims: s.dims, tight_min, tight_max, key: s.key }
            })
            .collect();
        self.atlas_refused = b.refused;
        if let Some((d, c)) = b.textures {
            self._dist_tex = d;
            self._color_tex = c;
            self.rebuild_binds(&gpu.device);
        }
        Some(self.slots.len())
    }

    /// Plan the slots for `plan`. Each key the drawn atlas holds stays where
    /// it is, and the rest take Z ranges of it that nothing drawn reads. When
    /// they do not fit, a build already under way into an atlas of its own
    /// is reused if it can hold them; failing that a new atlas is made, with
    /// room for as much again, so arrivals after this one fit beside it.
    /// Whatever is already written somewhere (drawn, or written by the build
    /// this replaces) is copied on the GPU, never uploaded again.
    ///
    /// True while the larger atlas is still being made: nothing is planned
    /// and the build this replaces is kept as it was, for what it wrote.
    fn plan_build(&mut self, gpu: &Gpu, plan: Vec<([u32; 3], u64)>, need: [u32; 3], asked: usize) -> bool {
        let old = self.building.take();
        // A browser's WebGL cannot copy between these 3D textures; there
        // only the drawn atlas's own free space avoids uploading again.
        let can_copy = gpu.adapter.get_info().backend != wgpu::Backend::Gl;
        let size_of = |t: &wgpu::Texture| {
            let e = t.size();
            [e.width, e.height, e.depth_or_array_layers]
        };
        let old_slots: &[BuildSlot] = old.as_ref().map_or(&[], |b| &b.slots);
        // 1. The drawn atlas: keys it holds stay, a build into it keeps what it wrote.
        if self.atlas_refused.is_none() {
            let into_drawn = old.as_ref().is_some_and(|b| b.textures.is_none());
            let taken: Vec<(u32, u32)> = self.slots.iter().map(|s| (s.origin[2], s.origin[2] + s.dims[2])).collect();
            let keep = |dims: [u32; 3], key: u64| {
                if let Some(l) = self.slots.iter().find(|l| l.key == key && l.dims == dims) {
                    return Some(BuildSlot { origin: l.origin, dims, key, done_z: dims[2], tight: Some((l.tight_min, l.tight_max)) });
                }
                let p = old_slots.iter().find(|p| into_drawn && p.key == key && p.dims == dims)?;
                Some(BuildSlot { origin: p.origin, dims, key, done_z: p.done_z, tight: p.tight })
            };
            if let Some(slots) = fit_slots(&plan, size_of(&self._dist_tex), &taken, keep, |_, _| None) {
                self.building = Some(AtlasBuild { textures: None, slots: slots.into_iter().map(|(s, _)| s).collect(), refused: None });
                return false;
            }
        }
        // 2. The atlas a build already made: it keeps what it wrote, and what
        // is drawn is copied in.
        if let Some(b) = old.as_ref().filter(|b| b.refused.is_none())
            && let Some((dist, _)) = &b.textures
        {
            let live_src = |dims: [u32; 3], key: u64| {
                let l = self.slots.iter().find(|l| can_copy && l.key == key && l.dims == dims)?;
                Some((l.origin, dims[2], Some((l.tight_min, l.tight_max))))
            };
            let keep = |dims: [u32; 3], key: u64| {
                let p = b.slots.iter().find(|p| p.key == key && p.dims == dims)?;
                Some(BuildSlot { origin: p.origin, dims, key, done_z: p.done_z, tight: p.tight })
            };
            if let Some(slots) = fit_slots(&plan, size_of(dist), &[], keep, live_src) {
                let Some(b) = old else { return false };
                let Some((dist, color)) = b.textures else { return false };
                let slots = self.copy_in(gpu, &dist, &color, None, slots);
                self.building = Some(AtlasBuild { textures: Some((dist, color)), slots, refused: None });
                return false;
            }
        }
        // 3. A new atlas, with room for half as much again (or the largest
        // volume, if more), so the next arrivals fit beside what it holds.
        // Short of memory for that, exactly what is needed.
        let limit = gpu.device.limits().max_texture_dimension_3d;
        let largest = plan.iter().map(|(d, _)| d[2]).max().unwrap_or(1);
        let roomy = [need[0], need[1], need[2].saturating_add((need[2] / 2).max(largest)).min(limit)];
        let wanted = volume_bytes(need);
        let mut plan = plan;
        let mut refused = None;
        let plan_dims: Vec<[u32; 3]> = plan.iter().map(|(d, _)| *d).collect();
        let made = match self.grown(gpu, GrowAsk { roomy, plan: plan_dims }, need) {
            Grown::Waiting => {
                self.building = old;
                return true;
            }
            Grown::Made(t, n) => {
                if n < plan.len() {
                    refused = Some(wanted);
                    plan.truncate(n);
                }
                Some(t)
            }
            Grown::Refused => {
                // Not even one voxel: keep drawing what there is.
                self.building = None;
                self.atlas_refused = Some(AtlasRefused { bytes: wanted, dropped: asked });
                return false;
            }
            Grown::Failed => {
                self.frame_allocs += 1;
                try_alloc_volume_textures(&gpu.device, roomy, self.alloc_cap)
            }
        };
        let (dist, color) = match made {
            Some(t) => t,
            None => loop {
                let dims = stacked_dims(&plan.iter().map(|(d, _)| *d).collect::<Vec<_>>(), plan.len());
                self.frame_allocs += 1;
                if let Some(t) = try_alloc_volume_textures(&gpu.device, dims, self.alloc_cap) {
                    if dims != need {
                        refused.get_or_insert(wanted);
                    }
                    break t;
                }
                refused.get_or_insert(wanted);
                if plan.is_empty() {
                    // Not even one voxel: keep drawing what there is.
                    self.building = None;
                    self.atlas_refused = Some(AtlasRefused { bytes: wanted, dropped: asked });
                    return false;
                }
                let keep = plan.len() / 2;
                plan.truncate(keep);
            },
        };
        let old_slots: &[BuildSlot] = old.as_ref().map_or(&[], |b| &b.slots);
        let live_src = |dims: [u32; 3], key: u64| {
            let l = self.slots.iter().find(|l| can_copy && l.key == key && l.dims == dims)?;
            Some((l.origin, dims[2], Some((l.tight_min, l.tight_max))))
        };
        let begun = |dims: [u32; 3], key: u64| {
            let p = old_slots.iter().find(|p| can_copy && p.key == key && p.dims == dims && p.done_z > 0)?;
            Some((p.origin, p.done_z, p.tight))
        };
        let slots = fit_slots(&plan, size_of(&dist), &[], |_, _| None, |d, k| live_src(d, k).or_else(|| begun(d, k)))
            .unwrap_or_default();
        // What came from the old build is read from its atlas (or the drawn
        // one, when it was building into that).
        let old_tex = old.as_ref().and_then(|b| b.textures.as_ref());
        let slots = self.copy_in(gpu, &dist, &color, old_tex, slots);
        let refused = refused.map(|bytes| AtlasRefused { bytes, dropped: asked - plan.len() });
        self.building = Some(AtlasBuild { textures: Some((dist, color)), slots, refused });
        false
    }

    /// The atlas `ask` wants, at least `need`, from the worker making it,
    /// started here when none is under way: the roomy size, or as much as
    /// memory allows, all of it off the frame. Native only: a browser hands
    /// a texture over at once, so there it is made on the frame.
    #[cfg(target_arch = "wasm32")]
    fn grown(&mut self, _gpu: &Gpu, _ask: GrowAsk, _need: [u32; 3]) -> Grown {
        Grown::Failed
    }

    /// See the browser's [`grown`](Self::grown) above: natively the driver's
    /// work is a worker's, the fallbacks short of memory included. Made on
    /// the frame, an atlas the roomy size could not have was a whole
    /// allocation and its zeroing in one frame, as memory ran short.
    #[cfg(not(target_arch = "wasm32"))]
    fn grown(&mut self, gpu: &Gpu, ask: GrowAsk, need: [u32; 3]) -> Grown {
        let fits = |d: [u32; 3]| d[0] >= need[0] && d[1] >= need[1] && d[2] >= need[2];
        if let Some((asked, rx)) = self.growing.take() {
            match rx.try_recv() {
                // Everything asked for, in room enough for what is asked now.
                Ok(Some((t, dims, n))) if n == asked.plan.len() && fits(dims) => return Grown::Made(t, ask.plan.len()),
                // Short of memory, for exactly this list: what fitted.
                Ok(Some((t, _, n))) if asked == ask => return Grown::Made(t, n),
                Ok(None) if asked == ask => return Grown::Refused,
                // Made for another list: another is made below.
                Ok(_) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Grown::Failed,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.growing = Some((asked, rx));
                    return Grown::Waiting;
                }
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (device, queue) = (gpu.device.clone(), gpu.queue.clone());
        let (job, cap) = (ask.clone(), self.alloc_cap);
        let made = std::thread::Builder::new().name("sdf-atlas".into()).spawn(move || {
            let _ = tx.send(grow_atlas(&device, &queue, &job, cap));
        });
        if made.is_err() {
            return Grown::Failed;
        }
        self.growing = Some((ask, rx));
        Grown::Waiting
    }

    /// Atlases made on the frame's own thread since the renderer was made
    /// (see the field): zero natively. For guards.
    #[doc(hidden)]
    pub fn atlas_allocs_on_frame(&self) -> u64 {
        self.frame_allocs
    }

    /// Refuse an atlas of more than `bytes`, as a GPU short of memory would,
    /// or lift the cap with `None`. For tests: no GPU's memory can be made
    /// short to order.
    #[doc(hidden)]
    pub fn set_atlas_memory_cap(&mut self, bytes: Option<u64>) {
        self.alloc_cap = bytes;
    }

    /// Encode the GPU copies `fit_slots` asked for into `(dist, color)`: from
    /// the drawn atlas when the key is drawn there, else from `old` (the
    /// atlas of the build being replaced, or the drawn one when it built
    /// into that).
    fn copy_in(
        &self,
        gpu: &Gpu,
        dist: &wgpu::Texture,
        color: &wgpu::Texture,
        old: Option<&(wgpu::Texture, wgpu::Texture)>,
        slots: Vec<(BuildSlot, Option<[u32; 3]>)>,
    ) -> Vec<BuildSlot> {
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("sdf-atlas-keep") });
        let mut copied = false;
        for (s, from) in &slots {
            let Some(from) = from else { continue };
            let drawn = self.slots.iter().any(|l| l.key == s.key && l.origin == *from && l.dims == s.dims);
            let (sd, sc) = if drawn { (&self._dist_tex, &self._color_tex) } else { old.map_or((&self._dist_tex, &self._color_tex), |(d, c)| (d, c)) };
            for (a, b) in [(sd, dist), (sc, color)] {
                enc.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo { texture: a, mip_level: 0, origin: wgpu::Origin3d { x: from[0], y: from[1], z: from[2] }, aspect: wgpu::TextureAspect::All },
                    wgpu::TexelCopyTextureInfo {
                        texture: b,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: s.origin[0], y: s.origin[1], z: s.origin[2] },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d { width: s.dims[0], height: s.dims[1], depth_or_array_layers: s.done_z },
                );
            }
            copied = true;
        }
        if copied {
            gpu.queue.submit(Some(enc.finish()));
        }
        slots.into_iter().map(|(s, _)| s).collect()
    }

    /// Upload the sub-box `[min, max)` of the volume keyed `key`, wherever the
    /// atlas holds it: the drawn atlas, and the one being built. `baked` is
    /// that volume's bake, already holding the change. Returns the cells
    /// written.
    pub fn set_volume_region_keyed(&mut self, gpu: &Gpu, key: u64, baked: &BakedSdf, min: [u32; 3], max: [u32; 3]) -> u64 {
        let before = self.uploaded_cells;
        if key == 0 {
            return 0;
        }
        if let Some(i) = self.slots.iter().position(|s| s.key == key) {
            self.set_volume_region(gpu, i, baked, min, max);
        }
        let drawn_at = self.slots.iter().find(|s| s.key == key).map(|s| s.origin);
        if let Some(b) = self.building.as_mut()
            && let Some(s) = b.slots.iter_mut().find(|s| s.key == key && s.dims == baked.dims)
            // The same texels as the drawn slot's, written just above.
            && !(b.textures.is_none() && drawn_at == Some(s.origin))
        {
            // Only the layers already written: the rest are written from
            // `baked` when their turn comes, and it holds the change already.
            let [w, h, _] = baked.dims;
            let (x0, y0, z0) = (min[0].min(w), min[1].min(h), min[2].min(s.done_z));
            let (x1, y1, z1) = (max[0].clamp(x0, w), max[1].clamp(y0, h), max[2].clamp(z0, s.done_z));
            if x1 > x0 && y1 > y0 && z1 > z0 {
                s.tight = merge_box(s.tight, content_bounds(baked, [x0, y0, z0], [x1, y1, z1]));
                let (dist, color) = b.textures.as_ref().map_or((&self._dist_tex, &self._color_tex), |(d, c)| (d, c));
                self.uploaded_cells += write_sub_box(gpu, dist, color, s.origin, baked, [x0, y0, z0], [x1, y1, z1]);
            }
        }
        self.uploaded_cells - before
    }

    /// Does drawn slot `slot` hold `baked`'s distances, as the GPU has them?
    /// Reads the atlas back, waiting for the GPU: for tests.
    #[doc(hidden)]
    pub fn slot_holds(&self, gpu: &Gpu, slot: usize, baked: &BakedSdf) -> bool {
        let Some(s) = self.slots.get(slot) else { return false };
        if s.dims != baked.dims {
            return false;
        }
        let [w, h, d] = s.dims;
        let row = (w * 2).div_ceil(256) * 256;
        let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row * h * d) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self._dist_tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x: s.origin[0], y: s.origin[1], z: s.origin[2] },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: d },
        );
        gpu.queue.submit(Some(enc.finish()));
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        let data = buf.slice(..).get_mapped_range();
        (0..d).all(|z| {
            (0..h).all(|y| {
                let at = ((z * h + y) * row) as usize;
                let cells = &baked.distance[((z * h + y) * w) as usize..][..w as usize];
                data[at..at + (w * 2) as usize]
                    .chunks(2)
                    .zip(cells)
                    .all(|(b, &v)| u16::from_le_bytes([b[0], b[1]]) == f32_to_f16(v))
            })
        })
    }

    /// Is an atlas being built over frames, or a larger one being made for
    /// it ([`set_volumes_keyed`](Self::set_volumes_keyed))? Its caller goes
    /// on calling while this holds.
    pub fn atlas_building(&self) -> bool {
        self.building.is_some() || self.growing.is_some()
    }

    /// Volume cells written from the CPU so far, for counting what a frame
    /// uploads. Each cell is 6 bytes.
    pub fn uploaded_cells(&self) -> u64 {
        self.uploaded_cells
    }

    /// Fill the renderer-owned globals fields: each slot's atlas offset + dims,
    /// the uploaded volume count (`params.w`), and the tight content box — mapped
    /// from voxel coordinates into the caller's (camera-relative) world box, so
    /// it stays consistent under the floating origin. Callers only provide world
    /// data.
    fn patch_globals(&self, globals: &mut RaymarchGlobals) {
        let fog = self.fog_active.and_then(|i| self.fog_slots.get(i));
        globals.fog_cache = match fog {
            Some(f) if self.fog_ready.load(std::sync::atomic::Ordering::Relaxed) => {
                [if self.fog_debug { 2.0 } else { 1.0 }, f.size.0 as f32, f.size.1 as f32, f.stride as f32]
            }
            _ => [0.0; 4],
        };
        let mut count = 0;
        for i in 0..MAX_VOLUMES {
            // The atlas region this instance samples: its own slot, or the one
            // the caller named in `vol_atlas.w` (region + 1) to share a bake.
            let tag = globals.vol_atlas[i][3];
            let region = if tag >= 0.5 { tag as usize - 1 } else { i };
            let Some(s) = self.slots.get(region) else {
                // Nothing uploaded for it: absent, whatever the caller said,
                // or the shader would sample the atlas at a meaningless offset.
                globals.vol_center[i][3] = 0.0;
                continue;
            };
            if globals.vol_center[i][3] >= 0.5 {
                count = i + 1;
            }
            let (off, dims) = (s.origin, s.dims);
            // `w` keeps the caller's tag, so patching the same globals twice
            // maps every instance the same way both times.
            globals.vol_atlas[i] = [off[0] as f32, off[1] as f32, off[2] as f32, tag];
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
        globals.params[3] = count as f32;
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
    /// loaded (the prepass copy already primed + cleared it) and each ray stops
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

        // Clear to the SKY colour, never black. The full-screen sky fragment is
        // meant to repaint every background pixel, but at orbital scale a grazing
        // ray can hit an f32 edge (catastrophic cancellation in the atmosphere
        // shell math) and emit a NaN — which resolves to 0 (black) and shows the
        // clear through, an intermittent black flash confined to the sky that only
        // appears windowed (the camera moves every frame; a fixed-pose headless
        // probe never hits it). Clearing to the sky colour makes that fallback the
        // sky instead of black, so the flicker is impossible by construction.
        let bg = globals.bg;
        let clear = wgpu::LoadOp::Clear(wgpu::Color {
            r: bg[0] as f64,
            g: bg[1] as f64,
            b: bg[2] as f64,
            a: bg[3] as f64,
        });
        // The clear rides this pass, so a pass the GPU rejects (a texture it
        // binds failed to be made) leaves last frame's picture underneath, and
        // everything drawn after it piles up on top frame after frame. Once
        // anything has gone wrong, the clear goes first on its own, binding
        // nothing that could have failed.
        if crate::device::gpu_trouble() {
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raymarch-clear") });
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raymarch-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: clear, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: (!primed).then_some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            gpu.queue.submit([encoder.finish()]);
        }
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
                    ops: wgpu::Operations { load: clear, store: wgpu::StoreOp::Store },
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

    /// March this view's volumetric fog once per half-resolution texel, from
    /// the camera to the depth prepass's surface or out through the sky, for
    /// every surface and the sky to read instead of marching their own.
    ///
    /// Lit volumetric fog marched per fragment was most of a 1080p frame: at
    /// sixteen steps, twenty milliseconds of a thirty-millisecond frame on a
    /// mid-range card, paid once for the sky and again for every surface. Here
    /// it is paid once for four pixels. A fragment the prepass has no surface
    /// for at its depth (raymarched matter, glass, particles) still marches
    /// its own, so nothing is drawn with the fog of something else.
    ///
    /// Call it after the depth prepass is bound
    /// ([`bind_frame_targets`](Self::bind_frame_targets)) and before the
    /// passes that shade; `size` is the prepass's. Without a prepass it does
    /// nothing, and every fragment marches as before.
    pub fn march_fog(&mut self, gpu: &Gpu, globals: RaymarchGlobals, size: (u32, u32)) {
        if !self.fog_pass_on || self.prime_view.is_none() || size.0 == 0 || size.1 == 0 {
            return;
        }
        let half = (size.0.div_ceil(2), size.1.div_ceil(2));
        let slot = match self.fog_slots.iter().position(|f| f.size == half) {
            Some(i) => i,
            None => {
                // A few view sizes at most; a resized window replaces the
                // oldest rather than piling up targets.
                if self.fog_slots.len() >= 4 {
                    self.fog_slots.remove(0);
                    self.fog_active = None;
                }
                self.fog_slots.push(FogSlot::new(&gpu.device, half));
                self.fog_slots.len() - 1
            }
        };
        if self.fog_active != Some(slot) {
            self.fog_active = Some(slot);
            self.rebuild_binds(&gpu.device);
        }
        let Some(bind) = self.bind_primed.as_ref() else { return };
        self.fog_ready.store(false, std::sync::atomic::Ordering::Relaxed);
        self.upload_globals(gpu, globals);
        let f = &self.fog_slots[slot];
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raymarch-fog") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raymarch-fog"),
                color_attachments: &[
                    Some(wgpu::RenderPassColorAttachment {
                        view: &f.layers[0].1,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    }),
                    Some(wgpu::RenderPassColorAttachment {
                        view: &f.layers[1].1,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    }),
                ],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.fog_pipeline);
            // The buffer this pass's copy fills is bound here too; the march
            // does not read it.
            rp.set_bind_group(0, bind, &[]);
            rp.draw(0..3, 0..1);
        }
        // The near layer's texels, then the far layer's.
        for (k, (tex, _)) in f.layers.iter().enumerate() {
            encoder.copy_texture_to_buffer(
                tex.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &f.buf,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: (k as u32 * f.stride * f.size.1 * FOG_TEXEL) as u64,
                        bytes_per_row: Some(f.stride * FOG_TEXEL),
                        rows_per_image: Some(f.size.1),
                    },
                },
                wgpu::Extent3d { width: f.size.0, height: f.size.1, depth_or_array_layers: 1 },
            );
        }
        gpu.queue.submit([encoder.finish()]);
        // From here on the globals say the fog is there to read.
        self.fog_ready.store(true, std::sync::atomic::Ordering::Relaxed);
        self.upload_globals(gpu, globals);
    }

    /// Turn the fog pass off (every fragment marches its own fog, as it did
    /// before the pass) or back on: for comparing the two.
    pub fn set_fog_pass(&mut self, on: bool) {
        self.fog_pass_on = on;
    }

    /// Draw every surface and sky pixel that takes the fog pass's march
    /// magenta, and leave those that march their own as they are: what shows
    /// how much of a frame the pass serves.
    pub fn set_fog_debug(&mut self, on: bool) {
        self.fog_debug = on;
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

/// The bind group layout for the shared distance field (uniform globals +
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
            // The COLOR atlas (rgba8, alpha = exact palette slot byte): the raster's
            // splat shader textureLoads it so texture transitions blend WEIGHTS of
            // real slots instead of interpolating slot indices.
            vol_tex_entry(3),
            // Baked GI probes. Part of the field group rather than a group of its
            // own because the bounce belongs to the same shared shading model as
            // the shadows and the AO: one bind, and raster meshes, terrain, blobs
            // and .flsl materials all get it at the same moment.
            crate::gi::probe_tex_entry(4),
            // The opaque-mesh depth prepass, for CONTACT shadows. The raymarch
            // pass has its own copy at group(0) binding 7 (where it caps the
            // march); this is the same texture reaching the raster pass, so a
            // mesh's own silhouette shadows what it is standing on. Depth32Float
            // binds as an unfilterable float; the 1×1 fallback is how the shader
            // knows there is no prepass this frame.
            prime_tex_entry(5),
            // The captured sky + its roughness chain, so a surface has something
            // to REFLECT. Same reasoning as the GI probes above: it belongs to
            // the shared shading model, so one capture reaches raster meshes and
            // `.flsl` materials in a single bind.
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
            wgpu::BindGroupLayoutEntry {
                binding: 7,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            // The scene colour history (last frame, with a mip chain) and its
            // sampler — what a screen-space reflection reads. A 1×1 view here
            // is the "no history" signal.
            wgpu::BindGroupLayoutEntry {
                binding: 8,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 9,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            // Reflection probes: one equirectangular layer per probe, with the
            // same roughness chain the sky has. A 1×1 layer here is the "no
            // probes" signal, exactly as it is for the sky and the prepass.
            wgpu::BindGroupLayoutEntry {
                binding: 10,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 11,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            // The half-resolution fog (`fog_cached`).
            fog_buf_entry(12),
        ],
    })
}

/// The depth-prepass binding, in both layouts that carry it.
pub(crate) fn prime_tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn make_field_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    dist: &wgpu::Texture,
    color: &wgpu::Texture,
    sampler: &wgpu::Sampler,
    gi: &wgpu::Texture,
    prime: &wgpu::TextureView,
    env: &wgpu::TextureView,
    env_samp: &wgpu::Sampler,
    scene: &wgpu::TextureView,
    scene_samp: &wgpu::Sampler,
    probes: &wgpu::TextureView,
    probes_samp: &wgpu::Sampler,
    fog: &wgpu::Buffer,
) -> wgpu::BindGroup {
    let dist_view = dist.create_view(&wgpu::TextureViewDescriptor::default());
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let gi_view = gi.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("sdf-field"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&dist_view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&color_view) },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&gi_view) },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(prime) },
            wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(env) },
            wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::Sampler(env_samp) },
            wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::TextureView(scene) },
            wgpu::BindGroupEntry { binding: 9, resource: wgpu::BindingResource::Sampler(scene_samp) },
            wgpu::BindGroupEntry { binding: 10, resource: wgpu::BindingResource::TextureView(probes) },
            wgpu::BindGroupEntry { binding: 11, resource: wgpu::BindingResource::Sampler(probes_samp) },
            wgpu::BindGroupEntry { binding: 12, resource: fog.as_entire_binding() },
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
    gi: &wgpu::Texture,
    fog: &wgpu::Buffer,
) -> wgpu::BindGroup {
    let dist_view = dist.create_view(&wgpu::TextureViewDescriptor::default());
    let gi_view = gi.create_view(&wgpu::TextureViewDescriptor::default());
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
            wgpu::BindGroupEntry { binding: 10, resource: wgpu::BindingResource::TextureView(&terrain_view) },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(tile_sampler) },
            wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::Sampler(tile_sampler_nearest) },
            wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&sky_view) },
            wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(prime) },
            wgpu::BindGroupEntry { binding: 9, resource: wgpu::BindingResource::TextureView(&gi_view) },
            wgpu::BindGroupEntry { binding: 11, resource: fog.as_entire_binding() },
        ],
    })
}

/// The half-resolution fog as the fog pass writes it, one texel packed as
/// `fog_pack` (field.wgsl) says.
const FOG_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Uint;
/// Bytes in one packed fog texel.
const FOG_TEXEL: u32 = 16;

/// One view size's fog: half the view's size, rounded up, in two layers
/// (the nearest and the farthest surface of each texel's pixels). The pass
/// renders each layer into its texture and the frame copies both into the
/// buffer the shading passes read, one after the other: a raster surface's
/// sampled textures are all spoken for.
struct FogSlot {
    size: (u32, u32),
    /// Texels in a row of the buffer: the width, padded to what a copy out
    /// of a texture needs.
    stride: u32,
    layers: [(wgpu::Texture, wgpu::TextureView); 2],
    buf: wgpu::Buffer,
}

impl FogSlot {
    fn new(device: &wgpu::Device, size: (u32, u32)) -> Self {
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT / FOG_TEXEL;
        let stride = size.0.div_ceil(align) * align;
        let layer = || {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("raymarch-fog"),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FOG_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            (tex, view)
        };
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raymarch-fog"),
            size: (2 * stride * size.1 * FOG_TEXEL) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { size, stride, layers: [layer(), layer()], buf }
    }
}

/// The fog binding, in both layouts that carry it.
fn fog_buf_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// One texel's worth of fog buffer, for a frame no fog pass has run in.
pub(crate) fn fog_fallback_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("raymarch-fog-fallback"),
        size: FOG_TEXEL as u64,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
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
/// What an atlas of these dimensions costs: f16 distance + RGBA8 colour.
fn volume_bytes(dims: [u32; 3]) -> u64 {
    dims.iter().map(|&d| d as u64).product::<u64>() * (2 + 4)
}

/// The atlas the accepted volumes need: stacked along Z, as wide and tall as
/// the widest and tallest.
fn atlas_dims(accepted: &[(&BakedSdf, [u32; 3])]) -> [u32; 3] {
    let (mut w, mut h, mut d) = (1u32, 1u32, 0u32);
    for (b, _) in accepted {
        w = w.max(b.dims[0]);
        h = h.max(b.dims[1]);
        d += b.dims[2];
    }
    [w, h, d.max(1)]
}

/// [`alloc_volume_textures`], or `None` when the GPU is out of memory for it.
///
/// Asked through an error scope, which native wgpu answers at once. A browser
/// answers later; there the first poll comes back empty, the textures are
/// taken as made, and a failure arrives as the reported error it always was.
fn try_alloc_volume_textures(device: &wgpu::Device, dims: [u32; 3], cap: Option<u64>) -> Option<(wgpu::Texture, wgpu::Texture)> {
    use std::task::{Context, Poll, Waker};
    if cap.is_some_and(|c| volume_bytes(dims) > c) {
        return None;
    }
    let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let made = alloc_volume_textures(device, dims);
    let mut pop = std::pin::pin!(scope.pop());
    match pop.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(Some(_)) => None,
        _ => Some(made),
    }
}

/// The GPU refused the volume atlas for lack of memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtlasRefused {
    /// What the full atlas would have needed.
    pub bytes: u64,
    /// Volumes left out to make it fit.
    pub dropped: usize,
}

fn alloc_volume_textures(device: &wgpu::Device, dims: [u32; 3]) -> (wgpu::Texture, wgpu::Texture) {
    let size = wgpu::Extent3d { width: dims[0], height: dims[1], depth_or_array_layers: dims[2] };
    let dist = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sdf-distance"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::R16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sdf-color"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
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

/// Lay `plan` out in an atlas of `size`: a slot `keep` names stays where it
/// is; a slot `copy` finds written elsewhere (at `from`, `done_z` layers, with
/// its content box) takes a free Z range and is copied there; the rest take a
/// free range, first fit, and are uploaded. `taken` are Z ranges nothing may
/// take (what is drawn there). `None` when something does not fit.
#[allow(clippy::type_complexity)]
fn fit_slots(
    plan: &[([u32; 3], u64)],
    size: [u32; 3],
    taken: &[(u32, u32)],
    keep: impl Fn([u32; 3], u64) -> Option<BuildSlot>,
    copy: impl Fn([u32; 3], u64) -> Option<([u32; 3], u32, Option<([f32; 3], [f32; 3])>)>,
) -> Option<Vec<(BuildSlot, Option<[u32; 3]>)>> {
    let mut taken = taken.to_vec();
    let mut out: Vec<Option<(BuildSlot, Option<[u32; 3]>)>> =
        plan.iter().map(|&(d, k)| if k == 0 { None } else { keep(d, k).map(|s| (s, None)) }).collect();
    for (s, _) in out.iter().flatten() {
        taken.push((s.origin[2], s.origin[2] + s.dims[2]));
    }
    for (slot, &(dims, key)) in out.iter_mut().zip(plan) {
        if slot.is_some() {
            continue;
        }
        if dims[0] > size[0] || dims[1] > size[1] {
            return None;
        }
        taken.sort_unstable();
        let mut z = 0u32;
        for &(a, b) in &taken {
            if a >= z + dims[2] {
                break;
            }
            z = z.max(b);
        }
        if z + dims[2] > size[2] {
            return None;
        }
        taken.push((z, z + dims[2]));
        let mut s = BuildSlot { origin: [0, 0, z], dims, key, done_z: 0, tight: None };
        let from = if key == 0 { None } else { copy(dims, key) };
        let from = from.map(|(at, done, tight)| {
            s.done_z = done;
            s.tight = tight;
            at
        });
        *slot = Some((s, from));
    }
    out.into_iter().collect()
}

/// Write Z layers `[z0, z1)` of `baked` into the atlas at `origin`. The
/// layers are contiguous in the bake, so nothing is gathered.
fn write_slab(gpu: &Gpu, dist: &wgpu::Texture, color: &wgpu::Texture, origin: [u32; 3], baked: &BakedSdf, z0: u32, z1: u32) {
    let [w, h, _] = baked.dims;
    let (a, b) = ((z0 * w * h) as usize, (z1 * w * h) as usize);
    let dist_f16: Vec<u16> = baked.distance[a..b].iter().map(|&v| f32_to_f16(v)).collect();
    let at = wgpu::Origin3d { x: origin[0], y: origin[1], z: origin[2] + z0 };
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: z1 - z0 };
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: dist, mip_level: 0, origin: at, aspect: wgpu::TextureAspect::All },
        bytemuck::cast_slice(&dist_f16),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 2), rows_per_image: Some(h) },
        size,
    );
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: color, mip_level: 0, origin: at, aspect: wgpu::TextureAspect::All },
        bytemuck::cast_slice(&baked.color[a..b]),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        size,
    );
}

/// Write the sub-box `[lo, hi)` (voxel coords, already clamped) of `baked`
/// into the atlas slot at `off`. Returns the cells written.
fn write_sub_box(gpu: &Gpu, dist_tex: &wgpu::Texture, color_tex: &wgpu::Texture, off: [u32; 3], baked: &BakedSdf, lo: [u32; 3], hi: [u32; 3]) -> u64 {
    let [w, h, _] = baked.dims;
    let ([x0, y0, z0], [x1, y1, z1]) = (lo, hi);
    let (rw, rh, rd) = (x1 - x0, y1 - y0, z1 - z0);
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
        wgpu::TexelCopyTextureInfo { texture: dist_tex, mip_level: 0, origin, aspect: wgpu::TextureAspect::All },
        bytemuck::cast_slice(&dist),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(rw * 2), rows_per_image: Some(rh) },
        extent,
    );
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: color_tex, mip_level: 0, origin, aspect: wgpu::TextureAspect::All },
        bytemuck::cast_slice(&col),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(rw * 4), rows_per_image: Some(rh) },
        extent,
    );
    rw as u64 * rh as u64 * rd as u64
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

/// The WGSL every raymarch module starts from: the pass shader + the shared
/// distance-field module. The mirror of [`crate::raster::pass_prelude`], and
/// public for the same reason — something outside this file needs to validate
/// the real seam rather than a reconstruction of it.
pub fn prelude() -> &'static str {
    concat!(include_str!("raymarch.wgsl"), "\n", include_str!("field.wgsl"))
}

#[cfg(test)]
mod oom_tests {
    /// **An atlas bigger than video memory is refused, not made broken.** A
    /// 2048³ atlas is 48 GB: inside wgpu's 3D-texture limit, so only the GPU's
    /// memory can say no. Ignored by default: a software rasteriser (CI's) may
    /// promise memory it lazily never touches. Run it on a real GPU with
    /// `cargo test -p floptle-render -- --ignored atlas_bigger`.
    #[test]
    #[ignore = "needs a real GPU with under 48 GB of memory"]
    fn an_atlas_bigger_than_video_memory_is_refused() {
        let gpu = crate::Gpu::headless(4, 4);
        assert!(
            super::try_alloc_volume_textures(&gpu.device, [2048, 2048, 2048], None).is_none(),
            "a 48 GB atlas came back as made"
        );
        assert!(super::try_alloc_volume_textures(&gpu.device, [64, 64, 64], None).is_some(), "a small one must still be made");
    }
}

#[cfg(test)]
mod grow_tests {
    use super::{GrowAsk, grow_tries, stacked_dims};

    /// **Short of memory, the atlas worker falls back as the frame did, so
    /// the frame never has to.** The frame made an atlas itself whenever the
    /// roomy one was refused: the exact size, then half the volumes, each an
    /// allocation of up to hundreds of megabytes and its zeroing on the
    /// frame. The worker tries them all now, in the frame's order: roomy,
    /// exact, then halves down to one volume, never none.
    #[test]
    fn the_worker_tries_every_size_the_frame_would_have() {
        let plan = vec![[192, 192, 192], [128, 96, 64], [64, 64, 64], [192, 13, 192], [32, 32, 32]];
        let need = stacked_dims(&plan, plan.len());
        assert_eq!(need, [192, 192, 192 + 64 + 64 + 192 + 32]);
        let roomy = [need[0], need[1], need[2] + need[2] / 2];
        let tries = grow_tries(&GrowAsk { roomy, plan: plan.clone() });
        assert_eq!(
            tries,
            vec![
                (roomy, 5),
                (need, 5),
                (stacked_dims(&plan, 2), 2),
                (stacked_dims(&plan, 1), 1),
            ]
        );
        assert_eq!(stacked_dims(&plan, 1), [192, 192, 192]);
        assert!(grow_tries(&GrowAsk { roomy, plan: Vec::new() }).is_empty(), "nothing to hold, nothing to make");
    }
}

#[cfg(test)]
mod atlas_tests {
    use super::{AtlasVolume, Raymarch};
    use floptle_field::BakedSdf;

    /// A bake whose every cell says which bake and which cell it is.
    fn bake(seed: u32, dims: [u32; 3]) -> BakedSdf {
        let n = (dims[0] * dims[1] * dims[2]) as usize;
        BakedSdf {
            dims,
            center: [0.0; 3],
            half_extent: [dims[0] as f32 * 0.5, dims[1] as f32 * 0.5, dims[2] as f32 * 0.5],
            distance: (0..n).map(|i| ((i as u32 * 7 + seed * 131) % 997) as f32 * 0.01 - 2.0).collect(),
            color: (0..n).map(|i| [(i % 251) as u8, seed as u8, (i / 251 % 256) as u8, 255]).collect(),
        }
    }

    fn holds(gpu: &crate::Gpu, rm: &Raymarch, i: usize, b: &BakedSdf) -> bool {
        rm.slot_holds(gpu, i, b)
    }

    /// Calls of `set_volumes_keyed` until it says the atlas holds `vols`,
    /// each frame's upload at most the budget and a layer: the frames taken
    /// and the cells written in all.
    fn land(gpu: &crate::Gpu, rm: &mut Raymarch, vols: &[AtlasVolume], budget: u64, mut each: impl FnMut(&Raymarch)) -> (usize, u64) {
        let start = rm.uploaded_cells();
        for frame in 1..10_000 {
            let before = rm.uploaded_cells();
            let done = rm.set_volumes_keyed(gpu, vols, budget);
            let layer = vols.iter().map(|v| (v.baked.dims[0] * v.baked.dims[1]) as u64).max().unwrap_or(0);
            let wrote = rm.uploaded_cells() - before;
            assert!(wrote <= budget.saturating_add(layer), "a frame uploaded {wrote} cells against a budget of {budget}");
            gpu.queue.submit(None);
            if done.is_some() {
                assert_eq!(done, Some(vols.len()), "not every volume was accepted");
                return (frame, rm.uploaded_cells() - start);
            }
            each(rm);
        }
        panic!("the atlas never finished");
    }

    /// Calls until the atlas `vols` needs has been made and the build into
    /// it has begun: the larger atlas is made on a worker, the frames before
    /// it arrives write nothing.
    fn grow(gpu: &crate::Gpu, rm: &mut Raymarch, vols: &[AtlasVolume], budget: u64) {
        for _ in 0..10_000 {
            let before = rm.uploaded_cells();
            assert!(rm.set_volumes_keyed(gpu, vols, budget).is_none(), "it finished without growing");
            if rm.building.as_ref().is_some_and(|b| b.textures.is_some()) {
                return;
            }
            assert_eq!(rm.uploaded_cells(), before, "a frame waiting for the atlas uploaded");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the larger atlas never came");
    }

    /// A device these tests can judge the atlas on. OpenGL takes the path
    /// without GPU copies (as the browser does), so the counts these tests
    /// expect are not its counts; and reading a slot past the first back
    /// from a 3D texture answers wrongly there even for the plain whole
    /// upload, so a slot cannot be checked bit for bit.
    fn copying_gpu() -> Option<crate::Gpu> {
        let gpu = crate::Gpu::headless(4, 4);
        (gpu.adapter.get_info().backend != wgpu::Backend::Gl).then_some(gpu)
    }

    fn cells(b: &BakedSdf) -> u64 {
        b.distance.len() as u64
    }

    /// **A volume that arrives uploads itself and nothing else, over frames,
    /// and the old slots are drawn until it is in.** Every terrain landing
    /// re-uploaded the whole atlas in its frame: Solar's belt rocks, one
    /// after another, were 100 to 490 ms of `terrainShadow` a frame. A body
    /// arriving between two others (the slots are in id order) moves neither
    /// of them: their keys are drawn, so they stay. Counted in cells: the
    /// frames upload the new volume's cells exactly, a budget's worth at a
    /// time, while the drawn table stays the old one; after, every slot
    /// holds its own bake, bit for bit.
    #[test]
    fn a_volume_that_arrives_uploads_itself_and_nothing_else() {
        let Some(gpu) = copying_gpu() else { return };
        let mut rm = Raymarch::new(&gpu);
        let (a, b, c) = (bake(1, [48, 40, 64]), bake(2, [64, 64, 48]), bake(3, [64, 56, 64]));
        let ab = [AtlasVolume { baked: &a, key: 11 }, AtlasVolume { baked: &b, key: 12 }];
        let (_, first) = land(&gpu, &mut rm, &ab, u64::MAX, |_| {});
        assert_eq!(first, cells(&a) + cells(&b));
        let acb = [ab[0], AtlasVolume { baked: &c, key: 13 }, ab[1]];
        let budget = 64 * 56 * 6;
        let (frames, wrote) = land(&gpu, &mut rm, &acb, budget, |rm| {
            let keys: Vec<u64> = rm.slots.iter().map(|s| s.key).collect();
            assert_eq!(keys, [11, 12], "the drawn slots changed before the new volume was whole");
        });
        assert_eq!(wrote, cells(&c), "the volumes already in the atlas were uploaded again");
        assert!(frames >= 10, "the arrival landed in {frames} frames: no budget");
        for (i, v) in acb.iter().enumerate() {
            assert!(holds(&gpu, &rm, i, v.baked), "slot {i} does not hold its bake");
        }
        // And one leaving costs nothing.
        let (frames, wrote) = land(&gpu, &mut rm, &[acb[1], acb[2]], budget, |_| {});
        assert_eq!((frames, wrote), (1, 0));
        assert!(holds(&gpu, &rm, 0, &c) && holds(&gpu, &rm, 1, &b));
    }

    /// **An atlas too small for what arrives grows, and keeps what it held
    /// without uploading it.** The bigger atlas is written beside the drawn
    /// one; what the drawn one holds is copied across on the GPU, so the
    /// frames upload only the newcomer. A second arrival while that is under
    /// way joins the same build, keeping what it wrote.
    #[test]
    fn a_growing_atlas_copies_what_it_holds() {
        let Some(gpu) = copying_gpu() else { return };
        let mut rm = Raymarch::new(&gpu);
        let (a, b, c) = (bake(4, [32, 32, 32]), bake(5, [32, 32, 300]), bake(6, [32, 24, 40]));
        let va = AtlasVolume { baked: &a, key: 21 };
        land(&gpu, &mut rm, &[va], u64::MAX, |_| {});
        let room = rm._dist_tex.size().depth_or_array_layers;
        assert!(room < 32 + 300, "the fixture fits without growing");
        let vb = AtlasVolume { baked: &b, key: 22 };
        let vc = AtlasVolume { baked: &c, key: 23 };
        // B starts (once the larger atlas is made, off the frame); C
        // arrives two frames into it.
        let start = rm.uploaded_cells();
        grow(&gpu, &mut rm, &[va, vb], 32 * 32 * 10);
        assert!(rm.set_volumes_keyed(&gpu, &[va, vb], 32 * 32 * 10).is_none());
        land(&gpu, &mut rm, &[va, vb, vc], 32 * 32 * 10, |rm| assert_eq!(rm.slots.len(), 1));
        assert_eq!(rm.uploaded_cells() - start, cells(&b) + cells(&c), "something was uploaded twice");
        for (i, v) in [&a, &b, &c].into_iter().enumerate() {
            assert!(holds(&gpu, &rm, i, v), "slot {i} does not hold its bake");
        }
    }

    /// **An edit's box reaches the volume being built as well as the one
    /// drawn, and uploads only the box.** A dig landing while its terrain's
    /// atlas is re-laid is not lost when the new atlas takes over.
    #[test]
    fn an_edit_box_reaches_both_atlases() {
        let Some(gpu) = copying_gpu() else { return };
        let mut rm = Raymarch::new(&gpu);
        let mut a = bake(7, [40, 40, 40]);
        let b = bake(8, [40, 40, 600]);
        land(&gpu, &mut rm, &[AtlasVolume { baked: &a, key: 31 }], u64::MAX, |_| {});
        // Growing, so `a` is copied into a second atlas.
        let budget = 40 * 40 * 20;
        grow(&gpu, &mut rm, &[AtlasVolume { baked: &a, key: 31 }, AtlasVolume { baked: &b, key: 32 }], budget);
        for i in 0..a.distance.len() {
            let (x, y, z) = (i % 40, i / 40 % 40, i / 1600);
            if (5..9).contains(&x) && (10..20).contains(&y) && (30..33).contains(&z) {
                a.distance[i] = -1.5;
            }
        }
        let before = rm.uploaded_cells();
        rm.set_volume_region_keyed(&gpu, 31, &a, [5, 10, 30], [9, 20, 33]);
        assert_eq!(rm.uploaded_cells() - before, 2 * 4 * 10 * 3, "the box went to each atlas once");
        assert!(holds(&gpu, &rm, 0, &a), "the drawn atlas lacks the edit");
        let vols = [AtlasVolume { baked: &a, key: 31 }, AtlasVolume { baked: &b, key: 32 }];
        land(&gpu, &mut rm, &vols, budget, |_| {});
        assert!(holds(&gpu, &rm, 0, &a), "the new atlas lost the edit");
        assert!(holds(&gpu, &rm, 1, &b));
    }
}
