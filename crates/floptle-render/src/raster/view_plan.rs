//! **One bucket plan per view per frame.**
//!
//! The sun map (both cascades), the depth prepass and the colour pass all
//! draw the same frame's instances. Each used to sort the whole list into
//! buckets and upload it again for itself: four hash sweeps over every
//! instance and three uploads of it a frame, a third of them for the near
//! cascade alone. A [`ViewPlan`] is that sorting done once, in the colour
//! pass's order, and uploaded once. The prepass draws the plan's opaque
//! buckets where they lie; each sun cascade keeps the members its cull
//! passes, bucket by bucket, without hashing anything again, and uploads
//! only those, behind the plan in the same buffer.
//!
//! The colour pass draws exactly the buckets, in exactly the order, it
//! always did: transparent draws blend in first-appearance order, so that
//! order is the picture. The depth passes draw the same set of draws they
//! always did, grouped a little differently (a shader surface no longer
//! shares a bucket with a plain one on the same mesh); a depth-only pass
//! with a less-than test keeps the nearest fragment whatever order they
//! arrive in, and its cut-out discard is per fragment, so its picture
//! cannot change with the order. The tests at the end hold both against
//! copies of the bucketing the passes did before.

use super::*;

/// How opaque an instance must be to draw in the opaque passes.
pub(super) const OPAQUE_CUTOFF: f32 = 0.999;

/// One bucket of plain draws: (mesh, texture, first instance, count).
pub(super) type Bucket = (usize, Option<u32>, u32, u32);

/// What the passes route a draw by, read from the surface extras in one
/// sweep: every pass used to make its own copies of these, and the sun map
/// made them once a cascade.
#[derive(Clone, Debug, Default)]
pub(super) struct ExtFlags {
    /// Screen-door transparency or a dissolve: opaque whatever the alpha.
    pub dither: Vec<bool>,
    /// The material blends, whatever the instance alpha.
    pub blend: Vec<bool>,
    /// Glass: its own pass draws it, after the scene behind it.
    pub glass: Vec<bool>,
    /// Casts no sun shadow.
    pub no_cast: Vec<bool>,
    /// How far the material's sway can bend it (lane 2 `.z`).
    pub sway: Vec<f32>,
}

impl ExtFlags {
    pub(super) fn of(mat_ext_cpu: &[[f32; 4]]) -> Self {
        let lanes = mat_ext_cpu.as_chunks::<EXT_LANES>().0;
        let mut f = ExtFlags {
            dither: Vec::with_capacity(lanes.len()),
            blend: Vec::with_capacity(lanes.len()),
            glass: Vec::with_capacity(lanes.len()),
            no_cast: Vec::with_capacity(lanes.len()),
            sway: Vec::with_capacity(lanes.len()),
        };
        for c in lanes {
            let bits = c[1][0] as u32;
            f.dither.push(bits & (EXT_DITHER_ALPHA | EXT_DISSOLVE) != 0);
            f.blend.push(bits & EXT_BLEND != 0);
            // lane 1 `.w` is transmission; only the metal-rough model has a
            // refraction term.
            f.glass.push(c[1][3] > 0.0 && bits & EXT_PHYSICAL != 0);
            f.no_cast.push(bits & EXT_NO_CAST != 0);
            f.sway.push(c[2][2].max(0.0));
        }
        f
    }

    fn flag(v: &[bool], raw: &InstanceRaw) -> bool {
        v.get(ext_index_of(raw) as usize).copied().unwrap_or(false)
    }

    /// Drawn in the opaque passes. Terrain's alpha is a dissolve threshold,
    /// not opacity, and a dithered surface is opaque by design; anything
    /// else is opaque when it is whole and its material does not blend.
    pub(super) fn opaque(&self, raw: &InstanceRaw) -> bool {
        is_terrain(raw) || Self::flag(&self.dither, raw) || (raw.color[3] >= OPAQUE_CUTOFF && !Self::flag(&self.blend, raw))
    }

    /// Not glass.
    pub(super) fn solid(&self, raw: &InstanceRaw) -> bool {
        !Self::flag(&self.glass, raw)
    }

    pub(super) fn casts(&self, raw: &InstanceRaw) -> bool {
        !Self::flag(&self.no_cast, raw)
    }

