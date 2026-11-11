//! Texture arrays for merged models whose slots wear different pictures.
//!
//! A model merged into one instance (`Raster::push_slot_table`) draws with
//! one group(1) bind, so its slots used to merge only when every one wore the
//! same pictures. Here a picture can also be written as a LAYER of a pooled
//! array: pictures of the same size and sampling share an array, and a merged
//! model binds one array per kind of picture (base, normal, roughness,
//! metallic, occlusion, emissive), each slot naming the layer it reads in its
//! slot-table entry. The pool is shared between models, so instances of
//! different models that draw from the same arrays bind the same set.
//!
//! The standalone texture stays too: a model's parts, the Inspector and every
//! ordinary draw keep binding it. The layer is a copy of the same bytes,
//! written by the same mip writer (`write_mip_chain`), so a layer and its
//! texture sample the same.
//!
//! See `.internal/docs/texture-array-slots-plan.md` (card 0690).

use std::collections::{HashMap, HashSet};

use super::{Raster, TexBind, TexFilter, TexId, TexSampling, SURFACE_SLOTS};
use crate::device::Gpu;
use crate::mesh::TextureData;

/// Layers an array holds at most: a slot names its layer in eight bits.
pub(crate) const MAX_POOL_LAYERS: u32 = 255;

/// Layers a new array starts with; it doubles as it fills.
const FIRST_LAYERS: u32 = 4;

/// What pictures may share an array: one size, one mip chain, one sampler
/// (the array is bound with a single sampler) and one texture quality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PoolKey {
    pub width: u32,
    pub height: u32,
    pub mip_count: u32,
    pub sampling: TexSampling,
    pub drop: u32,
}

/// Where a picture sits in the pools: which array, and which layer of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PoolLayer {
    pub pool: u32,
    pub layer: u32,
}

pub(crate) struct Pool {
    pub key: PoolKey,
    pub texture: wgpu::Texture,
    pub capacity: u32,
    pub used: u32,
    /// Bumped each time the array is made again bigger, so a set bound
    /// over the old one is made again too.
    pub generation: u32,
    /// The layers a slot without a picture of this kind reads: white, and
    /// the flat normal.
    pub neutral_white: Option<u32>,
    pub neutral_flat: Option<u32>,
}

/// Every pool, and what is in them.
pub(crate) struct Pools {
    pub pools: Vec<Pool>,
    by_key: HashMap<PoolKey, u32>,
    /// Pictures already written, by their bytes and sampling: the same
    /// picture brought by two models is one layer.
    layers: HashMap<(u64, TexSampling), PoolLayer>,
    /// Which layer each registered texture is in.
    of_tex: HashMap<u32, PoolLayer>,
    /// A material set's six pictures, by the set's id (`Raster::material_set`).
    set_keys: HashMap<u32, [u32; SURFACE_SLOTS]>,
    /// Array sets made, by the array each kind binds (u32::MAX: none) and
    /// those arrays' generations.
    array_sets: HashMap<([u32; SURFACE_SLOTS], [u32; SURFACE_SLOTS]), TexId>,
    /// The ids that are array sets, which only the array pipelines may bind.
    pub array_ids: HashSet<u32>,
    /// group(1) over `D2Array` textures.
    pub layout: wgpu::BindGroupLayout,
}

impl Pools {
    pub fn new(device: &wgpu::Device) -> Self {
        Self {
            pools: Vec::new(),
            by_key: HashMap::new(),
            layers: HashMap::new(),
            of_tex: HashMap::new(),
            set_keys: HashMap::new(),
            array_sets: HashMap::new(),
            array_ids: HashSet::new(),
            layout: super::surface_bind_layout_of(device, wgpu::TextureViewDimension::D2Array, "raster-surface-array"),
        }
    }

    /// Remember what pictures a material set binds.
    pub fn note_set(&mut self, id: TexId, key: [u32; SURFACE_SLOTS]) {
        self.set_keys.insert(id.0, key);
    }

    /// Forget which pictures are in which layer: the texture quality changed,
    /// so a picture written again is written at the new one, into a pool of
    /// its own. Sets bound over the old layers are left to those who hold them.
    pub fn forget_layers(&mut self) {
        self.layers.clear();
        self.of_tex.clear();
    }

