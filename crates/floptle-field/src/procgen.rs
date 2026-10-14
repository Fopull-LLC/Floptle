//! Generic procedural planet fill — the native backend of the Lua
//! `terrain.generatePlanet(id, opts)` API.
//!
//! Deliberately game-agnostic: what to build (solar systems, archetypes,
//! orbits, names) is game-side scripting; this module is only the heavy
//! per-voxel primitive a script can't afford to run itself — a layered,
//! cavernous, cratered sphere written into a sparse [`ChunkField`], with
//! every knob exposed on [`PlanetFill`].
//!
//! The recipe (proven on the solar demo's planetoid): sphere ± fbm-of-direction
//! relief; caves where two noise fields are both near zero (galleries that
//! widen with depth + a larger-scale chamber system), guarded by a crust
//! band, a depth cap and a solid core; impact craters subtracted in the fill
//! (carving after hits the band clamp and terraces); materials by depth layer
//! (surface biome patches → subsoil → strata → deep cave rock) with optional
//! glowing pockets, thin glowing seams and polar ice.

use floptle_core::math::Vec3;
use floptle_core::noise::{FbmCache, Noise, Rng};

use crate::ChunkField;

/// One material layer: a terrain palette slot (1-based) + an RGB tint
/// (`albedo = texture × tint`).
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LayerPaint {
    pub slot: u8,
    pub color: [f32; 3],
}

/// Sparse glowing pockets (crystal/ore): painted where a dedicated fbm field
/// exceeds `threshold`, below `min_depth`. Use a palette glow slot to make
/// them self-lit; higher thresholds = rarer.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct GlowPockets {
    pub paint: LayerPaint,
    pub threshold: f32,
    pub min_depth: f32,
}

/// Thin vein seams (magma/ore filaments): painted where a seam noise field
/// sits within `width` of `center` — a band, so seams read as veins running
/// through the rock instead of flooding whole walls.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SeamSpec {
    pub paint: LayerPaint,
    pub min_depth: f32,
    pub center: f32,
    pub width: f32,
}

/// Everything `generate_planet` needs — every field has a workable default,
/// so callers (the Lua table) override only what they care about.
/// Serializable: the RON form is the on-node "genspec" that lets a body
/// generate on-demand when first approached (G2 galaxy streaming) — new
/// fields must keep serde defaults so old genspec strings stay loadable.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PlanetFill {
    pub seed: u32,
    pub radius: f32,
    /// Voxel size (world units) — smaller = finer + heavier.
    pub voxel: f32,
    /// Peak surface displacement.
    pub relief: f32,
    /// Relief noise frequency (dunes ripple high, ice rolls low).
    pub bump_freq: f32,
    /// Cave zone thickness below the surface (0 = solid body).
    pub cave_depth: f32,
    /// Solid core ball radius the caves never touch (0 = none).
    pub core_r: f32,
    /// Molten zone paint around the core (slot 0 = none).
    pub core_paint: LayerPaint,
    /// Impact crater count (0 = none) + dent radii as fractions of `radius`.
    pub craters: u32,
    pub crater_min: f32,
    pub crater_max: f32,
    pub crater_dust: LayerPaint,
    /// Surface biomes: `patch + alt·patch_bias > patch_thr` → `surface_a`.
    pub surface_a: LayerPaint,
    pub surface_b: LayerPaint,
    pub patch_bias: f32,
    pub patch_thr: f32,
    /// Depth layers (dig walls read as geology).
    pub subsoil: LayerPaint,
    pub subsoil_depth: f32,
    pub strata: LayerPaint,
    pub strata_depth: f32,
    /// Deep cave-zone wall rock (below half the cave depth).
    pub deep: LayerPaint,
    pub pockets: Option<GlowPockets>,
    pub seam: Option<SeamSpec>,
    /// Polar caps + frost patches: `|dir.y| > lat` (or patch noise < -0.42).
    pub ice_caps: Option<(f32, LayerPaint)>,
    /// Mountain ranges: ridged peaks where a slow mask is high.
    pub mountains: Option<Mountains>,
    /// Bends the relief before it is read, into folded and wind-carved forms.
    pub warp: Option<Warp>,
    /// Flattens every peak above a height into a table top.
    pub plateau: Option<Plateau>,
    /// Steps the ground into terraces and mesas.
    pub terrace: Option<Terrace>,
    /// Real overhangs, arches and undercut cliffs: 3D noise in a shell round
    /// the surface.
    pub overhangs: Option<Overhangs>,
    /// Steep-walled ravines and canyons, cut along meandering lines.
    pub ravines: Option<Ravines>,
    /// Rivers from the highlands down to the sea, each in a valley it carves.
    pub rivers: Option<Rivers>,
    /// Surface materials by latitude, altitude, slope, moisture and nearness
    /// to a river: the first rule that matches paints the ground.
    pub biomes: Vec<Biome>,
    /// The size of the moisture pattern biomes read (higher is finer).
    pub moisture_scale: f32,
}

/// Mountain ranges: `height` at the crests, ranges `coverage` (0..1) of the
/// world, ridges `scale` across the sphere, crests sharper as `sharpness` rises.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Mountains {
    pub height: f32,
    pub scale: f32,
    pub sharpness: f32,
    pub coverage: f32,
    pub octaves: u32,
}

impl Default for Mountains {
    fn default() -> Self {
        Mountains { height: 40.0, scale: 3.0, sharpness: 2.0, coverage: 0.4, octaves: 5 }
    }
}

/// Domain warp: the relief is read at a point pushed up to `strength` (a
/// fraction of the radius) by noise `scale` across the sphere.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Warp {
    pub strength: f32,
    pub scale: f32,
}

/// Peaks above `height` are rounded off into flat tops over `smooth` metres.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Plateau {
    pub height: f32,
    pub smooth: f32,
}

impl Default for Plateau {
    fn default() -> Self {
        Plateau { height: 20.0, smooth: 3.0 }
    }
}

/// Terraces `step` metres tall; `sharpness` 0 is a gentle slope between them,
/// 1 a sheer rise.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Terrace {
    pub step: f32,
    pub sharpness: f32,
}