    pub(super) fn sway(&self, raw: &InstanceRaw) -> f32 {
        self.sway.get(ext_index_of(raw) as usize).copied().unwrap_or(0.0)
    }
}

/// What the passes need to know about a `.flsl` binding's shader.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FlslClass {
    /// Drawn in the opaque phase (it declares no blend).
    pub opaque: bool,
    /// It moves its vertices, so depth passes draw it through its own
    /// pipelines.
    pub displaced: bool,
    /// It reads the scene behind it, so it may wait for the capture.
    pub reads_scene: bool,
}

/// `.flsl` draws, plain and skinned, that `wanted` accepts, bucketed by
/// (mesh, texture, binding, skinned) into `raws`.
pub(super) fn bucket_flsl_by(
    flsl: &[FlslDraw],
    skins: &[SkinDraw],
    raws: &mut Vec<InstanceRaw>,
    wanted: impl Fn(&FlslBindingId, &InstanceRaw) -> bool,
) -> Vec<FlslBucket> {
    let groups = group_by_key(
        flsl.iter()
            .filter(|(_, _, bind, raw)| wanted(bind, raw))
            .map(|(id, tex, bind, raw)| ((id.0 as usize, tex.map(|t| t.0), bind.0, false), *raw))
            .chain(skins.iter().filter_map(|d| {
                let bind = d.flsl?;
                wanted(&bind, &d.instance)
                    .then(|| ((d.mesh.0 as usize, d.tex.map(|t| t.0), bind.0, true), Raster::skin_instance(d)))
            })),
    );
    let mut buckets = Vec::with_capacity(groups.len());
    for ((mesh_idx, tex_key, bind_id, skinned), members) in groups {
        let start = raws.len() as u32;
        raws.extend_from_slice(&members);
        buckets.push((mesh_idx, tex_key, bind_id, skinned, start, members.len() as u32));
    }
    buckets
}

/// Skinned draws that `keep` accepts, bucketed by (mesh, texture) into `raws`.
pub(super) fn bucket_skins_by(skins: &[SkinDraw], raws: &mut Vec<InstanceRaw>, keep: impl Fn(&SkinDraw) -> bool) -> Vec<Bucket> {
    let groups = group_by_key(
        skins.iter().filter(|d| keep(d)).map(|d| ((d.mesh.0 as usize, d.tex.map(|t| t.0)), Raster::skin_instance(d))),
    );
    let mut out = Vec::with_capacity(groups.len());
    for ((mesh_idx, tex_key), members) in groups {
        let start = raws.len() as u32;
        raws.extend_from_slice(&members);
        out.push((mesh_idx, tex_key, start, members.len() as u32));
    }
    out
}

/// A view's draws for one frame, sorted into buckets once and shared by
/// every pass that draws them: the sun map, the depth prepass and the
/// colour pass. Made by [`Raster::plan_view`].
///
/// The plan sits at the start of the raster's instance buffer once a pass
/// has put it there. Anything else that writes there (another view's
/// passes, the glass or the selection mask) makes it stale, and the next
/// pass that draws from it puts it back: right, only slower.
pub struct ViewPlan {
    /// The instance buffer generation the plan was uploaded at; 0 is never.
    pub(super) uploaded: u64,
    /// Every record, in the colour pass's order.
    pub(super) records: Vec<InstanceRaw>,
    pub(super) opaque: Vec<Bucket>,
    pub(super) transparent: Vec<Bucket>,
    /// Every opaque-phase shader draw: the colour pass leaves out those
    /// waiting for the scene capture, the depth passes draw them all.
    pub(super) flsl_opaque: Vec<FlslBucket>,
    pub(super) flsl_blended: Vec<FlslBucket>,
    pub(super) skin_opaque: Vec<Bucket>,
    pub(super) skin_blended: Vec<Bucket>,
    /// What the depth passes draw, all of it in `records`. The first
    /// `opaque.len()` plain buckets are the plain instances (the ones a sun
    /// cascade culls); the rest are shader surfaces drawn through the
    /// built-in depth pipeline.
    pub(super) depth: DepthBuckets,
    /// The glass, which no pass here draws: (mesh, instance), so the sun map
    /// can still count what it was given.
    pub(super) glass: Vec<(usize, InstanceRaw)>,
    pub(super) ext: ExtFlags,
    /// What each `.flsl` binding's shader was when the plan was made.
    pub(super) class: Vec<Option<FlslClass>>,
}