    /// The six pictures a bound texture draws with: a material set's, or a
    /// plain texture as a base with no maps.
    fn pictures_of(&self, t: Option<TexId>) -> [u32; SURFACE_SLOTS] {
        let mut k = [u32::MAX; SURFACE_SLOTS];
        match t {
            Some(t) => match self.set_keys.get(&t.0) {
                Some(key) => *key,
                None => {
                    k[0] = t.0;
                    k
                }
            },
            None => k,
        }
    }
}

impl Raster {
    /// Write the picture registered as `id` into the pools, if it is not
    /// there yet, and answer where it is. `data` and `sampling` are what
    /// `id` was registered with. `None` when the pools cannot take it: an
    /// array at its last layer, or a device that cannot make arrays that
    /// big.
    pub fn pool_texture(&mut self, gpu: &Gpu, id: TexId, data: &TextureData, sampling: TexSampling) -> Option<PoolLayer> {
        if let Some(&at) = self.pools.of_tex.get(&id.0) {
            return Some(at);
        }
        let mips = matches!(sampling.filter, TexFilter::SmoothMipmaps);
        let key = (super::picture_key(data, mips), sampling);
        if let Some(&at) = self.pools.layers.get(&key) {
            self.pools.of_tex.insert(id.0, at);
            return Some(at);
        }
        let drop = self.texture_drop;
        let smaller = if mips && drop > 0 { super::drop_top_mips(data, drop) } else { None };
        let t = smaller.as_ref().unwrap_or(data);
        let (w, h) = (t.width.max(1), t.height.max(1));
        let mip_count = if mips { 1 + (w.max(h) as f32).log2().floor() as u32 } else { 1 };
        let pool = self.pool_for(gpu, PoolKey { width: w, height: h, mip_count, sampling, drop })?;
        let layer = self.pool_reserve(gpu, pool)?;
        super::write_mip_chain(gpu, &self.pools.pools[pool as usize].texture, layer, t, mip_count);
        let at = PoolLayer { pool, layer };
        self.pools.layers.insert(key, at);
        self.pools.of_tex.insert(id.0, at);
        Some(at)
    }

    /// After [`Raster::replace_texture`] put new pixels under `id`: the same
    /// pixels into its layer when they are the same size, so a merged model
    /// shows them too; otherwise the picture leaves the pools, and a model
    /// whose slot wore it draws its parts until it is imported again. Either
    /// way the old pixels no longer answer for that layer.
    pub(crate) fn repool_replaced(&mut self, gpu: &Gpu, id: TexId, data: &TextureData) {
        let Some(at) = self.pools.of_tex.get(&id.0).copied() else { return };
        self.pools.layers.retain(|_, l| *l != at);
        let key = self.pools.pools[at.pool as usize].key;
        let smaller = if key.mip_count > 1 && key.drop > 0 { super::drop_top_mips(data, key.drop) } else { None };
        let t = smaller.as_ref().unwrap_or(data);
        if (t.width.max(1), t.height.max(1)) == (key.width, key.height) {
            super::write_mip_chain(gpu, &self.pools.pools[at.pool as usize].texture, at.layer, t, key.mip_count);
        } else {
            self.pools.of_tex.remove(&id.0);
        }
    }

    /// Where the picture registered as `id` is in the pools, if it is.
    pub fn pooled_layer(&self, id: TexId) -> Option<PoolLayer> {
        self.pools.of_tex.get(&id.0).copied()
    }

    /// Whether `tex` is an array set ([`Self::array_set_for`]), which only
    /// the array pipelines can bind.
    pub fn is_array_set(&self, tex: TexId) -> bool {
        self.pools.array_ids.contains(&tex.0)
    }

    /// The bytes the pools hold on the GPU, mip chains included.
    pub fn pooled_texture_bytes(&self) -> u64 {
        self.pools
            .pools
            .iter()
            .map(|p| {
                let (mut w, mut h, mut bytes) = (p.key.width as u64, p.key.height as u64, 0u64);
                for _ in 0..p.key.mip_count {
                    bytes += w * h * 4;
                    w = (w / 2).max(1);
                    h = (h / 2).max(1);
                }
                bytes * p.capacity as u64
            })
            .sum()
    }