impl Default for Terrace {
    fn default() -> Self {
        Terrace { step: 6.0, sharpness: 0.6 }
    }
}

/// Overhangs: the ground within `depth` metres of the surface is pushed in and
/// out by up to `strength` metres of 3D noise `scale` per metre, so a cliff can
/// lean out over its own foot.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Overhangs {
    pub depth: f32,
    pub scale: f32,
    pub strength: f32,
}

impl Default for Overhangs {
    fn default() -> Self {
        Overhangs { depth: 12.0, scale: 0.05, strength: 4.0 }
    }
}

/// `count` ravines `length` metres long, `width` across, `depth` deep, wandering
/// by `meander`.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Ravines {
    pub count: u32,
    pub depth: f32,
    pub width: f32,
    pub length: f32,
    pub meander: f32,
}

impl Default for Ravines {
    fn default() -> Self {
        Ravines { count: 4, depth: 20.0, width: 14.0, length: 400.0, meander: 1.0 }
    }
}

/// `count` rivers from the highest ground down to `sea_level` (metres above
/// the radius), carving a bed `width` across and `depth` deep in a valley four
/// times as wide.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Rivers {
    pub count: u32,
    pub depth: f32,
    pub width: f32,
    pub sea_level: f32,
    pub meander: f32,
}

impl Default for Rivers {
    fn default() -> Self {
        Rivers { count: 6, depth: 4.0, width: 8.0, sea_level: 0.0, meander: 0.6 }
    }
}

/// One surface rule: ground matching every range given is painted `paint`.
/// Ranges are `[min, max]`; one left out does not constrain.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Biome {
    pub paint: LayerPaint,
    /// Degrees from the equator, north or south.
    pub latitude: Option<[f32; 2]>,
    /// Metres above the radius.
    pub altitude: Option<[f32; 2]>,
    /// Degrees from level.
    pub slope: Option<[f32; 2]>,
    /// The moisture pattern, -1 (dry) to 1 (wet).
    pub moisture: Option<[f32; 2]>,
    /// Within this many metres of a river.
    pub near_river: Option<f32>,
}

impl Default for PlanetFill {
    fn default() -> Self {
        PlanetFill {
            seed: 7,
            radius: 120.0,
            voxel: 1.5,
            relief: 8.0,
            bump_freq: 4.3,
            cave_depth: 40.0,
            core_r: 8.0,
            core_paint: LayerPaint { slot: 0, color: [0.98, 0.82, 0.6] },
            craters: 0,
            crater_min: 0.12,
            crater_max: 0.26,
            crater_dust: LayerPaint { slot: 11, color: [0.78, 0.74, 0.66] },
            surface_a: LayerPaint { slot: 1, color: [0.72, 0.62, 0.58] },
            surface_b: LayerPaint { slot: 2, color: [0.66, 0.6, 0.74] },
            patch_bias: 0.45,
            patch_thr: 0.08,
            subsoil: LayerPaint { slot: 3, color: [0.6, 0.58, 0.68] },
            subsoil_depth: 2.4,
            strata: LayerPaint { slot: 4, color: [0.85, 0.78, 0.66] },
            strata_depth: 9.0,
            deep: LayerPaint { slot: 5, color: [0.78, 0.68, 0.58] },
            pockets: None,
            seam: None,
            ice_caps: None,
            mountains: None,
            warp: None,
            plateau: None,
            terrace: None,
            overhangs: None,
            ravines: None,
            rivers: None,
            biomes: Vec::new(),
            moisture_scale: 1.5,
        }
    }
}

fn tint(base: [f32; 3], vary: f32) -> [u8; 3] {
    let v = 0.85 + 0.25 * vary.clamp(-1.0, 1.0);
    // 12-step quantization: smooth per-voxel tints destroy the cfield's
    // color RLE (measured 28 → 84 MB); steps keep neighbours byte-equal.
    let f = |c: f32| (((c * v).clamp(0.0, 1.0) * 255.0 / 12.0).round() * 12.0) as u8;
    [f(base[0]), f(base[1]), f(base[2])]
}

fn rgba(rgb: [u8; 3], slot: u8) -> [u8; 4] {
    [rgb[0], rgb[1], rgb[2], slot]
}

/// Smooth minimum: `min(a, b)` with the corner rounded over about `k`. Never
/// above `min(a, b)`.
fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    b + (a - b) * h - k * h * (1.0 - h)
}

/// Smooth maximum: `max(a, b)` with the corner rounded over about `k`. Never
/// below `max(a, b)`.
fn smax(a: f32, b: f32, k: f32) -> f32 {
    -smin(-a, -b, k)
}

/// One [`FbmCache`] for each noise a planet's voxel reads: consecutive voxels
/// of a block mostly share lattice cells, so each call site keeps its own.
#[derive(Default)]
pub(crate) struct Caches {
    bump: FbmCache,
    a: FbmCache,
    b: FbmCache,
    c1: FbmCache,
    c2: FbmCache,
    detail: FbmCache,
    vary: FbmCache,
    patch: FbmCache,
    pocket: FbmCache,
    seam: FbmCache,
}

/// A planet's shape before it is voxelized: the ground, the craters and the
/// caves, as a signed distance (negative is rock).
pub(crate) struct PlanetShape {
    noise: Noise,
    radius: f32,
    relief: f32,
    cave_depth: f32,
    core_r: f32,
    bump_freq: f32,
    voxel: f32,
    craters: Vec<(Vec3, f32)>,
    /// The precomputed ground, for a world with relief layers.
    ground: Option<std::sync::Arc<crate::relief::Relief>>,
    overhangs: Option<Overhangs>,
}

impl PlanetShape {
    /// The ground alone: negative under it.
    #[cfg(test)]
    pub(crate) fn body(&self, p: Vec3) -> f32 {
        self.body_with(p, self.bump(p, &mut FbmCache::default()))
    }

    /// How far the ground is raised above `radius` in `p`'s direction.
    fn bump(&self, p: Vec3, cache: &mut FbmCache) -> f32 {
        let r = p.length();
        let dir = if r > 1e-3 { p / r } else { Vec3::X };
        match &self.ground {
            Some(relief) => relief.height(dir),
            None => self.noise.fbm_cached(dir * self.bump_freq, 5, cache) * self.relief,
        }
    }