impl ViewPlan {
    /// Sort `instances`, `flsl` and `skins` into one plan.
    pub(super) fn build(
        instances: &[DepthDraw],
        flsl: &[FlslDraw],
        skins: &[SkinDraw],
        ext: ExtFlags,
        class: &[Option<FlslClass>],
    ) -> ViewPlan {
        let class_of = |b: &FlslBindingId| class.get(b.0 as usize).copied().flatten();
        let mut records: Vec<InstanceRaw> = Vec::with_capacity(instances.len() + flsl.len() + skins.len());
        let bucketize = |want_opaque: bool, records: &mut Vec<InstanceRaw>| -> Vec<Bucket> {
            bucket_into(
                instances
                    .iter()
                    .filter(|(_, _, raw)| ext.solid(raw) && ext.opaque(raw) == want_opaque)
                    .map(|(id, tex, raw)| ((id.0 as usize, tex.map(|t| t.0)), raw)),
                records,
            )
            .into_iter()
            .map(|((mesh_idx, tex_key), start, count)| (mesh_idx, tex_key, start, count))
            .collect()
        };
        let opaque = bucketize(true, &mut records);
        let transparent = bucketize(false, &mut records);
        let flsl_opaque = bucket_flsl_by(flsl, skins, &mut records, |b, _| class_of(b).is_some_and(|c| c.opaque));
        let flsl_blended = bucket_flsl_by(flsl, skins, &mut records, |b, _| class_of(b).is_some_and(|c| !c.opaque));
        let skin_opaque = bucket_skins_by(skins, &mut records, |d| d.flsl.is_none() && ext.opaque(&d.instance));
        let skin_blended = bucket_skins_by(skins, &mut records, |d| d.flsl.is_none() && !ext.opaque(&d.instance));
        let glass =
            instances.iter().filter(|(_, _, raw)| !ext.solid(raw)).map(|(id, _, raw)| (id.0 as usize, *raw)).collect();

        // The depth passes: the opaque plain buckets, then the shader
        // surfaces that stay where their vertices are, through the built-in
        // depth pipelines; the ones that move draw through their own.
        let displaced_of = |bind: u32| class.get(bind as usize).copied().flatten().is_some_and(|c| c.displaced);
        let mut plain = opaque.clone();
        let mut skinned = skin_opaque.clone();
        let mut displaced = Vec::new();
        for &b @ (mesh, tex, bind, skin, start, count) in &flsl_opaque {
            if displaced_of(bind) {
                displaced.push(b);
            } else if skin {
                skinned.push((mesh, tex, start, count));
            } else {
                plain.push((mesh, tex, start, count));
            }
        }
        ViewPlan {
            uploaded: 0,
            records,
            opaque,
            transparent,
            flsl_opaque,
            flsl_blended,
            skin_opaque,
            skin_blended,
            depth: (plain, skinned, displaced),
            glass,
            ext,
            class: class.to_vec(),
        }
    }

    /// Records in the plan.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Instances the plan was made from, glass included.
    fn given(&self) -> usize {
        let plain = |b: &[Bucket]| b.iter().map(|&(_, _, _, c)| c as usize).sum::<usize>();
        plain(&self.opaque) + plain(&self.transparent) + self.glass.len()
    }