    /// The set a merged model whose slots draw with `slots` binds, and each
    /// slot's six layers in it: one array per kind of picture, each slot's
    /// picture of that kind a layer of it. A slot with no picture of a kind
    /// where another slot has one reads that array's white layer (its flat
    /// normal for a normal map); a kind no slot has binds the neutral default.
    ///
    /// `None` when a slot's picture is not in the pools ([`Self::pool_texture`])
    /// or two slots' pictures of one kind are in different arrays (another
    /// size or sampling): those slots draw as instances of their own.
    pub fn array_set_for(&mut self, gpu: &Gpu, slots: &[Option<TexId>]) -> Option<(TexId, Vec<[u8; SURFACE_SLOTS]>)> {
        let pictures: Vec<[u32; SURFACE_SLOTS]> = slots.iter().map(|&t| self.pools.pictures_of(t)).collect();
        let mut pool_of_kind = [u32::MAX; SURFACE_SLOTS];
        for k in 0..SURFACE_SLOTS {
            for p in &pictures {
                if p[k] == u32::MAX {
                    continue;
                }
                let at = self.pools.of_tex.get(&p[k])?;
                match pool_of_kind[k] {
                    u32::MAX => pool_of_kind[k] = at.pool,
                    pool if pool != at.pool => return None,
                    _ => {}
                }
            }
        }
        let mut layers = vec![[0u8; SURFACE_SLOTS]; slots.len()];
        for k in 0..SURFACE_SLOTS {
            let pool = pool_of_kind[k];
            if pool == u32::MAX {
                continue;
            }
            for (i, p) in pictures.iter().enumerate() {
                let layer = match p[k] {
                    u32::MAX => self.pool_neutral(gpu, pool, k == 1)?,
                    t => self.pools.of_tex[&t].layer,
                };
                layers[i][k] = u8::try_from(layer).ok()?;
            }
        }
        let generations = pool_of_kind.map(|p| self.pools.pools.get(p as usize).map_or(0, |p| p.generation));
        let key = (pool_of_kind, generations);
        if let Some(&id) = self.pools.array_sets.get(&key) {
            return Some((id, layers));
        }
        let id = self.make_array_set(gpu, pool_of_kind);
        self.pools.array_sets.insert(key, id);
        Some((id, layers))
    }

    /// The array that takes pictures of `key`, made if there is none.
    fn pool_for(&mut self, gpu: &Gpu, key: PoolKey) -> Option<u32> {
        if let Some(&p) = self.pools.by_key.get(&key) {
            return Some(p);
        }
        let capacity = FIRST_LAYERS.min(self.max_pool_layers(gpu));
        if capacity == 0 {
            return None;
        }
        let texture = pool_texture_of(gpu, &key, capacity);
        let p = self.pools.pools.len() as u32;
        self.pools.pools.push(Pool { key, texture, capacity, used: 0, generation: 0, neutral_white: None, neutral_flat: None });
        self.pools.by_key.insert(key, p);
        Some(p)
    }

    fn max_pool_layers(&self, gpu: &Gpu) -> u32 {
        gpu.device.limits().max_texture_array_layers.min(MAX_POOL_LAYERS)
    }

    /// The next free layer of `pool`, making the array bigger first if it is
    /// full: a new one twice the size, the old layers copied in. At import,
    /// never per frame.
    fn pool_reserve(&mut self, gpu: &Gpu, pool: u32) -> Option<u32> {
        let max = self.max_pool_layers(gpu);
        let p = &mut self.pools.pools[pool as usize];
        if p.used == p.capacity {
            if p.capacity >= max {
                return None;
            }
            let capacity = (p.capacity * 2).min(max);
            let bigger = pool_texture_of(gpu, &p.key, capacity);
            let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raster-pool-grow") });
            for level in 0..p.key.mip_count {
                let (w, h) = ((p.key.width >> level).max(1), (p.key.height >> level).max(1));
                enc.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo { texture: &p.texture, mip_level: level, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    wgpu::TexelCopyTextureInfo { texture: &bigger, mip_level: level, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: p.used },
                );
            }
            gpu.queue.submit([enc.finish()]);
            p.texture = bigger;
            p.capacity = capacity;
            p.generation += 1;
        }
        p.used += 1;
        Some(p.used - 1)
    }

    /// `pool`'s white layer, or its flat-normal layer, written the first time
    /// a model asks.
    fn pool_neutral(&mut self, gpu: &Gpu, pool: u32, flat: bool) -> Option<u32> {
        let p = &self.pools.pools[pool as usize];
        if let Some(l) = if flat { p.neutral_flat } else { p.neutral_white } {
            return Some(l);
        }
        let (w, h, mips) = (p.key.width, p.key.height, p.key.mip_count);
        let texel: [u8; 4] = if flat { [128, 128, 255, 255] } else { [255; 4] };
        let data = TextureData { width: w, height: h, pixels: texel.repeat((w * h) as usize) };
        let layer = self.pool_reserve(gpu, pool)?;
        super::write_mip_chain(gpu, &self.pools.pools[pool as usize].texture, layer, &data, mips);
        let p = &mut self.pools.pools[pool as usize];
        if flat {
            p.neutral_flat = Some(layer);
        } else {
            p.neutral_white = Some(layer);
        }
        Some(layer)
    }