    /// [`Self::body`] with the bump already known.
    fn body_with(&self, p: Vec3, bump: f32) -> f32 {
        let mut body = p.length() - (self.radius + bump);
        if let Some(o) = &self.overhangs
            && o.strength != 0.0
        {
            // Strongest at the surface, gone `depth` either side of it.
            let shell = 1.0 - (body.abs() / o.depth.max(0.1)).min(1.0);
            if shell > 0.0 {
                let s = shell * shell * (3.0 - 2.0 * shell);
                body += o.strength * s * self.noise.fbm(p * o.scale + Vec3::splat(431.0), 3);
            }
        }
        for (c, cr) in &self.craters {
            body = body.max(-((p - *c).length() - cr));
        }
        body
    }

    /// The lowest and highest the ground stands above the radius anywhere.
    fn height_range(&self) -> (f32, f32) {
        match &self.ground {
            Some(r) => r.range(),
            None => (-self.relief.abs(), self.relief.abs()),
        }
    }

    /// How far the overhang layer can move the ground either way.
    fn overhang_reach(&self) -> f32 {
        self.overhangs.map_or(0.0, |o| o.strength.abs())
    }

    /// The whole shape, caves included.
    ///
    /// Caves are where two noise fields are both near zero. Their cross-section
    /// is round (the length of the pair, not the larger of the two, which made
    /// a rhombus of four flat walls and four creases), galleries widen with
    /// depth, a larger-scale chamber field opens rooms further down, and a
    /// solid core survives at the centre. Every join is smooth, so tunnels meet
    /// rooms and the ceiling in rounded junctions, and a metre of noise at wall
    /// scale keeps a wall from being a plane over tens of metres. The crust
    /// gate is a smooth maximum, never below the hard one: no cave comes within
    /// three voxels of the surface.
    #[cfg(test)]
    pub(crate) fn d(&self, p: Vec3) -> f32 {
        self.d_with(p, self.body(p), &mut Caches::default())
    }

    /// [`Self::d`] with the ground already known.
    fn d_with(&self, p: Vec3, body: f32, k: &mut Caches) -> f32 {
        // Within a voxel and a half of the surface, or above it, the crust gate
        // alone keeps every cave out (`gated ≥ 3·voxel + body ≥ −body`), so
        // the answer is the ground's and the cave noise need not be asked.
        if self.cave_depth <= 0.0 || body >= -1.5 * self.voxel + 0.01 {
            return body;
        }
        let noise = &self.noise;
        let r = p.length();
        let a = noise.fbm_cached(p * 0.018, 3, &mut k.a);
        let b = noise.fbm_cached(p * 0.018 + Vec3::splat(51.7), 3, &mut k.b);
        let w = 0.1 + (-body * 0.0006).clamp(0.0, 0.07);
        let tunnel = (a.hypot(b) - w) * 34.0;
        let c1 = noise.fbm_cached(p * 0.008 + Vec3::splat(113.0), 2, &mut k.c1);
        let c2 = noise.fbm_cached(p * 0.008 + Vec3::splat(7.9), 2, &mut k.c2);
        let chamber = (c1.hypot(c2) - 0.14) * 70.0;
        let ceiling = body + self.cave_depth * 0.42;
        let detail = noise.fbm_cached(p * 0.07 + Vec3::splat(3.3), 3, &mut k.detail) * 1.2;
        let cave = smin(tunnel, smax(chamber, ceiling, 6.0), 4.0) + detail;
        // The crust and floor joins narrow with a shallow cave zone. At their
        // full widths the two met in the middle of a moon's few metres of cave
        // and flattened every cave in it; a zone 20 m deep or more keeps them.
        let k = (self.cave_depth / 20.0).min(1.0);
        let gated = smax(smax(cave, self.voxel * 3.0 + body, 2.0 * k), -(body + self.cave_depth), 4.0 * k)
            .max((self.core_r + self.voxel * 6.0) - r);
        body.max(-gated)
    }
}

/// Fill one planet into a fresh sparse field. Deterministic in `spec`.
///
/// Only the shell the ground and the caves live in is evaluated voxel by
/// voxel, shared out across cores; see [`ChunkField::generate`] and
/// [`Planet::classify`]. The field is the one a voxel-by-voxel fill of the
/// whole cube writes, voxel for voxel.
pub fn generate_planet(spec: &PlanetFill) -> ChunkField {
    let planet = Planet::new(spec);
    let ext = planet.extent();
    ChunkField::generate(spec.voxel, Vec3::splat(-ext), Vec3::splat(ext), &planet)
}

/// A planet ready to voxelize: its shape and its paint.
pub(crate) struct Planet {
    shape: PlanetShape,
    spec: PlanetFill,
}

impl Planet {
    pub(crate) fn new(spec: &PlanetFill) -> Self {
        let mut rng = Rng::new(spec.seed.wrapping_add(31));
        let (radius, relief) = (spec.radius, spec.relief);
        // Impact craters: spherical dents subtracted in the fill SDF.
        let craters: Vec<(Vec3, f32)> = (0..spec.craters)
            .map(|_| {
                let d = Vec3::new(
                    rng.range(-1.0, 1.0) as f32,
                    rng.range(-1.0, 1.0) as f32,
                    rng.range(-1.0, 1.0) as f32,
                )
                .normalize_or_zero();
                let d = if d == Vec3::ZERO { Vec3::X } else { d };
                let r = rng.range(spec.crater_min as f64, spec.crater_max as f64) as f32 * radius;
                (d * (radius + relief * 0.3), r)
            })
            .collect();
        let shape = PlanetShape {
            noise: Noise::new(spec.seed),
            radius,
            relief,
            cave_depth: spec.cave_depth,
            core_r: spec.core_r,
            bump_freq: spec.bump_freq,
            voxel: spec.voxel.clamp(0.25, 16.0),
            craters,
            ground: crate::relief::Relief::wanted(spec).then(|| std::sync::Arc::new(crate::relief::Relief::new(spec))),
            overhangs: spec.overhangs,
        };
        Planet { shape, spec: spec.clone() }
    }