    /// One sun cascade's depth draws: the plan's plain opaque buckets with
    /// only the members `keep` passes, appended to `region` (whose first
    /// record sits at `base` in the instance buffer), then the shader and
    /// skinned surfaces where they lie in the plan, then the `extra` casters
    /// only the sun map draws, bucketed into `region` too. Returns the
    /// buckets and (drawn, given) over every instance, as the sun map always
    /// counted them: `keep` asked of each, opaque or not.
    pub(super) fn cull_depth(
        &self,
        keep: &dyn Fn(usize, &InstanceRaw) -> bool,
        extra: &[DepthDraw],
        region: &mut Vec<InstanceRaw>,
        base: u32,
    ) -> (DepthBuckets, (usize, usize)) {
        let (plain, skinned, displaced) = &self.depth;
        let mut buckets = Vec::with_capacity(plain.len());
        let mut drawn = 0usize;
        for &(mesh, tex, start, count) in &plain[..self.opaque.len()] {
            let from = region.len();
            region.extend(self.records[start as usize..(start + count) as usize].iter().filter(|r| keep(mesh, r)));
            let kept = (region.len() - from) as u32;
            drawn += kept as usize;
            if kept > 0 {
                buckets.push((mesh, tex, base + from as u32, kept));
            }
        }
        buckets.extend_from_slice(&plain[self.opaque.len()..]);
        // Counted only: what no depth pass draws.
        for &(mesh, _, start, count) in &self.transparent {
            drawn += self.records[start as usize..(start + count) as usize].iter().filter(|r| keep(mesh, r)).count();
        }
        drawn += self.glass.iter().filter(|(mesh, r)| keep(*mesh, r)).count();
        if !extra.is_empty() {
            let ext = &self.ext;
            let mut kept_extra = 0usize;
            let extra_buckets = bucket_into(
                extra
                    .iter()
                    .filter(|(id, _, raw)| {
                        let k = keep(id.0 as usize, raw);
                        kept_extra += k as usize;
                        k && ext.solid(raw) && ext.opaque(raw)
                    })
                    .map(|(id, tex, raw)| ((id.0 as usize, tex.map(|t| t.0)), raw)),
                region,
            );
            drawn += kept_extra;
            // `bucket_into` counts starts from the region's own start.
            buckets.extend(extra_buckets.into_iter().map(|((m, t), s, c)| (m, t, base + s, c)));
        }
        let given = self.given() + extra.len();
        ((buckets, skinned.clone(), displaced.clone()), (drawn, given))
    }
}

#[cfg(test)]
mod tests {
    //! The plan against copies of the bucketing the passes did before it:
    //! the colour pass draw for draw, in order; the depth passes the same
    //! set of draws.
    use super::*;

    /// The colour pass's lists as `draw_scene_with` made them before the
    /// plan: (records, opaque, transparent, flsl opaque, flsl blended,
    /// skinned opaque, skinned blended).
    #[allow(clippy::type_complexity)]
    fn old_colour(
        instances: &[DepthDraw],
        flsl: &[FlslDraw],
        skins: &[SkinDraw],
        ext: &ExtFlags,
        class: &[Option<FlslClass>],
        deferred: bool,
    ) -> (Vec<InstanceRaw>, Vec<Bucket>, Vec<Bucket>, Vec<FlslBucket>, Vec<FlslBucket>, Vec<Bucket>, Vec<Bucket>) {
        let mut raws: Vec<InstanceRaw> = Vec::with_capacity(instances.len());
        let dithers = &ext.dither;
        let blends = &ext.blend;
        let is_opaque = |raw: &InstanceRaw| {
            is_terrain(raw)
                || dithers.get(ext_index_of(raw) as usize).copied().unwrap_or(false)
                || (raw.color[3] >= OPAQUE_CUTOFF && !blends.get(ext_index_of(raw) as usize).copied().unwrap_or(false))
        };
        let glassy = &ext.glass;
        let solid = |raw: &InstanceRaw| !glassy.get(ext_index_of(raw) as usize).copied().unwrap_or(false);
        let bucketize = |want_opaque: bool, raws: &mut Vec<InstanceRaw>| -> Vec<Bucket> {
            bucket_into(
                instances
                    .iter()
                    .filter(|(_, _, raw)| solid(raw) && is_opaque(raw) == want_opaque)
                    .map(|(id, tex, raw)| ((id.0 as usize, tex.map(|t| t.0)), raw)),
                raws,
            )
            .into_iter()
            .map(|((mesh_idx, tex_key), start, count)| (mesh_idx, tex_key, start, count))
            .collect()
        };
        let opaque = bucketize(true, &mut raws);
        let transparent = bucketize(false, &mut raws);
        let sh = |b: &FlslBindingId| class.get(b.0 as usize).copied().flatten();
        let here = |c: FlslClass| !(deferred && c.reads_scene);
        let flsl_opaque = bucket_flsl_by(flsl, skins, &mut raws, |b, _| sh(b).is_some_and(|c| c.opaque && here(c)));
        let flsl_blended = bucket_flsl_by(flsl, skins, &mut raws, |b, _| sh(b).is_some_and(|c| !c.opaque && here(c)));
        let skin_opaque = bucket_skins_by(skins, &mut raws, |d| d.flsl.is_none() && is_opaque(&d.instance));
        let skin_blended = bucket_skins_by(skins, &mut raws, |d| d.flsl.is_none() && !is_opaque(&d.instance));
        (raws, opaque, transparent, flsl_opaque, flsl_blended, skin_opaque, skin_blended)
    }

