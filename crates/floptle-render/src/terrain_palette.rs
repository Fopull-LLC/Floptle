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
}

impl TerrainPalette {
    /// The palette from `layers`, each already `size`² (the loader resizes
    /// them). Building the mips is the slow part, so do it off the frame.
    pub fn from_layers(layers: Vec<TextureData>, size: u32) -> Self {
        let layers = layers
            .into_iter()
            .take(TERRAIN_SLOTS as usize)
            .map(|t| (t.width == size && t.height == size).then(|| srgb_mip_chain(t.pixels, size)))
            .collect();
        Self { size, layers }
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

    /// The palette as a [`TERRAIN_SLOTS`]-layer sRGB array with every level.
    pub fn upload(&self, gpu: &Gpu) -> wgpu::Texture {
        let levels = self.levels();
        let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain-palette"),
            size: wgpu::Extent3d { width: self.size, height: self.size, depth_or_array_layers: TERRAIN_SLOTS },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let white = vec![255u8; (self.size * self.size * 4) as usize];
        for layer in 0..TERRAIN_SLOTS {
            for level in 0..levels {
                let side = (self.size >> level).max(1);
                let bytes = (side * side * 4) as usize;
                let data = self.level(layer as usize, level as usize).unwrap_or(&white[..bytes]);
                gpu.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &tex,
                        mip_level: level,
                        origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(side * 4), rows_per_image: Some(side) },
                    wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
                );
            }
        }
        tex
    }
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
    fn the_layer_size_setting_rounds_down_to_an_allowed_size() {
        assert_eq!(terrain_layer_size(0), 256);
        assert_eq!(terrain_layer_size(256), 256);
        assert_eq!(terrain_layer_size(700), 512);
        assert_eq!(terrain_layer_size(1024), 1024);
        assert_eq!(terrain_layer_size(4096), 1024);
    }
}