    /// How far from the centre the ground can reach: the radius, the highest
    /// the relief layers raise it, and what overhangs can push out, with a
    /// little to spare. The cube a fill walks.
    pub(crate) fn extent(&self) -> f32 {
        let s = &self.shape;
        s.radius + s.height_range().1.max(s.relief.abs()) + s.overhang_reach() + 2.0
    }

    /// The colour a voxel at `p` is painted, the ground there raised `bump`.
    fn paint(&self, p: Vec3, bump: f32, k: &mut Caches) -> [u8; 4] {
        let (spec, noise) = (&self.spec, &self.shape.noise);
        let (radius, relief, cave_depth, core_r) =
            (self.shape.radius, self.shape.relief, self.shape.cave_depth, self.shape.core_r);
        let r = p.length();
        let dir = if r > 1e-3 { p / r } else { Vec3::X };
        let alt = (r - radius) / relief.max(1.0);
        let depth = (radius + bump) - r;
        let vary = noise.fbm_cached(p * 0.05 + Vec3::splat(83.0), 2, &mut k.vary);
        let patch = noise.fbm_cached(dir * 9.0 + Vec3::splat(37.0), 3, &mut k.patch);

        // Molten core zone (usually a glow slot): a deep dig reads hot
        // before the core itself appears.
        if spec.core_paint.slot != 0 && cave_depth > 0.0 && r < core_r + core_r.min(10.0) {
            return rgba(tint(spec.core_paint.color, vary), spec.core_paint.slot);
        }
        // Sparse glowing pockets — the reason to dig.
        if let Some(pk) = &spec.pockets {
            let pocket = noise.fbm_cached(p * 0.05 + Vec3::splat(17.3), 3, &mut k.pocket);
            if depth > pk.min_depth && pocket > pk.threshold {
                return rgba(tint(pk.paint.color, vary), pk.paint.slot);
            }
        }
        // Vein seams: a thin band of the seam field — filaments through
        // the rock, not glowing walls.
        if let Some(sm) = &spec.seam
            && depth > sm.min_depth
            && (noise.fbm_cached(p * 0.035 + Vec3::splat(5.9), 3, &mut k.seam) - sm.center).abs() < sm.width
        {
            return rgba(tint(sm.paint.color, vary), sm.paint.slot);
        }
        // Depth layers: deep cave walls → strata → subsoil.
        if depth > cave_depth * 0.5 {
            return rgba(tint(spec.deep.color, vary), spec.deep.slot);
        }
        if depth > spec.strata_depth {
            return rgba(tint(spec.strata.color, vary), spec.strata.slot);
        }
        if depth > spec.subsoil_depth {
            return rgba(tint(spec.subsoil.color, vary), spec.subsoil.slot);
        }
        // Biome rules, for the ground that is not a deeper layer.
        if !spec.biomes.is_empty()
            && let Some(relief) = &self.shape.ground
        {
            let lat = dir.y.abs().clamp(0.0, 1.0).asin().to_degrees();
            let within = |range: Option<[f32; 2]>, v: f32| range.is_none_or(|[lo, hi]| v >= lo && v <= hi);
            let mut slope = None;
            let mut moisture = None;
            for b in &spec.biomes {
                if !within(b.latitude, lat) || !within(b.altitude, bump) {
                    continue;
                }
                if b.slope.is_some() && !within(b.slope, *slope.get_or_insert_with(|| relief.slope_degrees(dir, radius))) {
                    continue;
                }
                if b.moisture.is_some()
                    && !within(
                        b.moisture,
                        *moisture.get_or_insert_with(|| noise.fbm(dir * spec.moisture_scale + Vec3::splat(913.0), 3)),
                    )
                {
                    continue;
                }
                if b.near_river.is_some_and(|m| relief.river_distance(dir) > m) {
                    continue;
                }
                return rgba(tint(b.paint.color, vary), b.paint.slot);
            }
        }
        // Crater floors wear dust.
        for (c, cr) in &self.shape.craters {
            if (p - *c).length() < cr + 1.6 {
                return rgba(tint(spec.crater_dust.color, vary), spec.crater_dust.slot);
            }
        }
        // Polar caps + frost patches.
        if let Some((lat, ice)) = &spec.ice_caps
            && (dir.y.abs() > *lat || patch < -0.42)
        {
            return rgba(tint(ice.color, vary), ice.slot);
        }
        // Surface biomes.
        if patch + alt * spec.patch_bias > spec.patch_thr {
            rgba(tint(spec.surface_a.color, vary), spec.surface_a.slot)
        } else {
            rgba(tint(spec.surface_b.color, vary), spec.surface_b.slot)
        }
    }

    /// Bounds on the ground's raise, [`PlanetShape::bump`], over a box.
    ///
    /// The directions a box covers lie in the box of each `p / |p|` component's
    /// extremes, which `fbm_bounds` can bound the relief noise over.
    fn bump_bounds(&self, lo: Vec3, hi: Vec3, r_min: f32, r_max: f32) -> (f32, f32) {
        let s = &self.shape;
        if r_min <= 1e-3 {
            return (-s.relief, s.relief);
        }
        let axis = |l: f32, h: f32| {
            let (a, b) = if l >= 0.0 {
                (l / r_max, h / r_min)
            } else if h <= 0.0 {
                (l / r_min, h / r_max)
            } else {
                (l / r_min, h / r_min)
            };
            (a.max(-1.0), b.min(1.0))
        };
        let (x, y, z) = (axis(lo.x, hi.x), axis(lo.y, hi.y), axis(lo.z, hi.z));
        let dl = Vec3::new(x.0, y.0, z.0) * s.bump_freq;
        let dh = Vec3::new(x.1, y.1, z.1) * s.bump_freq;
        let (bl, bh) = s.noise.fbm_bounds(dl.min(dh), dl.max(dh), 5);
        let (a, b) = (bl.max(-1.0) * s.relief, bh.min(1.0) * s.relief);
        (a.min(b), a.max(b))
    }