    /// A depth pass's buckets as `bucket_opaque_depth_draws` made them
    /// before the plan, and (drawn, given) as the sun map counted them.
    fn old_depth(
        instances: &[DepthDraw],
        keep: &dyn Fn(usize, &InstanceRaw) -> bool,
        flsl: &[FlslDraw],
        skins: &[SkinDraw],
        ext: &ExtFlags,
        class: &[Option<FlslClass>],
        raws: &mut Vec<InstanceRaw>,
    ) -> (DepthBuckets, (usize, usize)) {
        let blended = |raw: &InstanceRaw| ext.blend.get(ext_index_of(raw) as usize).copied().unwrap_or(false);
        let dithered = |raw: &InstanceRaw| ext.dither.get(ext_index_of(raw) as usize).copied().unwrap_or(false);
        let sh = |b: &FlslBindingId| class.get(b.0 as usize).copied().flatten();
        let flsl_opaque = |bind: &FlslBindingId| sh(bind).is_some_and(|c| c.opaque && !c.displaced);
        let solid = |raw: &InstanceRaw| !ext.glass.get(ext_index_of(raw) as usize).copied().unwrap_or(false);
        let cast = std::cell::Cell::new(0usize);
        let buckets: Vec<Bucket> = bucket_into(
            instances
                .iter()
                .filter(|d| {
                    let raw = &d.2;
                    let k = keep(d.0.0 as usize, raw);
                    cast.set(cast.get() + k as usize);
                    k && solid(raw) && (is_terrain(raw) || dithered(raw) || (raw.color[3] >= OPAQUE_CUTOFF && !blended(raw)))
                })
                .map(|(id, tex, raw)| ((id.0 as usize, tex.map(|t| t.0)), raw))
                .chain(
                    flsl.iter()
                        .filter(|(_, _, bind, _)| flsl_opaque(bind))
                        .map(|(id, tex, _, raw)| ((id.0 as usize, tex.map(|t| t.0)), raw)),
                ),
            raws,
        )
        .into_iter()
        .map(|((mesh_idx, tex_key), start, count)| (mesh_idx, tex_key, start, count))
        .collect();
        let skin_buckets = bucket_skins_by(skins, raws, |d| match &d.flsl {
            None => {
                is_terrain(&d.instance) || dithered(&d.instance) || (d.instance.color[3] >= OPAQUE_CUTOFF && !blended(&d.instance))
            }
            Some(bind) => flsl_opaque(bind),
        });
        let displaced = bucket_flsl_by(flsl, skins, raws, |b, _| sh(b).is_some_and(|c| c.opaque && c.displaced));
        ((buckets, skin_buckets, displaced), (cast.get(), instances.len()))
    }

    /// Each draw of a plain list, in order: (mesh, texture, its records).
    fn plain_draws(b: &[Bucket], raws: &[InstanceRaw]) -> Vec<(usize, Option<u32>, Vec<[u8; 4]>)> {
        b.iter()
            .map(|&(m, t, s, c)| (m, t, raws[s as usize..(s + c) as usize].iter().map(fingerprint).collect()))
            .collect()
    }

    #[allow(clippy::type_complexity)]
    fn flsl_draws(b: &[FlslBucket], raws: &[InstanceRaw]) -> Vec<(usize, Option<u32>, u32, bool, Vec<[u8; 4]>)> {
        b.iter()
            .map(|&(m, t, bind, sk, s, c)| (m, t, bind, sk, raws[s as usize..(s + c) as usize].iter().map(fingerprint).collect()))
            .collect()
    }

