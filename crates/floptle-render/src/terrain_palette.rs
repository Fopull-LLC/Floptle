//! The terrain palette: every slot's picture at one size, with its whole mip
//! chain, as one texture array both the raymarch and the raster sample.
//!
//! A palette slot is a photograph of ground tiled every few metres. Without
//! smaller copies of it, ground a hundred metres out samples one texel in
//! dozens and crawls as the camera moves; with them, the GPU reads a copy
//! already averaged to the size the pixel covers. The copies are averaged in
//! linear light, so a speckled rock does not darken as it recedes.

use crate::{Gpu, TextureData, TERRAIN_SLOTS};

/// The size a palette layer is stored at when a project names none.
pub const TERRAIN_LAYER_SIZE: u32 = 256;

/// The sizes a palette layer may be stored at (`terrain_layer_size` in
/// project.ron). Each step up quadruples the memory: 32 layers with their
/// mips are about 10.7, 42.7 and 170.7 MiB.
pub const TERRAIN_LAYER_SIZES: [u32; 3] = [256, 512, 1024];

/// The palette size a project's setting asks for, as one the palette can
/// be: the largest allowed size not over it, and never under the smallest.
pub fn terrain_layer_size(asked: u32) -> u32 {
    TERRAIN_LAYER_SIZES.iter().copied().filter(|&s| s <= asked).max().unwrap_or(TERRAIN_LAYER_SIZE)
}

/// A palette ready to go to the GPU: each layer's mip chain, largest first,
/// RGBA8 sRGB. A layer of another size, or none, is white at every level, so
/// slot indices still line up with what the terrain painted.
pub struct TerrainPalette {
    size: u32,
    layers: Vec<Option<Vec<Vec<u8>>>>,
    /// The slots' surface maps, as stored (not sRGB): entry `i` is slot `i`'s
    /// normal map, entry `TERRAIN_SLOTS + i` its occlusion, roughness,
    /// metalness and height. They go up as the palette array's layers past
    /// its colours. Empty when no slot has any.
    surfaces: Vec<Option<Vec<Vec<u8>>>>,
    normal_mask: u32,
    orm_mask: u32,
}

/// Layers in the palette array when a slot has surface maps: the colours,
/// then a normal map and an ORM-and-height map for each slot. Without any it
/// is the colours alone, [`TERRAIN_SLOTS`].
pub const SURFACED_LAYERS: u32 = TERRAIN_SLOTS * 3;

/// A slot with no normal map: straight up out of the surface.
const FLAT_NORMAL: [u8; 4] = [128, 128, 255, 255];
/// A slot with no ORM map: no occlusion, and the material's own roughness and
/// metalness (the map multiplies them), at the middle height.
const PLAIN_ORM: [u8; 4] = [255, 255, 255, 128];

impl TerrainPalette {
    /// The palette from `layers`, each already `size`² (the loader resizes
    /// them). Building the mips is the slow part, so do it off the frame.
    pub fn from_layers(layers: Vec<TextureData>, size: u32) -> Self {
        let layers = layers
            .into_iter()
            .take(TERRAIN_SLOTS as usize)
            .map(|t| (t.width == size && t.height == size).then(|| srgb_mip_chain(t.pixels, size)))
            .collect();
        Self { size, layers, surfaces: Vec::new(), normal_mask: 0, orm_mask: 0 }
    }

    /// Add each slot's surface maps, each already `size`² or `None`: a normal
    /// map (OpenGL, +Y up), an ORM map (glTF: R occlusion, G roughness, B
    /// metalness) and a height map (its red channel). A slot that names none
    /// costs nothing; a palette where none does has no surface array at all.
    pub fn with_surfaces(
        mut self,
        normals: Vec<Option<TextureData>>,
        orms: Vec<Option<TextureData>>,
        heights: Vec<Option<TextureData>>,
    ) -> Self {
        let size = self.size;
        let fits = |t: &TextureData| t.width == size && t.height == size;
        let slots = TERRAIN_SLOTS as usize;
        let mut normal_layers = Vec::with_capacity(slots);
        let mut orm_layers = Vec::with_capacity(slots);
        let (mut normal_mask, mut orm_mask) = (0u32, 0u32);
        let mut normals = normals.into_iter();
        let mut orms = orms.into_iter();
        let mut heights = heights.into_iter();
        for i in 0..slots {
            let n = normals.next().flatten().filter(fits);
            let o = orms.next().flatten().filter(fits);
            let h = heights.next().flatten().filter(fits);
            normal_layers.push(n.map(|t| {
                normal_mask |= 1 << i;
                linear_mip_chain(t.pixels, size, true)
            }));
            if o.is_some() {
                orm_mask |= 1 << i;
            }
            orm_layers.push(ormh_chain(o.as_ref(), h.as_ref(), size));
        }
        if normal_mask != 0 || orm_layers.iter().any(Option::is_some) {
            normal_layers.extend(orm_layers);
            self.surfaces = normal_layers;
        }
        self.normal_mask = normal_mask;
        self.orm_mask = orm_mask;
        self
    }