    /// A lower bound on the cave field, the `cave` term of [`PlanetShape::d`],
    /// over a box whose ground (`body`) is at least `body_lo` everywhere.
    fn cave_lower_bound(&self, lo: Vec3, hi: Vec3, body_lo: f32) -> f32 {
        let s = &self.shape;
        let n = &s.noise;
        let span = |f: f32, off: f32, oct: u32| {
            n.fbm_bounds(lo * f + Vec3::splat(off), hi * f + Vec3::splat(off), oct)
        };
        // The nearest an interval comes to zero.
        let near0 = |(l, h): (f32, f32)| if l > 0.0 { l } else if h < 0.0 { -h } else { 0.0 };
        let (a, b) = (span(0.018, 0.0, 3), span(0.018, 51.7, 3));
        // Galleries widen with depth, so the widest `w` in the box is at its
        // deepest, `body_lo`.
        let w = 0.1 + (-body_lo * 0.0006).clamp(0.0, 0.07);
        let tunnel = (near0(a).hypot(near0(b)) - w) * 34.0;
        let (c1, c2) = (span(0.008, 113.0, 2), span(0.008, 7.9, 2));
        let chamber = (near0(c1).hypot(near0(c2)) - 0.14) * 70.0;
        // The ceiling only raises the cave (smax ≥ max), smin(a, b, k) ≥
        // min(a, b) − k/4, and the wall detail is at least −1.2.
        tunnel.min(chamber) - 1.0 - 1.2
    }
}

impl crate::VoxelSource for Planet {
    /// Open air: the box is above the highest ground its directions can have
    /// (craters only lower ground). Deep solid: below the lowest ground less
    /// the cave zone, or inside the cave zone where neither the tunnels nor
    /// the chambers can reach. Anything else is evaluated.
    fn classify(&self, lo: Vec3, hi: Vec3, band: f32) -> crate::BlockClass {
        use crate::BlockClass;
        let s = &self.shape;
        let margin = s.voxel;
        let near = Vec3::ZERO.clamp(lo, hi);
        let far = lo.abs().max(hi.abs());
        let (r_min, r_max) = (near.length(), far.length());
        // Overhangs move the ground either way by up to their strength.
        let lean = s.overhang_reach();
        if r_min - (s.radius + s.height_range().1) - lean >= band + margin {
            return BlockClass::Air;
        }
        let (bump_lo, bump_hi) = match &s.ground {
            Some(relief) => {
                let corners: [Vec3; 8] = std::array::from_fn(|k| {
                    Vec3::new(
                        if k & 1 == 0 { lo.x } else { hi.x },
                        if k & 2 == 0 { lo.y } else { hi.y },
                        if k & 4 == 0 { lo.z } else { hi.z },
                    )
                });
                relief.height_bounds(&corners)
            }
            None => self.bump_bounds(lo, hi, r_min, r_max),
        };
        if r_min - (s.radius + bump_hi) - lean >= band + margin {
            return BlockClass::Air;
        }
        // The highest the ground can be in the box: the plain body, or a
        // crater's dent, which raises it (a crater is air).
        let mut body_hi = r_max - (s.radius + bump_lo) + lean;
        for (c, cr) in &s.craters {
            let q = c.clamp(lo, hi);
            body_hi = body_hi.max(cr - (*c - q).length());
        }
        if s.cave_depth <= 0.0 {
            return if body_hi <= -(band + margin) { BlockClass::Solid } else { BlockClass::Mixed };
        }
        // Below the cave zone: the depth gate alone keeps it solid.
        if body_hi <= -(s.cave_depth + band + margin) {
            return BlockClass::Solid;
        }
        // In the cave zone, under the crust: solid if no cave reaches the box.
        let body_lo = r_min - (s.radius + bump_hi) - lean;
        if body_hi <= -(band + margin) && self.cave_lower_bound(lo, hi, body_lo) >= band + margin {
            return BlockClass::Solid;
        }
        BlockClass::Mixed
    }

    type Scratch = Caches;

    fn sample(&self, p: Vec3, band: f32, k: &mut Caches) -> Option<(f32, [u8; 4])> {
        let bump = self.shape.bump(p, &mut k.bump);
        let body = self.shape.body_with(p, bump);
        if body >= band {
            return None; // caves only ever open air: `d ≥ body`
        }
        let d = self.shape.d_with(p, body, k);
        (d < band).then(|| (d, self.paint(p, bump, k)))
    }