    /// The group(1) set over one array per kind (`u32::MAX`: the neutral
    /// default viewed as a one-layer array), registered as a texture id the
    /// array pipelines bind.
    fn make_array_set(&mut self, gpu: &Gpu, pool_of_kind: [u32; SURFACE_SLOTS]) -> TexId {
        // Base and emissive are pictures, read through the sRGB view; the
        // other four are numbers, read raw (see `TexBind::linear_view`).
        let srgb = |k: usize| k == 0 || k == 5;
        let array_view = |tex: &wgpu::Texture, srgb: bool| {
            tex.create_view(&wgpu::TextureViewDescriptor {
                format: Some(if srgb && tex.format() == wgpu::TextureFormat::Rgba8UnormSrgb {
                    wgpu::TextureFormat::Rgba8UnormSrgb
                } else if tex.format() == wgpu::TextureFormat::Rgba8UnormSrgb {
                    wgpu::TextureFormat::Rgba8Unorm
                } else {
                    tex.format()
                }),
                dimension: Some(wgpu::TextureViewDimension::D2Array),
                ..Default::default()
            })
        };
        let mut views = Vec::with_capacity(SURFACE_SLOTS);
        let mut samplers = Vec::with_capacity(SURFACE_SLOTS);
        for (k, &pool) in pool_of_kind.iter().enumerate() {
            match self.pools.pools.get(pool as usize) {
                Some(p) => {
                    views.push(array_view(&p.texture, srgb(k)));
                    let sampling = p.key.sampling;
                    samplers.push(self.sampler_for(gpu, sampling));
                }
                None => {
                    let neutral = if k == 1 { &self._flat_normal_tex } else { &self.default_tex };
                    views.push(array_view(neutral, srgb(k)));
                    samplers.push(self.neutral_samp.clone());
                }
            }
        }
        let slots: [(&wgpu::TextureView, &wgpu::Sampler); SURFACE_SLOTS] = std::array::from_fn(|k| (&views[k], &samplers[k]));
        let bind = super::make_surface_bind(&gpu.device, &self.pools.layout, "raster-surface-array-set", slots);
        // `view` / `_texture` mirror the default, as a set with no base does:
        // nothing asks an array set for a picture to show.
        let view = self.default_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let linear_view = super::linear_view_of(&self.default_tex);
        let id = TexId(self.textures.len() as u32);
        self.textures.push(TexBind {
            bind,
            view,
            linear_view,
            sampling: TexSampling::default(),
            _texture: self.default_tex.clone(),
            revision: Default::default(),
        });
        self.pools.array_ids.insert(id.0);
        id
    }
}