    /// The slots with a normal map, bit per slot.
    pub fn normal_mask(&self) -> u32 {
        self.normal_mask
    }

    /// The slots with an ORM map, bit per slot.
    pub fn orm_mask(&self) -> u32 {
        self.orm_mask
    }

    /// The side of the largest level.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// How many levels each layer has, down to 1×1.
    pub fn levels(&self) -> u32 {
        self.size.max(1).ilog2() + 1
    }

    /// The pixels of layer `layer` at level `level`, if the layer has a picture.
    pub fn level(&self, layer: usize, level: usize) -> Option<&[u8]> {
        self.layers.get(layer)?.as_ref()?.get(level).map(Vec::as_slice)
    }

    /// The palette as a [`TERRAIN_SLOTS`]-layer sRGB array with every level,
    /// or [`SURFACED_LAYERS`] when a slot has surface maps. The maps share the
    /// colours' array, rather than taking a binding of their own, so a custom
    /// shader keeps every texture slot it had; each value is stored as the
    /// sRGB code that decodes back to it.
    pub fn upload(&self, gpu: &Gpu) -> wgpu::Texture {
        let levels = self.levels();
        let layers = if self.surfaces.is_empty() { TERRAIN_SLOTS } else { SURFACED_LAYERS };
        let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain-palette"),
            size: wgpu::Extent3d { width: self.size, height: self.size, depth_or_array_layers: layers },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            // COPY_SRC so a guard can read back what a slot holds.
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let white = vec![255u8; (self.size * self.size * 4) as usize];
        for layer in 0..TERRAIN_SLOTS {
            for level in 0..levels {
                let side = (self.size >> level).max(1);
                let bytes = (side * side * 4) as usize;
                let data = self.level(layer as usize, level as usize).unwrap_or(&white[..bytes]);
                write_layer(gpu, &tex, layer, level, side, data);
            }
        }
        if !self.surfaces.is_empty() {
            let code = data_to_srgb_table();
            for (i, surface) in self.surfaces.iter().enumerate() {
                let neutral = if (i as u32) < TERRAIN_SLOTS { FLAT_NORMAL } else { PLAIN_ORM };
                for level in 0..levels {
                    let side = (self.size >> level).max(1);
                    let encoded: Vec<u8> = match surface.as_ref().and_then(|c| c.get(level as usize)) {
                        Some(px) => px.iter().map(|&v| code[v as usize]).collect(),
                        None => neutral.map(|v| code[v as usize]).repeat((side * side) as usize),
                    };
                    write_layer(gpu, &tex, TERRAIN_SLOTS + i as u32, level, side, &encoded);
                }
            }
        }
        tex
    }
}

/// An ORM map and a height map packed as one chain (R occlusion, G
/// roughness, B metalness, A height), each `size`²; `None` when neither is.
fn ormh_chain(o: Option<&TextureData>, h: Option<&TextureData>, size: u32) -> Option<Vec<Vec<u8>>> {
    if o.is_none() && h.is_none() {
        return None;
    }
    let texels = (size * size) as usize;
    let mut px = Vec::with_capacity(texels * 4);
    for k in 0..texels {
        let orm = o.map_or([255, 255, 255], |t| [t.pixels[k * 4], t.pixels[k * 4 + 1], t.pixels[k * 4 + 2]]);
        let height = h.map_or(128, |t| t.pixels[k * 4]);
        px.extend_from_slice(&[orm[0], orm[1], orm[2], height]);
    }
    Some(linear_mip_chain(px, size, false))
}