    /// A record, by its bytes (all of them hashed into four).
    fn fingerprint(r: &InstanceRaw) -> [u8; 4] {
        let h = bytemuck::bytes_of(r).iter().fold(0x811c_9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193));
        h.to_le_bytes()
    }

    /// Every single-instance draw a depth pass makes, sorted: (which list,
    /// mesh, texture, binding, the record). `base` is where `raws` starts in
    /// the instance buffer; `plan` holds records below it.
    #[allow(clippy::type_complexity)]
    fn depth_set(
        (plain, skinned, displaced): &DepthBuckets,
        plan: &[InstanceRaw],
        region: &[InstanceRaw],
        base: u32,
    ) -> Vec<(u8, usize, Option<u32>, u32, bool, [u8; 4])> {
        let at = |i: u32| if i >= base { region[(i - base) as usize] } else { plan[i as usize] };
        let mut out = Vec::new();
        for &(m, t, s, c) in plain {
            out.extend((s..s + c).map(|i| (0, m, t, 0, false, fingerprint(&at(i)))));
        }
        for &(m, t, s, c) in skinned {
            out.extend((s..s + c).map(|i| (1, m, t, 0, false, fingerprint(&at(i)))));
        }
        for &(m, t, bind, sk, s, c) in displaced {
            out.extend((s..s + c).map(|i| (2, m, t, bind, sk, fingerprint(&at(i)))));
        }
        out.sort();
        out
    }

    /// A small deterministic generator.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn below(&mut self, n: u32) -> u32 {
            self.next() % n
        }
    }

    /// Surface extras: 0 neutral, 1 dithered, 2 blended, 3 glass, 4 no-cast,
    /// 5 swaying.
    fn extras() -> ExtFlags {
        let mut lanes = vec![[0.0f32; 4]; 6 * EXT_LANES];
        lanes[EXT_LANES + 1][0] = EXT_DITHER_ALPHA as f32;
        lanes[2 * EXT_LANES + 1][0] = EXT_BLEND as f32;
        lanes[3 * EXT_LANES + 1] = [EXT_PHYSICAL as f32, 0.0, 0.0, 0.8];
        lanes[4 * EXT_LANES + 1][0] = EXT_NO_CAST as f32;
        lanes[5 * EXT_LANES + 2][2] = 0.7;
        ExtFlags::of(&lanes)
    }

    /// Shader bindings 0..6: opaque, blended, opaque reading the scene,
    /// blended reading the scene, opaque moving its vertices, and one with
    /// no shader at all.
    fn classes() -> Vec<Option<FlslClass>> {
        vec![
            Some(FlslClass { opaque: true, displaced: false, reads_scene: false }),
            Some(FlslClass { opaque: false, displaced: false, reads_scene: false }),
            Some(FlslClass { opaque: true, displaced: false, reads_scene: true }),
            Some(FlslClass { opaque: false, displaced: false, reads_scene: true }),
            Some(FlslClass { opaque: true, displaced: true, reads_scene: false }),
            None,
        ]
    }

    fn raw(rng: &mut Rng, i: usize) -> InstanceRaw {
        let mut r = instance_of(Mat4::from_translation(glam::Vec3::new(i as f32, rng.below(100) as f32, 0.0)), [0.5; 3]);
        match rng.below(10) {
            0 => r.color[3] = 0.4,
            1 => r.color[3] = 0.999,
            _ => {}
        }
        if rng.below(6) == 0 {
            // Terrain, faded or not.
            r.normal_mat[2][3] = 1.0;
        }
        set_ext_index(&mut r, rng.below(7).min(5));
        r
    }

    #[allow(clippy::type_complexity)]
    fn mixed(seed: u64, n: usize) -> (Vec<DepthDraw>, Vec<FlslDraw>, Vec<SkinDraw>, Vec<DepthDraw>) {
        let mut rng = Rng(seed);
        let tex = |rng: &mut Rng| (rng.below(3) == 0).then(|| TexId(rng.below(4)));
        // Few meshes, so buckets hold several members and runs break.
        let instances =
            (0..n).map(|i| (MeshId(rng.below(40)), tex(&mut rng), raw(&mut rng, i))).collect::<Vec<_>>();
        let flsl = (0..n / 8)
            .map(|i| (MeshId(rng.below(40)), tex(&mut rng), FlslBindingId(rng.below(6)), raw(&mut rng, 10_000 + i)))
            .collect();
        let skins = (0..n / 10)
            .map(|i| SkinDraw {
                mesh: MeshId(rng.below(40)),
                tex: tex(&mut rng),
                instance: raw(&mut rng, 20_000 + i),
                pose: rng.below(5),
                flsl: (rng.below(3) == 0).then(|| FlslBindingId(rng.below(6))),
            })
            .collect();
        let extra = (0..n / 20).map(|i| (MeshId(rng.below(40)), tex(&mut rng), raw(&mut rng, 30_000 + i))).collect();
        (instances, flsl, skins, extra)
    }

    #[test]
    fn the_colour_pass_draws_what_it_drew_before_in_the_same_order() {
        for seed in 0..40 {
            let (instances, flsl, skins, _) = mixed(seed, 300);
            let ext = extras();
            let class = classes();
            let plan = ViewPlan::build(&instances, &flsl, &skins, ext.clone(), &class);
            for deferred in [false, true] {
                let (raws, o, t, fo, fb, so, sb) = old_colour(&instances, &flsl, &skins, &ext, &class, deferred);
                let r = &plan.records;
                assert_eq!(plain_draws(&plan.opaque, r), plain_draws(&o, &raws), "seed {seed}: opaque buckets");
                assert_eq!(plain_draws(&plan.transparent, r), plain_draws(&t, &raws), "seed {seed}: transparent buckets");
                assert_eq!(plain_draws(&plan.skin_opaque, r), plain_draws(&so, &raws), "seed {seed}: skinned opaque");
                assert_eq!(plain_draws(&plan.skin_blended, r), plain_draws(&sb, &raws), "seed {seed}: skinned blended");
                // The pass leaves out the shaders waiting for the capture
                // (`draw_scene_planned` filters by the same rule).
                let here = |b: &&FlslBucket| class[b.2 as usize].is_some_and(|c| !(deferred && c.reads_scene));
                let fo_new: Vec<FlslBucket> = plan.flsl_opaque.iter().filter(here).copied().collect();
                let fb_new: Vec<FlslBucket> = plan.flsl_blended.iter().filter(here).copied().collect();
                assert_eq!(flsl_draws(&fo_new, r), flsl_draws(&fo, &raws), "seed {seed}: shader opaque");
                assert_eq!(flsl_draws(&fb_new, r), flsl_draws(&fb, &raws), "seed {seed}: shader blended");
            }
        }
    }

    #[test]
    fn the_depth_passes_draw_the_set_they_drew_before_and_count_the_same() {
        for seed in 0..40 {
            let (instances, flsl, skins, extra) = mixed(seed, 300);
            let ext = extras();
            let class = classes();
            let plan = ViewPlan::build(&instances, &flsl, &skins, ext.clone(), &class);

            // The prepass: everything, straight from the plan.
            let mut raws = Vec::new();
            let (old, _) = old_depth(&instances, &|_, _| true, &flsl, &skins, &ext, &class, &mut raws);
            assert_eq!(depth_set(&plan.depth, &plan.records, &[], u32::MAX), depth_set(&old, &raws, &[], u32::MAX), "seed {seed}: prepass");

            // A sun cascade: the casters its cull keeps, the extra casters
            // with them, as the old pass saw them all in one list.
            let mut all = instances.clone();
            all.extend_from_slice(&extra);
            for cut in [0u32, 3, 7] {
                let keep = |mesh: usize, r: &InstanceRaw| {
                    ext.casts(r) && (cut == 0 || !(mesh as u32 + r.model[3][1] as u32).is_multiple_of(cut))
                };
                let mut raws = Vec::new();
                let (old, old_n) = old_depth(&all, &keep, &flsl, &skins, &ext, &class, &mut raws);
                let base = plan.len() as u32 + 17;
                let mut region = Vec::new();
                let (new, new_n) = plan.cull_depth(&keep, &extra, &mut region, base);
                assert_eq!(new_n, old_n, "seed {seed} cut {cut}: (drawn, given)");
                assert_eq!(
                    depth_set(&new, &plan.records, &region, base),
                    depth_set(&old, &raws, &[], u32::MAX),
                    "seed {seed} cut {cut}: sun cascade"
                );
                // Only the culled plain records are uploaded again.
                let plain_kept: usize = plan.opaque.iter().map(|&(m, _, s, c)| {
                    plan.records[s as usize..(s + c) as usize].iter().filter(|r| keep(m, r)).count()
                }).sum();
                assert!(region.len() <= plain_kept + extra.len(), "seed {seed}: the cascade uploaded {} records", region.len());
            }
        }
    }
}