/// An array of `capacity` layers for pictures of `key`, in the format every
/// engine picture is uploaded in, readable raw as well as sRGB.
fn pool_texture_of(gpu: &Gpu, key: &PoolKey, capacity: u32) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("raster-texture-pool"),
        size: wgpu::Extent3d { width: key.width, height: key.height, depth_or_array_layers: capacity },
        mip_level_count: key.mip_count,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[wgpu::TextureFormat::Rgba8Unorm],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MaterialParams, TexFilter, TexSampling};

    fn raster() -> Option<(Gpu, Raster)> {
        let gpu = Gpu::headless(16, 16);
        if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
            eprintln!("skipped — this device cannot build the raster pass");
            return None;
        }
        let raster = Raster::new(&gpu);
        Some((gpu, raster))
    }

    /// A picture of `w`×`h` whose every texel is `seed` mixed with its place,
    /// so two pictures and two texels never read alike.
    fn picture(w: u32, h: u32, seed: u8) -> TextureData {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(&[seed, (x * 7) as u8 ^ seed, (y * 13) as u8, 255]);
            }
        }
        TextureData { width: w, height: h, pixels }
    }

    fn mipped() -> TexSampling {
        TexSampling { filter: TexFilter::SmoothMipmaps, ..Default::default() }
    }

    /// **Pictures of one size and sampling share an array, one layer each,
    /// and the same picture twice is one layer.** Another size, or another
    /// sampling, is another array.
    #[test]
    fn pictures_pool_by_size_and_sampling() {
        let Some((gpu, mut r)) = raster() else { return };
        let s = mipped();
        let (a, b, a2) = (picture(32, 32, 1), picture(32, 32, 2), picture(32, 32, 1));
        let ta = r.register_texture_shared(&gpu, &a, s);
        let tb = r.register_texture_shared(&gpu, &b, s);
        let ta2 = r.register_texture(&gpu, &a2, s);
        let la = r.pool_texture(&gpu, ta, &a, s).unwrap();
        let lb = r.pool_texture(&gpu, tb, &b, s).unwrap();
        let la2 = r.pool_texture(&gpu, ta2, &a2, s).unwrap();
        assert_eq!(la.pool, lb.pool, "two 32² mipped pictures went to two arrays");
        assert_ne!(la.layer, lb.layer);
        assert_eq!(la, la2, "the same picture registered twice took two layers");
        let small = picture(16, 16, 3);
        let ts = r.register_texture(&gpu, &small, s);
        assert_ne!(r.pool_texture(&gpu, ts, &small, s).unwrap().pool, la.pool, "a 16² picture shared a 32² array");
        let flat = TexSampling::default();
        let tf = r.register_texture(&gpu, &b, flat);
        assert_ne!(r.pool_texture(&gpu, tf, &b, flat).unwrap().pool, la.pool, "another sampler shared the array");
        assert_eq!(r.pooled_layer(ta), Some(la));
    }

    /// **An array that fills is made again bigger with every layer it held,
    /// at every mip level, byte for byte.** Four pictures fill a new array;
    /// each layer is read back, two more pictures make it grow, and every
    /// layer reads the same as before, its top level the picture's own bytes.
    #[test]
    fn a_full_array_grows_and_keeps_its_layers() {
        let Some((gpu, mut r)) = raster() else { return };
        let s = mipped();
        let pics: Vec<TextureData> = (0..6).map(|i| picture(32, 32, 10 + i)).collect();
        let mut at = Vec::new();
        let mut before = Vec::new();
        for (i, p) in pics.iter().enumerate() {
            let t = r.register_texture(&gpu, p, s);
            at.push(r.pool_texture(&gpu, t, p, s).unwrap());
            if i == 3 {
                let pool = &r.pools.pools[at[0].pool as usize];
                assert_eq!(pool.generation, 0, "the array grew before it was full");
                for l in &at {
                    before.push([0, 2, 5].map(|level| crate::probe::readback_layer(&gpu, &pool.texture, l.layer, level)));
                }
            }
        }
        let pool = &r.pools.pools[at[0].pool as usize];
        assert!(pool.capacity >= 6 && pool.generation >= 1, "the array never grew: {} layers, generation {}", pool.capacity, pool.generation);
        for (i, levels) in before.iter().enumerate() {
            for (j, level) in [0, 2, 5].into_iter().enumerate() {
                let got = crate::probe::readback_layer(&gpu, &pool.texture, at[i].layer, level);
                assert!(got == levels[j], "layer {} level {level} changed when the array grew", at[i].layer);
            }
            assert!(levels[0] == pics[i].pixels, "layer {} does not hold its picture", at[i].layer);
        }
        for (i, p) in pics.iter().enumerate().skip(4) {
            assert!(crate::probe::readback_layer(&gpu, &pool.texture, at[i].layer, 0) == p.pixels, "a layer written after the growth is wrong");
        }
    }

    /// **A merged model's slots bind one set when their pictures share
    /// arrays, each slot naming its layers, and the same slots bind the same
    /// set again.** A slot with no normal map beside one that has one reads
    /// the array's flat-normal layer, written once. A picture of another
    /// size, or one never pooled, is refused.
    #[test]
    fn slots_in_one_array_bind_one_set() {
        let Some((gpu, mut r)) = raster() else { return };
        let s = mipped();
        let (wall, trim, normal) = (picture(32, 32, 1), picture(32, 32, 2), picture(32, 32, 3));
        let tw = r.register_texture_shared(&gpu, &wall, s);
        let tt = r.register_texture_shared(&gpu, &trim, s);
        let tn = r.register_texture_shared(&gpu, &normal, s);
        for (t, d) in [(tw, &wall), (tt, &trim), (tn, &normal)] {
            r.pool_texture(&gpu, t, d, s).unwrap();
        }
        let wall_set = r.material_set(&gpu, Some(tw), [Some(tn), None, None, None, None]);
        let (set, layers) = r.array_set_for(&gpu, &[Some(wall_set), Some(tt)]).expect("a set for two slots in one array");
        assert!(r.is_array_set(set));
        assert_eq!(layers[0][0], r.pooled_layer(tw).unwrap().layer as u8);
        assert_eq!(layers[1][0], r.pooled_layer(tt).unwrap().layer as u8);
        assert_eq!(layers[0][1], r.pooled_layer(tn).unwrap().layer as u8);
        let flat = r.pools.pools[r.pooled_layer(tn).unwrap().pool as usize].neutral_flat.expect("a flat-normal layer");
        assert_eq!(layers[1][1] as u32, flat, "the trim, with no normal map, reads the flat normal");
        let used = r.pools.pools[0].used;
        let (again, layers2) = r.array_set_for(&gpu, &[Some(wall_set), Some(tt)]).unwrap();
        assert_eq!((again, &layers2), (set, &layers), "the same slots made a second set");
        assert_eq!(r.pools.pools[0].used, used, "the flat-normal layer was written twice");

        let odd = picture(16, 16, 4);
        let to = r.register_texture(&gpu, &odd, s);
        r.pool_texture(&gpu, to, &odd, s).unwrap();
        assert!(r.array_set_for(&gpu, &[Some(tw), Some(to)]).is_none(), "a 16² base shared a set with a 32² one");
        let loose = r.register_texture(&gpu, &picture(32, 32, 5), s);
        assert!(r.array_set_for(&gpu, &[Some(tw), Some(loose)]).is_none(), "a picture never pooled was bound");
    }

    /// **New pixels under a pooled picture reach its layer when they are the
    /// same size, and take it out of the pools when they are not.**
    #[test]
    fn a_replaced_picture_rewrites_its_layer_or_leaves() {
        let Some((gpu, mut r)) = raster() else { return };
        let s = mipped();
        let (a, b) = (picture(32, 32, 1), picture(32, 32, 2));
        let ta = r.register_texture_shared(&gpu, &a, s);
        let at = r.pool_texture(&gpu, ta, &a, s).unwrap();
        let tex = |r: &Raster| r.pools.pools[at.pool as usize].texture.clone();
        r.replace_texture(&gpu, ta, &b);
        assert!(crate::probe::readback_layer(&gpu, &tex(&r), at.layer, 0) == b.pixels, "the layer kept the old pixels");
        assert_eq!(r.pooled_layer(ta), Some(at));
        // The old picture, brought again, is not the layer that now holds b.
        let ta2 = r.register_texture(&gpu, &a, s);
        assert_ne!(r.pool_texture(&gpu, ta2, &a, s), Some(at), "the old bytes still answered for the rewritten layer");
        r.replace_texture(&gpu, ta, &picture(16, 16, 3));
        assert_eq!(r.pooled_layer(ta), None, "a picture of another size stayed in its array");
    }

    /// **A slot table that names no layers is the table it always was; one
    /// that names layers is its own.** Layers for the wrong number of slots
    /// get no table.
    #[test]
    fn layered_slot_tables_keep_the_plain_ones() {
        let Some((_gpu, mut r)) = raster() else { return };
        let p = |c: f32| MaterialParams::flat([c, 0.5, 0.5]);
        let slots = [p(0.1), p(0.2), p(0.3)];
        let plain = r.push_slot_table(&[4, 8], &slots).unwrap();
        assert_eq!(r.push_slot_table_layered(&[4, 8], &slots, None), Some(plain));
        assert_eq!(r.push_slot_table_layered(&[4, 8], &slots, Some(&[[0; 6]; 3])), Some(plain));
        let layered = r.push_slot_table_layered(&[4, 8], &slots, Some(&[[0; 6], [1, 0, 0, 0, 0, 0], [2, 3, 0, 0, 0, 1]])).unwrap();
        assert_ne!(layered, plain);
        let base = layered as usize * super::super::EXT_LANES;
        // Group entry, header, then each slot's eight lanes: the third slot's
        // lanes 6 and 7.
        let rec = base + 2 * super::super::EXT_LANES + 2 * 8;
        assert_eq!(r.mat_ext_cpu[rec + 6][2..], [2.0, 3.0]);
        assert_eq!(r.mat_ext_cpu[rec + 7], [0.0, 0.0, 0.0, 1.0]);
        assert!(r.push_slot_table_layered(&[4, 8], &slots, Some(&[[0; 6]; 2])).is_none());
    }
}