/// One slot's pictures, mip chains built and ready to write into a live
/// palette ([`write_slot`]): its colour, and its normal and ORM-and-height
/// maps if it has them. What a palette that fills in slot by slot hands from
/// its worker to the frame.
pub struct SlotPictures {
    pub colour: Option<Vec<Vec<u8>>>,
    pub normal: Option<Vec<Vec<u8>>>,
    pub orm: Option<Vec<Vec<u8>>>,
}

impl SlotPictures {
    /// The chains for one slot's pictures, each already `size`² (others are
    /// left out, as [`TerrainPalette::from_layers`] and
    /// [`TerrainPalette::with_surfaces`] leave them).
    pub fn new(
        colour: Option<TextureData>,
        normal: Option<TextureData>,
        orm: Option<TextureData>,
        height: Option<TextureData>,
        size: u32,
    ) -> Self {
        let fits = |t: &TextureData| t.width == size && t.height == size;
        let (o, h) = (orm.filter(fits), height.filter(fits));
        Self {
            colour: colour.filter(fits).map(|t| srgb_mip_chain(t.pixels, size)),
            normal: normal.filter(fits).map(|t| linear_mip_chain(t.pixels, size, true)),
            orm: ormh_chain(o.as_ref(), h.as_ref(), size),
        }
    }
}

/// A palette standing in while its pictures are read: `size`² with every
/// level, and each slot one flat colour, `means[i]` (linear RGB; mid grey
/// past the list). With `surfaced`, the layers for surface maps too, flat
/// normals and plain ORM. The ground shows its colours from the first frame
/// rather than white, and the pictures land into it slot by slot
/// ([`write_slot`]).
pub fn stand_in(gpu: &Gpu, size: u32, means: &[[f32; 3]], surfaced: bool) -> wgpu::Texture {
    let levels = size.max(1).ilog2() + 1;
    let layers = if surfaced { SURFACED_LAYERS } else { TERRAIN_SLOTS };
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("terrain-palette"),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: layers },
        mip_level_count: levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        // COPY_SRC so a guard can read back what a slot holds.
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let code = data_to_srgb_table();
    let mut fill = vec![0u8; (size * size * 4) as usize];
    for layer in 0..layers {
        let texel = if layer < TERRAIN_SLOTS {
            let m = means.get(layer as usize).copied().unwrap_or([0.214; 3]);
            [linear_to_srgb(m[0]), linear_to_srgb(m[1]), linear_to_srgb(m[2]), 255]
        } else if layer < 2 * TERRAIN_SLOTS {
            FLAT_NORMAL.map(|v| code[v as usize])
        } else {
            PLAIN_ORM.map(|v| code[v as usize])
        };
        for px in fill.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&texel);
        }
        for level in 0..levels {
            let side = (size >> level).max(1);
            write_layer(gpu, &tex, layer, level, side, &fill[..(side * side * 4) as usize]);
        }
    }
    tex
}

/// Write one slot's pictures into a live palette made by [`stand_in`] or
/// [`TerrainPalette::upload`] at the same size: its colour layer, and its
/// surface-map layers when the palette has them. A picture the slot does not
/// have leaves its layer as it was.
pub fn write_slot(gpu: &Gpu, tex: &wgpu::Texture, slot: u32, pictures: &SlotPictures) {
    if slot >= TERRAIN_SLOTS {
        return;
    }
    let size = tex.width();
    let write = |layer: u32, chain: &[Vec<u8>], encode: Option<&[u8; 256]>| {
        for (level, px) in chain.iter().enumerate() {
            let side = (size >> level).max(1);
            if px.len() != (side * side * 4) as usize {
                return;
            }
            match encode {
                Some(code) => {
                    let enc: Vec<u8> = px.iter().map(|&v| code[v as usize]).collect();
                    write_layer(gpu, tex, layer, level as u32, side, &enc);
                }
                None => write_layer(gpu, tex, layer, level as u32, side, px),
            }
        }
    };
    if let Some(c) = &pictures.colour {
        write(slot, c, None);
    }
    if tex.depth_or_array_layers() >= SURFACED_LAYERS {
        let code = data_to_srgb_table();
        if let Some(n) = &pictures.normal {
            write(TERRAIN_SLOTS + slot, n, Some(&code));
        }
        if let Some(o) = &pictures.orm {
            write(2 * TERRAIN_SLOTS + slot, o, Some(&code));
        }
    }
}

