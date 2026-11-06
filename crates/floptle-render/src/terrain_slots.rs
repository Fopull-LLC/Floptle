//! The terrain slot store: one record per palette slot, read by the meshed
//! terrain (raster) and the raymarched terrain alike.
//!
//! Every per-slot setting the terrain shaders read lives here: the scale,
//! the blend width and its roughness, anti-tiling and macro variation, the
//! filter and mip flags, glow, which palette layers hold the slot's colour and
//! surface maps, and the material knobs a slot lays over its terrain's
//! Material (tint, emissive, roughness, metalness, normal and occlusion
//! strength, shading model). One storage buffer, so growing past 32 slots or
//! adding a knob costs no uniform space and no texture binding.
//!
//! [`TerrainSlotRaw::IDENTITY`] is a slot that changes nothing: a store of
//! identities draws exactly what the palette drew before the store existed.
//! The layout is mirrored by `struct TerrainSlot` in `field.wgsl`; change the
//! two together (a test holds the size).

use bytemuck::{Pod, Zeroable};

/// Slots a store holds: the palette's slots, and room to grow.
pub const SLOT_STORE_LEN: usize = 256;

/// "No layer": the slot has no picture of that kind.
pub const SLOT_NONE: u32 = 0xffff;

/// Bits in [`TerrainSlotRaw::flags`]`[0]`.
pub mod flag {
    /// The colour is pixel art: read nearest, full-size level only.
    pub const NEAREST: u32 = 1 << 0;
    /// The slot reads its mips by footprint (`SmoothMipmaps`).
    pub const MIPS: u32 = 1 << 1;
    /// Self-lit: the colour bypasses lighting (the cave-visibility channel).
    pub const GLOW: u32 = 1 << 2;
    /// Shade with the Classic model whatever the terrain's Material says.
    pub const SHADING_CLASSIC: u32 = 1 << 3;
    /// Shade with the Physical model whatever the terrain's Material says.
    pub const SHADING_PHYSICAL: u32 = 1 << 4;
    /// Unlit: the colour as it is, no light at all.
    pub const UNLIT: u32 = 1 << 5;
    /// The slot has a Material of its own: `pbr`, `emissive`, the tint and
    /// the shading model are its values, not multipliers on the terrain's.
    pub const OWN_MATERIAL: u32 = 1 << 6;
}

/// One slot, as the shaders read it. Eleven `vec4`s, 176 bytes; see the
/// field notes for each lane's meaning and its identity value.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct TerrainSlotRaw {
    /// rgb tint over the colour (1), w triplanar scale multiplier (1).
    pub tint_scale: [f32; 4],
    /// rgb emissive colour, w strength (0).
    pub emissive: [f32; 4],
    /// x roughness ×, y metalness ×, z normal strength × (negative flips),
    /// w occlusion strength × (all 1).
    pub pbr: [f32; 4],
    /// x blend width in metres (0), y blend roughness (0.5), z height blend
    /// (0), w height bias (0).
    pub blend: [f32; 4],
    /// x antitile (0), y macro strength (0), z macro size in metres (0 = 60),
    /// w height contrast (1).
    pub tile: [f32; 4],
    /// x wobble amount, y wobble size, z pixel dither, w parallax (all 0).
    pub border: [f32; 4],
    /// Classic specular override: rgb, w strength (w < 0 = inherit).
    pub specular: [f32; 4],
    /// Classic overrides: x shininess, y rim strength, z ambient (each < 0 =
    /// inherit), w a pixel-art picture's own width in texels (0 = the
    /// layer's), the grid a pixel border steps on.
    pub classic: [f32; 4],
    /// rgb rim colour, w wetness response (reserved; 1).
    pub rim_wet: [f32; 4],
    /// Palette layers: x colour, y normal map, z ORM and height, w extra
    /// ([`SLOT_NONE`] = none).
    pub layers: [u32; 4],
    /// x flag bits ([`flag`]), y surface-shader index (0 = none), z shader
    /// param base, w border style (0 soft).
    pub flags: [u32; 4],
}

impl TerrainSlotRaw {
    /// A slot that changes nothing, its colour in layer `colour`.
    pub const fn identity(colour: u32) -> Self {
        Self {
            tint_scale: [1.0, 1.0, 1.0, 1.0],
            emissive: [0.0; 4],
            pbr: [1.0; 4],
            blend: [0.0, 0.5, 0.0, 0.0],
            tile: [0.0, 0.0, 0.0, 1.0],
            border: [0.0; 4],
            specular: [1.0, 1.0, 1.0, -1.0],
            classic: [-1.0, -1.0, -1.0, 0.0],
            rim_wet: [0.0, 0.0, 0.0, 1.0],
            layers: [colour, SLOT_NONE, SLOT_NONE, SLOT_NONE],
            flags: [0; 4],
        }
    }

    /// The identity store: slot `i` reads palette layer `i`, and nothing else
    /// is set. What a renderer starts with, so a probe that never sets the
    /// store draws as before.
    pub fn identity_store() -> Vec<Self> {
        (0..SLOT_STORE_LEN as u32).map(Self::identity).collect()
    }
}

/// The GPU buffer a store lives in, sized for [`SLOT_STORE_LEN`] slots and
/// holding the identity store until written.
pub fn new_slot_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    use wgpu::util::DeviceExt;
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("terrain-slots"),
        contents: bytemuck::cast_slice(&TerrainSlotRaw::identity_store()),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
    })
}

/// Write `slots` at the start of a store's buffer; the rest keeps what it had.
pub fn write_slots(queue: &wgpu::Queue, buf: &wgpu::Buffer, slots: &[TerrainSlotRaw]) {
    let n = slots.len().min(SLOT_STORE_LEN);
    if n > 0 {
        queue.write_buffer(buf, 0, bytemuck::cast_slice(&slots[..n]));
    }
}

/// The bind-group layout entry for a store: read-only storage, fragment.
pub const fn slot_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shader reads the store as `array<TerrainSlot>`: eleven vec4s, no
    /// padding. A lane added on one side and not the other reads every slot
    /// after the first from the wrong place.
    #[test]
    fn the_slot_store_stride_matches_the_shader() {
        assert_eq!(std::mem::size_of::<TerrainSlotRaw>(), 11 * 16);
        let wgsl = include_str!("field.wgsl");
        let start = wgsl.find("struct TerrainSlot {").expect("field.wgsl declares the slot");
        let body = &wgsl[start..start + wgsl[start..].find("};").unwrap()];
        let lanes = body.lines().filter(|l| l.trim_start().split(':').nth(1).is_some_and(|t| t.contains("vec4<"))).count();
        assert_eq!(lanes, 11, "field.wgsl's TerrainSlot has {lanes} vec4 lanes:\n{body}");
    }
}