    fn solid_color(&self, p: Vec3, k: &mut Caches) -> [u8; 4] {
        let bump = self.shape.bump(p, &mut k.bump);
        self.paint(p, bump, k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fill the generator replaced: every voxel of the cube, one thread.
    fn fill_every_voxel(spec: &PlanetFill) -> ChunkField {
        let planet = Planet::new(spec);
        let mut field = ChunkField::new(spec.voxel.clamp(0.25, 16.0));
        let ext = planet.extent();
        field.fill_with_rgba(
            Vec3::splat(-ext),
            Vec3::splat(ext),
            |p| planet.shape.d(p),
            |p| planet.paint(p, planet.shape.bump(p, &mut FbmCache::default()), &mut Caches::default()),
        );
        field
    }

    /// **A small moon keeps caves you can stand in.** The joins that round a
    /// cave into the crust gate above it and the depth gate below are a few
    /// metres wide; on a moon whose cave zone is under ten metres deep they met
    /// in the middle, and the roomiest point of any cave was 1.46 m from a wall.
    /// Sampled straight down 2,000 directions, some cave keeps 2 m of room.
    #[test]
    fn a_small_moon_keeps_caves_tall_enough_to_stand_in() {
        let radius = 38.7;
        let spec = PlanetFill {
            seed: 5150,
            radius,
            voxel: 1.5,
            relief: radius * 0.065,
            cave_depth: 9.7,
            core_r: 5.0,
            ..PlanetFill::default()
        };
        let planet = Planet::new(&spec);
        let mut room = 0.0f32;
        let mut r = Rng::new(3);
        for _ in 0..2000 {
            let d = Vec3::new(r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32)
                .normalize_or_zero();
            if d == Vec3::ZERO {
                continue;
            }
            let top = radius + spec.relief;
            let mut t = 0.0f32;
            while t < spec.relief * 2.0 + spec.cave_depth + 3.0 {
                let p = d * (top - t);
                // Below the ground only: a cave, not the sky.
                if planet.shape.body(p) < 0.0 {
                    room = room.max(planet.shape.d(p));
                }
                t += 0.05;
            }
        }
        assert!(room >= 2.0, "the roomiest cave on the moon is {room:.2} m from a wall");
    }

    /// [`Planet`], counting the voxels it is asked to evaluate.
    struct Counting(Planet, std::sync::atomic::AtomicUsize);

    impl crate::VoxelSource for Counting {
        type Scratch = Caches;
        fn classify(&self, lo: Vec3, hi: Vec3, band: f32) -> crate::BlockClass {
            self.0.classify(lo, hi, band)
        }
        fn sample(&self, p: Vec3, band: f32, k: &mut Caches) -> Option<(f32, [u8; 4])> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.0.sample(p, band, k)
        }
        fn solid_color(&self, p: Vec3, k: &mut Caches) -> [u8; 4] {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.0.solid_color(p, k)
        }
    }

    /// **A planet costs its surface, not its volume.** With the same ground
    /// and caves, doubling the radius quadruples the shell they live in and
    /// multiplies the cube around it by eight; the voxels evaluated have to
    /// follow the shell. And what is kept follows what the chunks say: a
    /// stored chunk costs well under the 256 KB a flat one did.
    #[test]
    fn a_planet_costs_its_shell_not_its_cube() {
        let run = |radius: f32| {
            let spec = PlanetFill { seed: 77, radius, voxel: 1.0, relief: 5.0, cave_depth: 12.0, core_r: 4.0, craters: 3, ..PlanetFill::default() };
            let src = Counting(Planet::new(&spec), Default::default());
            let ext = spec.radius + spec.relief + 2.0;
            let field = ChunkField::generate(spec.voxel, Vec3::splat(-ext), Vec3::splat(ext), &src);
            let cube = (2.0 * (ext + field.band() + 1.0)).powi(3) as usize;
            (src.1.into_inner(), cube, field)
        };
        let (small, small_cube, _) = run(60.0);
        let (big, big_cube, field) = run(120.0);
        let growth = big as f32 / small as f32;
        // A shell of fixed thickness grows a little faster than r² at these
        // sizes (its volume here grows ×4.3), and the cube grows ×8.
        assert!(growth < 5.5, "doubling the radius multiplied the evaluated voxels by {growth:.2}; the shell grows by ~4.3, the cube by 8");
        assert!(big * 2 < big_cube, "{big} of the cube's {big_cube} voxels evaluated");
        assert!(small < small_cube, "{small} of {small_cube}");
        let per_chunk = field.memory_bytes() / field.data_chunks().max(1);
        // About 62 KB here; unfolded solid blocks alone put it near 96 KB.
        assert!(per_chunk < 80 * 1024, "{per_chunk} bytes a stored chunk; a flat one was 262144");
    }

    /// **Mountains taller than the base relief are generated whole.** The fill
    /// walked a cube sized by the base relief alone, so a range taller than it
    /// came out sliced flat at the cube's face.
    #[test]
    fn mountains_taller_than_the_relief_are_not_cut_off() {
        let spec = PlanetFill {
            seed: 8,
            radius: 60.0,
            voxel: 1.0,
            relief: 3.0,
            cave_depth: 0.0,
            mountains: Some(Mountains { height: 20.0, scale: 2.0, sharpness: 1.5, coverage: 0.7, octaves: 3 }),
            ..PlanetFill::default()
        };
        let f = generate_planet(&spec);
        let planet = Planet::new(&spec);
        let (_, top) = planet.shape.height_range();
        assert!(top > spec.relief + 8.0, "the fixture's mountains are not tall: {top}");
        let mut highest = 0.0f32;
        let mut r = Rng::new(2);
        for _ in 0..4000 {
            let d = Vec3::new(r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32).normalize_or_zero();
            if let Some(hit) = f.raycast(d * (spec.radius + 60.0), -d, 120.0) {
                highest = highest.max(hit.length() - spec.radius);
            }
        }
        assert!(highest > spec.relief + 4.0, "the ground tops out at {highest:.1} m: the peaks were cut off");
        assert!(highest <= top + 1.0, "ground at {highest:.1} m, above the grid's highest {top:.1}");
    }

    /// **Relief layers cost a planet little more to fill.** The ground with
    /// mountains, warp, terraces, overhangs, ravines and rivers is read from a
    /// precomputed grid whose bounds keep the classifier skipping what it
    /// skips, so the voxels evaluated grow with the extra surface the layers
    /// make and no more: at most 30% over the same world without them.
    #[test]
    fn relief_layers_add_at_most_a_third_to_the_voxels_evaluated() {
        let plain = PlanetFill { seed: 21, radius: 90.0, voxel: 1.0, relief: 6.0, cave_depth: 16.0, core_r: 5.0, ..PlanetFill::default() };
        let layered = PlanetFill {
            mountains: Some(Mountains { height: 8.0, scale: 3.0, sharpness: 2.0, coverage: 0.4, octaves: 4 }),
            warp: Some(Warp { strength: 0.15, scale: 2.5 }),
            terrace: Some(Terrace { step: 3.0, sharpness: 0.8 }),
            overhangs: Some(Overhangs { depth: 6.0, scale: 0.1, strength: 2.0 }),
            ravines: Some(Ravines { count: 3, depth: 8.0, width: 6.0, length: 150.0, meander: 1.0 }),
            rivers: Some(Rivers { count: 4, depth: 2.0, width: 4.0, sea_level: -2.0, meander: 0.5 }),
            ..plain.clone()
        };
        let count = |spec: &PlanetFill| {
            let src = Counting(Planet::new(spec), Default::default());
            let ext = src.0.extent();
            ChunkField::generate(spec.voxel, Vec3::splat(-ext), Vec3::splat(ext), &src);
            src.1.into_inner()
        };
        let (a, b) = (count(&plain), count(&layered));
        assert!((b as f64) < 1.3 * a as f64, "the layers took the voxels evaluated from {a} to {b}");
    }

    /// **Skipping blocks changes no voxel.** The generator evaluates only the
    /// blocks it cannot prove are open air or deep solid, on many threads; the
    /// field it builds must be the one the whole-cube fill builds, distance and
    /// colour, chunk for chunk — a block wrongly called solid is a wall in a
    /// cave, and one wrongly called air is a hole in the ground.
    #[test]
    fn generating_matches_filling_every_voxel() {
        let worlds = [
            // Caves, craters, pockets, seams and ice.
            PlanetFill {
                seed: 4242,
                radius: 70.0,
                voxel: 1.0,
                relief: 6.0,
                cave_depth: 26.0,
                core_r: 6.0,
                craters: 5,
                pockets: Some(GlowPockets { paint: LayerPaint { slot: 7, color: [1.0, 0.4, 0.2] }, threshold: 0.3, min_depth: 4.0 }),
                seam: Some(SeamSpec { paint: LayerPaint { slot: 6, color: [0.9, 0.9, 0.3] }, min_depth: 8.0, center: 0.3, width: 0.05 }),
                ice_caps: Some((0.8, LayerPaint { slot: 9, color: [0.9, 0.95, 1.0] })),
                ..PlanetFill::default()
            },
            // A solid body with tall relief and no caves.
            PlanetFill { seed: 9, radius: 48.0, voxel: 1.5, relief: 9.0, cave_depth: 0.0, craters: 0, ..PlanetFill::default() },
            // Every relief layer at once: mountains on warped ground, plateaus,
            // terraces, overhangs, ravines, rivers, and biome rules.
            PlanetFill {
                seed: 77,
                radius: 64.0,
                voxel: 1.0,
                relief: 4.0,
                cave_depth: 14.0,
                core_r: 4.0,
                craters: 2,
                mountains: Some(Mountains { height: 14.0, scale: 2.5, sharpness: 2.0, coverage: 0.6, octaves: 4 }),
                warp: Some(Warp { strength: 0.15, scale: 2.0 }),
                plateau: Some(Plateau { height: 10.0, smooth: 2.0 }),
                terrace: Some(Terrace { step: 3.0, sharpness: 0.7 }),
                overhangs: Some(Overhangs { depth: 6.0, scale: 0.12, strength: 2.5 }),
                ravines: Some(Ravines { count: 2, depth: 8.0, width: 6.0, length: 120.0, meander: 1.0 }),
                rivers: Some(Rivers { count: 3, depth: 2.0, width: 4.0, sea_level: -1.0, meander: 0.5 }),
                biomes: vec![
                    Biome { paint: LayerPaint { slot: 12, color: [1.0, 1.0, 1.0] }, altitude: Some([12.0, 100.0]), ..Biome::default() },
                    Biome { paint: LayerPaint { slot: 13, color: [0.5, 0.5, 0.5] }, slope: Some([35.0, 90.0]), ..Biome::default() },
                    Biome { paint: LayerPaint { slot: 14, color: [0.3, 0.6, 0.3] }, near_river: Some(6.0), ..Biome::default() },
                ],
                ..PlanetFill::default()
            },
        ];
        for spec in &worlds {
            let (want, got) = (fill_every_voxel(spec), generate_planet(spec));
            let mut cw = want.all_chunk_coords();
            let mut cg = got.all_chunk_coords();
            cw.sort_unstable();
            cg.sort_unstable();
            assert_eq!(cw, cg, "the same chunks are stored");
            assert_eq!(want.data_chunks(), got.data_chunks());
            let (mut dw, mut colw, mut dg, mut colg) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for c in cw {
                let lo = [c[0] * crate::CHUNK, c[1] * crate::CHUNK, c[2] * crate::CHUNK];
                want.gather(lo, crate::CHUNK as usize, &mut dw, &mut colw);
                got.gather(lo, crate::CHUNK as usize, &mut dg, &mut colg);
                for i in 0..dw.len() {
                    assert!(dw[i] == dg[i] && colw[i] == colg[i], "chunk {c:?} voxel {i}: {} {:?} vs {} {:?}", dw[i], colw[i], dg[i], colg[i]);
                }
            }
        }
    }

    /// The card's Bryolru-like world: radius 200, relief 13, caves to 0.35 r.
    fn bryolru() -> PlanetShape {
        let radius = 200.19;
        PlanetShape {
            noise: Noise::new(800921623),
            radius,
            relief: 13.11,
            cave_depth: radius * 0.35,
            core_r: radius * 0.07,
            bump_freq: 4.5,
            voxel: 1.5,
            craters: Vec::new(),
            ground: None,
            overhangs: None,
        }
    }

    /// Wall points of the shape, cave walls and the outer ground apart: for
    /// each, how far the wall runs before it curves by a radian (its radius of
    /// curvature), and the sharpest normal change to a wall point 1.5 m away.
    fn wall_samples(shape: &PlanetShape, n: usize) -> [Vec<(f32, f32)>; 2] {
        let f = |p: Vec3| shape.d(p);
        let grad = |p: Vec3| {
            let h = 0.25;
            Vec3::new(
                f(p + Vec3::X * h) - f(p - Vec3::X * h),
                f(p + Vec3::Y * h) - f(p - Vec3::Y * h),
                f(p + Vec3::Z * h) - f(p - Vec3::Z * h),
            )
            .normalize_or_zero()
        };
        let mut seed = 7u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f32) / (1u64 << 31) as f32
        };
        let mut out: [Vec<(f32, f32)>; 2] = [Vec::new(), Vec::new()];
        let mut tries = 0;
        while out[0].len() + out[1].len() < n && tries < n * 200 {
            tries += 1;
            // A random open point in the cave zone or just above the ground,
            // then the nearest wall along a random ray.
            let d = Vec3::new(rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0).normalize_or_zero();
            let r0 = shape.radius - shape.cave_depth + rnd() * (shape.cave_depth + shape.relief + 5.0);
            let p0 = d * r0;
            if d == Vec3::ZERO || f(p0) <= 0.2 {
                continue;
            }
            let rd = Vec3::new(rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0).normalize_or_zero();
            let mut t = 0.0;
            let mut hit = None;
            while t < 30.0 {
                let v = f(p0 + rd * t);
                if v < 0.0 {
                    hit = Some(t);
                    break;
                }
                t += (v * 0.3).max(0.05);
            }
            let Some(t) = hit else { continue };
            let p = p0 + rd * t;
            // Cave wall or open ground: is the ground what makes the wall here?
            let outer = shape.body(p) >= f(p) - 1e-3;
            let nrm = grad(p);
            let t1 = nrm.cross(Vec3::new(0.3, 0.8, 0.5)).normalize_or_zero();
            let t2 = nrm.cross(t1);
            let k1 = (grad(p + t1 * 0.75) - grad(p - t1 * 0.75)).length() / 1.5;
            let k2 = (grad(p + t2 * 0.75) - grad(p - t2 * 0.75)).length() / 1.5;
            let mut crease = 0.0f32;
            for step in [t1, t2, -t1, -t2] {
                let mut q = p + step * 1.5;
                for _ in 0..8 {
                    q -= grad(q) * f(q);
                }
                crease = crease.max(nrm.dot(grad(q)).clamp(-1.0, 1.0).acos().to_degrees());
            }
            out[outer as usize].push((1.0 / k1.max(k2).max(1e-4), crease));
        }
        out
    }

    /// **Cave walls are rock, not panels.** On the Bryolru-like spec, no more of
    /// the cave walls is flat over 20 m than twice the outer ground's share (it
    /// was 27% against 6%), and under 2% of them sit within 1.5 m of a crease
    /// sharper than 60°.
    #[test]
    fn cave_walls_curve_like_the_ground_and_have_no_creases() {
        let [caves, ground] = wall_samples(&bryolru(), 2500);
        assert!(caves.len() > 300 && ground.len() > 300, "too few samples: {} caves, {} ground", caves.len(), ground.len());
        let flat = |s: &[(f32, f32)]| s.iter().filter(|x| x.0 > 20.0).count() as f32 / s.len() as f32;
        let sharp = |s: &[(f32, f32)], deg: f32| s.iter().filter(|x| x.1 > deg).count() as f32 / s.len() as f32;
        let (fc, fg) = (flat(&caves), flat(&ground));
        let creased = sharp(&caves, 60.0);
        eprintln!(
            "caves {}: {:.1}% flat over 20 m, {:.1}% by a crease > 30°, {:.1}% > 60°; ground {}: {:.1}% flat",
            caves.len(),
            fc * 100.0,
            sharp(&caves, 30.0) * 100.0,
            creased * 100.0,
            ground.len(),
            fg * 100.0
        );
        assert!(fc <= 2.0 * fg.max(0.01), "{:.1}% of cave walls are flat over 20 m against {:.1}% of the ground", fc * 100.0, fg * 100.0);
        assert!(creased < 0.02, "{:.1}% of cave walls sit by a crease sharper than 60°", creased * 100.0);
    }

    /// **The crust holds.** No cave opens within three voxels of the surface,
    /// however the joins are smoothed.
    #[test]
    fn no_cave_comes_within_three_voxels_of_the_surface() {
        let shape = bryolru();
        let mut seed = 3u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f32) / (1u64 << 31) as f32
        };
        let mut checked = 0;
        for _ in 0..20000 {
            let d = Vec3::new(rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0).normalize_or_zero();
            if d == Vec3::ZERO {
                continue;
            }
            // Walk in from above the ground to three voxels under it.
            let mut r = shape.radius + shape.relief + 1.0;
            while shape.body(d * r) > 0.0 {
                r -= 0.25;
            }
            // Down through the crust, by the ground's own depth: from just
            // under the surface (which is zero) to three voxels below it.
            loop {
                let p = d * r;
                let depth = -shape.body(p);
                if depth >= shape.voxel * 3.0 {
                    break;
                }
                if depth > 0.01 {
                    assert!(shape.d(p) < 0.0, "open space {depth:.2} m under the surface at {p}");
                    checked += 1;
                }
                r -= 0.25;
            }
        }
        assert!(checked > 100_000);
    }

    /// Same spec → byte-identical field; a different seed diverges.    /// Same spec → byte-identical field; a different seed diverges.
    #[test]
    fn generate_planet_is_deterministic() {
        let spec = PlanetFill { radius: 24.0, voxel: 1.0, cave_depth: 10.0, ..Default::default() };
        let a = generate_planet(&spec).to_bytes();
        let b = generate_planet(&spec).to_bytes();
        assert_eq!(a, b);
        let c = generate_planet(&PlanetFill { seed: 8, ..spec }).to_bytes();
        assert_ne!(a, c);
    }

    /// The seam band paints veins, not walls: a banded seam must color far
    /// fewer voxels than a threshold at the same level would.
    #[test]
    fn seam_band_is_sparse() {
        let base = PlanetFill {
            radius: 22.0,
            voxel: 1.0,
            relief: 3.0,
            cave_depth: 12.0,
            core_r: 3.0,
            ..Default::default()
        };
        let spec = PlanetFill {
            seam: Some(SeamSpec {
                paint: LayerPaint { slot: 6, color: [1.0, 0.8, 0.6] },
                min_depth: 2.0,
                center: 0.3,
                width: 0.04,
            }),
            ..base.clone()
        };
        let field = generate_planet(&spec);
        let (mut seam_px, mut total) = (0u32, 0u32);
        let (Some((lo, hi)),) = (field.bounds(),) else { panic!("empty field") };
        let mut p = lo;
        while p.x < hi.x {
            p.y = lo.y;
            while p.y < hi.y {
                p.z = lo.z;
                while p.z < hi.z {
                    if field.d(p) < 0.0 {
                        total += 1;
                        if field.color(p)[3] == 6 {
                            seam_px += 1;
                        }
                    }
                    p.z += 2.0;
                }
                p.y += 2.0;
            }
            p.x += 2.0;
        }
        assert!(total > 500, "sample too small ({total})");
        let frac = seam_px as f32 / total as f32;
        assert!(frac < 0.15, "seams flood the rock: {frac:.2} of solid voxels glow");
    }
}