fn write_layer(gpu: &Gpu, tex: &wgpu::Texture, layer: u32, level: u32, side: u32, data: &[u8]) {
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: level,
            origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(side * 4), rows_per_image: Some(side) },
        wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
    );
}

/// A square data picture's mip chain (normals, ORM): each level the 2×2
/// average of the one above, as stored. With `normal` the averaged vector is
/// put back to unit length, so a bumpy surface far off reads flatter rather
/// than shorter.
pub fn linear_mip_chain(level0: Vec<u8>, size: u32, normal: bool) -> Vec<Vec<u8>> {
    let mut chain = vec![level0];
    let mut side = size;
    while side > 1 {
        let next_side = side / 2;
        let cur = chain.last().expect("level 0 is there");
        let mut next = vec![0u8; (next_side * next_side * 4) as usize];
        for y in 0..next_side {
            for x in 0..next_side {
                let at = |dx: u32, dy: u32| (((2 * y + dy) * side + 2 * x + dx) * 4) as usize;
                let taps = [at(0, 0), at(1, 0), at(0, 1), at(1, 1)];
                let o = ((y * next_side + x) * 4) as usize;
                let mut avg = [0.0f32; 4];
                for (c, a) in avg.iter_mut().enumerate() {
                    *a = taps.iter().map(|&i| cur[i + c] as f32).sum::<f32>() * 0.25;
                }
                if normal {
                    let v = [avg[0] / 127.5 - 1.0, avg[1] / 127.5 - 1.0, avg[2] / 127.5 - 1.0];
                    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                    if len > 1e-4 {
                        for c in 0..3 {
                            avg[c] = (v[c] / len + 1.0) * 127.5;
                        }
                    }
                }
                for c in 0..4 {
                    next[o + c] = (avg[c] + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        }
        chain.push(next);
        side = next_side;
    }
    chain
}

/// sRGB byte to linear light.
fn srgb_to_linear_table() -> [f32; 256] {
    let mut t = [0.0f32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        let c = i as f32 / 255.0;
        *v = if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    }
    t
}

/// For each stored data byte, the sRGB code whose decode is nearest it: what
/// a surface map is written into the sRGB palette array as.
fn data_to_srgb_table() -> [u8; 256] {
    let lin = srgb_to_linear_table();
    let mut t = [0u8; 256];
    for (v, code) in t.iter_mut().enumerate() {
        let want = v as f32 / 255.0;
        let mut best = 0usize;
        for (c, l) in lin.iter().enumerate() {
            if (l - want).abs() < (lin[best] - want).abs() {
                best = c;
            }
        }
        *code = best as u8;
    }
    t
}

fn linear_to_srgb(l: f32) -> u8 {
    let l = l.clamp(0.0, 1.0);
    let c = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
    (c * 255.0 + 0.5) as u8
}

/// A square sRGB picture's mip chain, `level0` first: each level the 2×2
/// average of the one above in linear light (alpha averaged as is).
pub fn srgb_mip_chain(level0: Vec<u8>, size: u32) -> Vec<Vec<u8>> {
    let lin = srgb_to_linear_table();
    let mut chain = vec![level0];
    let mut side = size;
    while side > 1 {
        let next_side = side / 2;
        let cur = chain.last().expect("level 0 is there");
        let mut next = vec![0u8; (next_side * next_side * 4) as usize];
        for y in 0..next_side {
            for x in 0..next_side {
                let at = |dx: u32, dy: u32| (((2 * y + dy) * side + 2 * x + dx) * 4) as usize;
                let taps = [at(0, 0), at(1, 0), at(0, 1), at(1, 1)];
                let o = ((y * next_side + x) * 4) as usize;
                for c in 0..3 {
                    let sum: f32 = taps.iter().map(|&i| lin[cur[i + c] as usize]).sum();
                    next[o + c] = linear_to_srgb(sum * 0.25);
                }
                let a: u32 = taps.iter().map(|&i| cur[i + 3] as u32).sum();
                next[o + 3] = ((a + 2) / 4) as u8;
            }
        }
        chain.push(next);
        side = next_side;
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layer_has_every_level_down_to_one_texel() {
        let p = TerrainPalette::from_layers(vec![TextureData { pixels: vec![200; 256 * 256 * 4], width: 256, height: 256 }], 256);
        assert_eq!(p.levels(), 9);
        for level in 0..9 {
            let side = 256usize >> level;
            assert_eq!(p.level(0, level).map(<[u8]>::len), Some(side * side * 4), "level {level}");
        }
        assert!(p.level(1, 0).is_none(), "a slot with no picture is white at upload");
    }

    #[test]
    fn mips_average_in_linear_light() {
        // Black and white checkered texel by texel: a gamma-blind average is
        // sRGB 128, which is 22% of white's light, not half of it. Half of
        // white's light is sRGB 188.
        let mut px = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                px[(y * 4 + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let chain = srgb_mip_chain(px, 4);
        assert_eq!(chain.len(), 3);
        for (level, pixels) in chain.iter().enumerate().skip(1) {
            for t in pixels.chunks(4) {
                assert!((187..=189).contains(&t[0]), "level {level} averaged to sRGB {}", t[0]);
                assert_eq!(t[3], 255);
            }
        }
    }

    #[test]
    fn a_normal_maps_mips_stay_unit_length() {
        // Two opposite tilts side by side: the plain average is half as long
        // as a normal, and reads as a dark, flattened surface.
        let mut px = Vec::new();
        for i in 0..4 {
            px.extend_from_slice(if i % 2 == 0 { &[218, 128, 218, 255] } else { &[38, 128, 218, 255] });
        }
        let chain = linear_mip_chain(px, 2, true);
        let t = &chain[1];
        let v = [t[0] as f32 / 127.5 - 1.0, t[1] as f32 / 127.5 - 1.0, t[2] as f32 / 127.5 - 1.0];
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        assert!((len - 1.0).abs() < 0.02, "the averaged normal is {len} long");
        assert!(v[2] > 0.99, "two opposite tilts average to straight up, not {v:?}");
    }

    #[test]
    fn surface_maps_go_where_named_and_nowhere_else() {
        let grey = |v: u8| Some(TextureData { pixels: vec![v; 4 * 4 * 4], width: 4, height: 4 });
        let p = TerrainPalette::from_layers(Vec::new(), 4);
        let none = p.with_surfaces(vec![None; 3], vec![None; 3], vec![None; 3]);
        assert!(none.surfaces.is_empty(), "a palette naming no maps holds no surface array");
        let p = TerrainPalette::from_layers(Vec::new(), 4).with_surfaces(vec![None, grey(200)], vec![grey(90)], vec![None, None, grey(40)]);
        assert_eq!((p.normal_mask(), p.orm_mask()), (0b010, 0b001));
        let orm = |slot: usize| p.surfaces[TERRAIN_SLOTS as usize + slot].as_ref().map(|c| c[0][..4].to_vec());
        assert_eq!(orm(0), Some(vec![90, 90, 90, 128]), "an ORM map with no height sits at the middle height");
        assert_eq!(orm(1), None);
        assert_eq!(orm(2), Some(vec![255, 255, 255, 40]), "a height map alone keeps the plain ORM");
    }

    #[test]
    fn a_surface_value_survives_the_srgb_array() {
        // Stored as the code that decodes nearest it, every byte comes back
        // within the step of the sRGB curve at its brightest, about 1.1 bytes.
        let (code, lin) = (data_to_srgb_table(), srgb_to_linear_table());
        let worst = (0..256usize).map(|v| (lin[code[v] as usize] * 255.0 - v as f32).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1.2, "a stored value came back {worst} bytes off");
        assert_eq!(code[0], 0);
        assert_eq!(code[255], 255);
    }

    #[test]
    fn the_layer_size_setting_rounds_down_to_an_allowed_size() {
        assert_eq!(terrain_layer_size(0), 256);
        assert_eq!(terrain_layer_size(256), 256);
        assert_eq!(terrain_layer_size(700), 512);
        assert_eq!(terrain_layer_size(1024), 1024);
        assert_eq!(terrain_layer_size(4096), 1024);
    }
}
