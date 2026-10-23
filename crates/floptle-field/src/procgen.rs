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
    /// Leave the buried cave zone ungenerated until something comes near it.
    /// From outside a planet is its surface, and its caves are most of its
    /// chunks: a deferred planet fills its surface shell and keeps the rest as
    /// stand-in rock, made when a dig, a body or the camera reaches it.
    pub defer_caves: bool,
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
    /// Only between these latitudes, degrees from the equator: north or
    /// south when both are 0 or more, north positive when either is
    /// negative. `None` is everywhere.
    pub latitude: Option<[f32; 2]>,
    /// Degrees over which the layer fades out past `latitude`.
    pub latitude_edge: f32,
}

impl Default for Mountains {
    fn default() -> Self {
        Mountains { height: 40.0, scale: 3.0, sharpness: 2.0, coverage: 0.4, octaves: 5, latitude: None, latitude_edge: 5.0 }
    }
}

/// Domain warp: the relief is read at a point pushed up to `strength` (a
/// fraction of the radius) by noise `scale` across the sphere.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Warp {
    pub strength: f32,
    pub scale: f32,
    /// Only between these latitudes, degrees from the equator: north or
    /// south when both are 0 or more, north positive when either is
    /// negative. `None` is everywhere.
    pub latitude: Option<[f32; 2]>,
    /// Degrees over which the layer fades out past `latitude`.
    pub latitude_edge: f32,
}

impl Default for Warp {
    fn default() -> Self {
        Warp { strength: 0.0, scale: 0.0, latitude: None, latitude_edge: 5.0 }
    }
}

/// Peaks above `height` are rounded off into flat tops over `smooth` metres.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Plateau {
    pub height: f32,
    pub smooth: f32,
    /// Only between these latitudes, degrees from the equator: north or
    /// south when both are 0 or more, north positive when either is
    /// negative. `None` is everywhere.
    pub latitude: Option<[f32; 2]>,
    /// Degrees over which the layer fades out past `latitude`.
    pub latitude_edge: f32,
}

impl Default for Plateau {
    fn default() -> Self {
        Plateau { height: 20.0, smooth: 3.0, latitude: None, latitude_edge: 5.0 }
    }
}

/// Terraces `step` metres tall; `sharpness` 0 is a gentle slope between them,
/// 1 a sheer rise.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Terrace {
    pub step: f32,
    pub sharpness: f32,
    /// Only between these latitudes, degrees from the equator: north or
    /// south when both are 0 or more, north positive when either is
    /// negative. `None` is everywhere.
    pub latitude: Option<[f32; 2]>,
    /// Degrees over which the layer fades out past `latitude`.
    pub latitude_edge: f32,
}

impl Default for Terrace {
    fn default() -> Self {
        Terrace { step: 6.0, sharpness: 0.6, latitude: None, latitude_edge: 5.0 }
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
            defer_caves: false,
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
        let mut body = self.ground_with(p, bump);
        for (c, cr) in &self.craters {
            body = body.max(-((p - *c).length() - cr));
        }
        body
    }

    /// The ground with its overhangs, before the craters: negative under it.
    fn ground_with(&self, p: Vec3, bump: f32) -> f32 {
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
    // Kept as the field's source either way: it makes the deferred caves, and
    // any chunk put away to save memory, again.
    let defer = spec.defer_caves && spec.cave_depth > 0.0;
    ChunkField::generate_sourced(spec.voxel, Vec3::splat(-ext), Vec3::splat(ext), std::sync::Arc::new(planet), defer)
}

/// [`generate_planet`], making only the chunks within `reach` of `near`
/// (planet-local) and leaving coarse stand-ins for the rest of the surface,
/// made exactly as [`generate_planet`] makes them when something comes near
/// (see [`ChunkField::generate_near`]). For a resume, or any world the player
/// is not yet on: ready in a fraction of the time.
pub fn generate_planet_near(spec: &PlanetFill, near: Vec3, reach: f32) -> ChunkField {
    let planet = Planet::new(spec);
    let ext = planet.extent();
    let defer = spec.defer_caves && spec.cave_depth > 0.0;
    let size = crate::CHUNK as f32 * spec.voxel.clamp(0.25, 16.0);
    ChunkField::generate_near(spec.voxel, Vec3::splat(-ext), Vec3::splat(ext), std::sync::Arc::new(planet), defer, |c| {
        let lo = Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) * size;
        (near.clamp(lo, lo + Vec3::splat(size)) - near).length() <= reach
    })
}

/// One lake on a generated planet: a body of still water standing in a basin.
#[derive(Clone, Debug)]
pub struct Lake {
    /// Its surface, in metres above the planet's radius.
    pub level: f32,
    /// The direction from the planet's centre to its middle (area-weighted).
    pub centre: Vec3,
    /// The great-circle distance from the middle to its farthest shore, metres.
    pub reach: f32,
    /// Its surface area, square metres.
    pub area: f32,
    /// Its deepest point, metres below the surface.
    pub depth: f32,
}

/// The lakes a planet's basins hold, without generating it: every basin
/// filled to the height it spills at (see [`crate::water::lake_levels`]), on
/// a grid of cells about `cell` metres across, and each connected stretch of
/// water deeper than `min_depth` above `sea_level` reported as one lake.
/// Ground below `sea_level` is sea, and not a lake. Largest first.
pub fn lakes_of(spec: &PlanetFill, cell: f32, min_depth: f32, sea_level: f32) -> Vec<Lake> {
    let planet = Planet::new(spec);
    let radius = spec.radius;
    let n = ((std::f32::consts::FRAC_PI_2 * radius) / cell.max(0.5)).ceil().clamp(4.0, 2048.0) as usize;
    let grid = crate::water::WaterGrid::sphere(n, radius, |d| planet.ground_height(d));
    let level = crate::water::lake_levels(&grid, sea_level);
    let wet = |c: usize| level[c] - grid.ground[c] > min_depth && level[c] > sea_level;
    let dir_of = |c: usize| {
        let (f, r) = (c / (n * n), c % (n * n));
        let u = -1.0 + ((r % n) as f32 + 0.5) * 2.0 / n as f32;
        let v = -1.0 + ((r / n) as f32 + 0.5) * 2.0 / n as f32;
        crate::water::face_dir(f, u, v)
    };
    let mut seen = vec![false; grid.len()];
    let mut lakes = Vec::new();
    for start in 0..grid.len() {
        if seen[start] || !wet(start) {
            continue;
        }
        // One lake: the wet cells joined to this one at the same level.
        let mut stack = vec![start];
        seen[start] = true;
        let mut cells = Vec::new();
        while let Some(c) = stack.pop() {
            cells.push(c);
            for &o in &grid.nbr[c] {
                let o = o as usize;
                if o != crate::water::WALL as usize && !seen[o] && wet(o) && (level[o] - level[c]).abs() < 1e-3 {
                    seen[o] = true;
                    stack.push(o);
                }
            }
        }
        let area: f32 = cells.iter().map(|&c| grid.area[c]).sum();
        let centre = cells.iter().map(|&c| dir_of(c) * grid.area[c]).sum::<Vec3>().normalize_or_zero();
        let reach = cells.iter().map(|&c| dir_of(c).angle_between(centre)).fold(0.0f32, f32::max) * radius;
        let depth = cells.iter().map(|&c| level[c] - grid.ground[c]).fold(0.0f32, f32::max);
        lakes.push(Lake { level: level[start], centre, reach, area, depth });
    }
    lakes.sort_by(|a, b| b.area.total_cmp(&a.area));
    lakes
}

/// What makes a planet's pending chunks, for a field loaded from a file: it
/// knows which chunks are pending but not how to make them. The spec has to be
/// the one the planet was generated from.
pub fn pending_source_for(spec: &PlanetFill) -> Option<std::sync::Arc<dyn crate::PendingSource>> {
    let planet = Planet::new(spec);
    let ext = planet.extent();
    let defer = spec.defer_caves && spec.cave_depth > 0.0;
    Some(ChunkField::generate_source_for(spec.voxel, Vec3::splat(-ext), Vec3::splat(ext), std::sync::Arc::new(planet), defer))
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

    /// The ground's height above the radius in direction `d` (a unit
    /// vector): the relief, and a crater's floor where one has been carved.
    /// Overhangs and caves are not part of it.
    pub(crate) fn ground_height(&self, d: Vec3) -> f32 {
        let s = &self.shape;
        let mut t = s.radius + s.bump(d * s.radius, &mut FbmCache::default());
        // A crater is a ball taken out of the ground: where the surface falls
        // inside one, the ground is the ball's floor along this direction.
        for (c, cr) in &s.craters {
            let b = d.dot(*c);
            let disc = b * b - c.length_squared() + cr * cr;
            if disc > 0.0 {
                let (near, far) = (b - disc.sqrt(), b + disc.sqrt());
                if t > near && t <= far {
                    t = near.max(0.0);
                }
            }
        }
        t - s.radius
    }

    /// How far from the centre the ground can reach: the radius, the highest
    /// the relief layers raise it, and what overhangs can push out, with a
    /// little to spare. The cube a fill walks.
    pub(crate) fn extent(&self) -> f32 {
        let s = &self.shape;
        s.radius + s.height_range().1.max(s.relief.abs()) + s.overhang_reach() + 2.0
    }

    /// The colour a voxel at `p` is painted, the ground there raised `bump`.
    ///
    /// Its depth is how far it is under the ground as the overhangs leave it,
    /// not under the ground's height in its direction: an overhang lifts the
    /// surface up to its strength off that height, and the subsoil and strata
    /// counted from the height came up through undug ground in blotches, with
    /// crystal pockets on the spires.
    fn paint(&self, p: Vec3, bump: f32, k: &mut Caches) -> [u8; 4] {
        let (spec, noise) = (&self.spec, &self.shape.noise);
        let (radius, relief, cave_depth, core_r) =
            (self.shape.radius, self.shape.relief, self.shape.cave_depth, self.shape.core_r);
        let r = p.length();
        let dir = if r > 1e-3 { p / r } else { Vec3::X };
        let alt = (r - radius) / relief.max(1.0);
        let depth = -self.shape.ground_with(p, bump);
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
        // Depth layers: deep cave walls → strata → subsoil. A world with no
        // cave zone has no deep cave rock: at a zone of zero, everything a
        // hair under the ground was deep rock, and no subsoil or strata showed.
        if cave_depth > 0.0 && depth > cave_depth * 0.5 {
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

    /// A chunk wholly below the crust gate holds no outer surface, so all
    /// that is in it is caves and the rock between them: nothing anyone sees
    /// from outside. Only those the classifier cannot already settle.
    fn defer(&self, lo: Vec3, hi: Vec3, band: f32) -> bool {
        let s = &self.shape;
        if s.cave_depth <= 0.0 {
            return false;
        }
        let far = lo.abs().max(hi.abs());
        let corners: [Vec3; 8] = std::array::from_fn(|k| {
            Vec3::new(
                if k & 1 == 0 { lo.x } else { hi.x },
                if k & 2 == 0 { lo.y } else { hi.y },
                if k & 4 == 0 { lo.z } else { hi.z },
            )
        });
        let near = Vec3::ZERO.clamp(lo, hi);
        let bump_lo = match &s.ground {
            Some(relief) => relief.height_bounds(&corners).0,
            None => self.bump_bounds(lo, hi, near.length(), far.length()).0,
        };
        let mut body_hi = far.length() - (s.radius + bump_lo) + s.overhang_reach();
        for (c, cr) in &s.craters {
            body_hi = body_hi.max(cr - (*c - c.clamp(lo, hi)).length());
        }
        body_hi <= -(3.0 * s.voxel + band + s.voxel) && self.classify(lo, hi, band) == crate::BlockClass::Mixed
    }
}

/// How much of a layer limited to `band` (degrees, see [`Mountains::latitude`])
/// stands in direction `d`, fading over `edge` degrees past the band; the
/// latitude is measured from the equator round the spin axis, +Y, the axis the
/// biomes' latitude is measured on.
pub(crate) fn latitude_weight(band: Option<[f32; 2]>, edge: f32, d: Vec3) -> f32 {
    let Some([a, b]) = band else { return 1.0 };
    let (lo, hi) = (a.min(b), a.max(b));
    let y = d.y.clamp(-1.0, 1.0);
    let lat = if lo < 0.0 { y.asin() } else { y.abs().asin() }.to_degrees();
    let e = edge.max(1e-3);
    let ramp = |x: f32| {
        let t = x.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    ramp((lat - (lo - e)) / e) * (1.0 - ramp((lat - hi) / e))
}

#[cfg(test)]
mod tests {
    /// **Overhangs do not bring the subsoil up through undug ground.** The
    /// paint's depth was counted from the ground's height in a voxel's
    /// direction, while overhangs move the surface up to their strength off
    /// that height: with subsoil two voxels down and overhangs of 6, 3% of this
    /// world's undug ground showed a deeper layer. Read half a voxel under the
    /// surface down 600 directions.
    #[test]
    fn overhangs_do_not_bring_the_subsoil_up_through_undug_ground() {
        let spec = PlanetFill {
            seed: 77,
            radius: 120.0,
            voxel: 1.5,
            relief: 8.0,
            cave_depth: 30.0,
            subsoil_depth: 3.0,
            subsoil: LayerPaint { slot: 9, color: [0.5, 0.5, 0.5] },
            overhangs: Some(Overhangs { depth: 10.0, scale: 0.08, strength: 6.0 }),
            ..PlanetFill::default()
        };
        let field = generate_planet(&spec);
        let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
        let (mut seen, mut subsoil) = (0, 0);
        for i in 0..600 {
            let y = 1.0 - 2.0 * (i as f32 + 0.5) / 600.0;
            let r = (1.0 - y * y).sqrt();
            let d = Vec3::new(r * (golden * i as f32).cos(), y, r * (golden * i as f32).sin());
            let Some(hit) = field.raycast(d * 200.0, -d, 200.0) else { continue };
            let n = field.grad(hit).normalize_or_zero();
            seen += 1;
            // Anything but the two surface slots is a deeper layer showing.
            subsoil += usize::from(!matches!(field.slot_at(hit - n * 0.5 * spec.voxel), Some(1 | 2)));
        }
        assert!(seen > 500, "only {seen} rays found the ground");
        assert!(subsoil * 100 <= seen, "{subsoil} of {seen} points of undug ground show a deeper layer");
    }

    /// **A lake's ground is the ground the planet is generated with.** The
    /// height `lakes_of` reads in a direction is where a ray from outside the
    /// generated field meets its surface, craters included, to within a voxel.
    #[test]
    fn the_ground_a_lake_reads_is_the_generated_ground() {
        let spec = PlanetFill { seed: 3, radius: 120.0, voxel: 1.5, relief: 8.0, cave_depth: 0.0, craters: 6, crater_min: 0.35, crater_max: 0.45, ..PlanetFill::default() };
        let planet = Planet::new(&spec);
        let field = generate_planet(&spec);
        let mut rng = Rng::new(17);
        let mut in_crater = 0;
        for _ in 0..200 {
            let d = Vec3::new(rng.range(-1.0, 1.0) as f32, rng.range(-1.0, 1.0) as f32, rng.range(-1.0, 1.0) as f32).normalize_or_zero();
            if d == Vec3::ZERO {
                continue;
            }
            let Some(hit) = field.raycast(d * 200.0, -d, 200.0) else { continue };
            let h = planet.ground_height(d);
            let base = planet.shape.bump(d * spec.radius, &mut FbmCache::default());
            in_crater += usize::from(h < base - 1.0);
            assert!((hit.length() - (spec.radius + h)).abs() < 1.5, "{d}: ground at {} but the field's surface at {}", spec.radius + h, hit.length());
        }
        assert!(in_crater > 5, "no ray went into a crater: {in_crater}");
    }

    /// **A crater holds a lake.** Of two craters on a smooth planet with no
    /// sea, the deeper is where the planet drains to; the other comes back as
    /// a lake in it, standing between its floor and the ground round it.
    #[test]
    fn a_crater_comes_back_as_a_lake() {
        let spec = PlanetFill { seed: 8, radius: 150.0, voxel: 1.5, relief: 1.0, cave_depth: 0.0, craters: 2, crater_min: 0.15, crater_max: 0.3, ..PlanetFill::default() };
        let planet = Planet::new(&spec);
        let floor = |c: Vec3| planet.ground_height(c.normalize());
        let (a, b) = (planet.shape.craters[0].0, planet.shape.craters[1].0);
        let c = if floor(a) > floor(b) { a } else { b };
        let c = c.normalize();
        let lakes = lakes_of(&spec, 3.0, 0.5, -1000.0);
        let lake = lakes
            .iter()
            .min_by(|a, b| a.centre.angle_between(c).total_cmp(&b.centre.angle_between(c)))
            .expect("the planet holds no lake");
        assert!(lake.centre.angle_between(c.normalize()) < 0.1, "no lake is in the crater: {lakes:?}");
        let floor = floor(c);
        assert!(lake.level > floor + 1.0 && lake.level < 1.5, "level {} over a floor at {floor}", lake.level);
        assert!(lake.depth > 1.0 && lake.area > 100.0, "{lake:?}");
    }

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

    /// **A planet whose caves wait is the same planet once they are made.**
    /// Deferring leaves the buried cave zone as stand-in rock: from outside the
    /// planet reads as it does when filled whole, and realizing every pending
    /// chunk gives the same caves, to within the evening-out at chunk faces.
    /// A dig into it makes what it reaches before reading it, and a save
    /// keeps the chunks that are still to make.
    #[test]
    fn a_deferred_planet_is_the_whole_planet_once_its_caves_are_made() {
        let spec = PlanetFill { seed: 13, radius: 150.0, voxel: 1.0, relief: 5.0, cave_depth: 70.0, core_r: 8.0, ..PlanetFill::default() };
        let whole = generate_planet(&spec);
        let mut deferred = generate_planet(&PlanetFill { defer_caves: true, ..spec.clone() });
        let pending = deferred.pending_count();
        assert!(pending > 20, "only {pending} chunks deferred");
        assert!(deferred.data_chunks() * 10 < whole.data_chunks() * 8, "{} data chunks deferred against {} whole", deferred.data_chunks(), whole.data_chunks());
        // From outside, the same ground.
        let mut r = Rng::new(6);
        for _ in 0..500 {
            let d = Vec3::new(r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32).normalize_or_zero();
            let (a, b) = (whole.raycast(d * 200.0, -d, 100.0), deferred.raycast(d * 200.0, -d, 100.0));
            assert_eq!(a.is_some(), b.is_some());
            if let (Some(a), Some(b)) = (a, b) {
                assert!((a - b).length() < 1e-3, "the surface moved: {a:?} against {b:?}");
            }
        }
        // A save keeps what is still to make, and the spec makes it.
        let mut back = ChunkField::from_bytes(&deferred.to_bytes()).expect("the save loads");
        assert_eq!(back.pending_count(), pending);
        back.set_pending_source(pending_source_for(&PlanetFill { defer_caves: true, ..spec.clone() }).unwrap());
        // A dig inside the cave zone makes the chunks it reaches first.
        let deep = Vec3::new(0.0, spec.radius - 18.0, 0.0);
        let c = deep.to_array().map(|v| (v / spec.voxel).floor() as i32).map(|i| i.div_euclid(crate::CHUNK));
        let before = back.pending_count();
        back.sculpt(crate::Brush::Lower, deep, 2.0, 1.0, crate::BrushProfile::default());
        assert!(back.pending_count() < before, "the dig left its chunk {c:?} ungenerated");
        // Every chunk made, in two orders and through a dig: the same caves,
        // to the bit, whatever was made first; and close to the planet filled
        // whole, which evened all its chunks out together.
        let all: Vec<[i32; 3]> = deferred.all_chunk_coords();
        let mut other = generate_planet(&PlanetFill { defer_caves: true, ..spec.clone() });
        for c in all.iter().rev() {
            other.realize(&[*c]);
        }
        deferred.realize(&all);
        assert_eq!(deferred.pending_count(), 0);
        let (mut differ, mut sign, mut band) = (0usize, 0usize, 0usize);
        let (mut dw, mut cw, mut dd, mut cd, mut d2, mut c2) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut coords = whole.all_chunk_coords();
        coords.extend(deferred.all_chunk_coords());
        coords.sort_unstable();
        coords.dedup();
        for ch in coords {
            let lo = [ch[0] * crate::CHUNK, ch[1] * crate::CHUNK, ch[2] * crate::CHUNK];
            whole.gather(lo, crate::CHUNK as usize, &mut dw, &mut cw);
            deferred.gather(lo, crate::CHUNK as usize, &mut dd, &mut cd);
            other.gather(lo, crate::CHUNK as usize, &mut d2, &mut c2);
            assert!(dd == d2 && cd == c2, "chunk {ch:?} came out differently made in another order");
            for i in 0..dw.len() {
                band += usize::from(dw[i].abs() < whole.band());
                differ += usize::from((dw[i] - dd[i]).abs() > 0.05 * spec.voxel);
                sign += usize::from((dw[i] < 0.0) != (dd[i] < 0.0));
            }
        }
        assert!(sign * 1000 < band, "{sign} of {band} band voxels changed side against the planet filled whole");
        assert!(differ * 50 < band, "{differ} of {band} band voxels differ by more than a twentieth of a voxel");
    }

    /// A dug planet for the save tests: ten bites of a mining laser down into
    /// the cave zone and a flattened pad, with the recipe written on it.
    fn dug_planet(spec: &PlanetFill) -> ChunkField {
        let mut f = generate_planet(spec);
        f.set_spec(ron_like(spec));
        let top = Vec3::new(0.0, spec.radius + spec.relief * 2.0 + 10.0, 0.0);
        let ground = f.raycast(top, Vec3::NEG_Y, top.y).expect("the planet is there");
        for i in 0..10 {
            f.sculpt(crate::Brush::Lower, ground - Vec3::new(0.0, i as f32 * 1.6, i as f32 * 0.4), 2.4, 0.85, crate::BrushProfile::default());
        }
        let pad = (Vec3::new(1.0, 1.0, 0.3).normalize() * (spec.radius + 2.0)).normalize();
        let at = f.raycast(pad * (spec.radius * 1.5), -pad, spec.radius).expect("the pad's ground");
        f.flatten_to_plane(at, pad, 6.0, 1.0, crate::BrushProfile::default());
        f
    }

    /// The spec as a string a save can carry: anything the test can turn back
    /// into the same planet.
    fn ron_like(spec: &PlanetFill) -> String {
        format!("{}", spec.seed)
    }

    /// Every chunk of `a` and `b` reads the same, to the precision a save keeps.
    fn same_as_saved(a: &ChunkField, b: &ChunkField) -> Result<(), String> {
        let band = a.band();
        let q = |v: f32| ((v / band) * 127.0).round() as i32;
        let mut coords = a.all_chunk_coords();
        coords.extend(b.all_chunk_coords());
        coords.sort_unstable();
        coords.dedup();
        let (mut da, mut ca, mut db, mut cb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for c in coords {
            let lo = [c[0] * crate::CHUNK, c[1] * crate::CHUNK, c[2] * crate::CHUNK];
            a.gather(lo, crate::CHUNK as usize, &mut da, &mut ca);
            b.gather(lo, crate::CHUNK as usize, &mut db, &mut cb);
            for i in 0..da.len() {
                if q(da[i]) != q(db[i]) {
                    return Err(format!("chunk {c:?} voxel {i}: {} against {}", da[i], db[i]));
                }
                if da[i] < band - 1e-6 && ca[i] != cb[i] {
                    return Err(format!("chunk {c:?} voxel {i}: colour {:?} against {:?}", ca[i], cb[i]));
                }
            }
        }
        Ok(())
    }

    /// **A generated planet saves what was done to it, not the whole field.**
    /// Every save of a dug 800 m planet wrote its 400 MB field, though all but
    /// a few chunks of it were what the generator makes. Now a save holds the
    /// edited chunks and the recipe; loading lays them over a fresh generation
    /// and gets back the same planet, the edits as the save kept them.
    #[test]
    fn a_dug_planet_saves_its_edits_and_comes_back_the_same() {
        let spec = PlanetFill { seed: 41, radius: 140.0, voxel: 1.5, relief: 6.0, cave_depth: 80.0, core_r: 5.0, defer_caves: true, ..PlanetFill::default() };
        let dug = dug_planet(&spec);
        assert!(dug.edited_count() > 0 && dug.edited_count() < 40, "{} chunks edited", dug.edited_count());
        let edits = dug.to_bytes();
        let mut whole = dug.clone();
        whole.clear_spec();
        let whole = whole.to_bytes();
        assert!(edits.len() * 5 < whole.len(), "the save is {} bytes against {} for the whole field", edits.len(), whole.len());

        let back = ChunkField::from_bytes(&edits).expect("the save loads");
        assert!(back.is_delta() && back.spec() == Some(ron_like(&spec).as_str()));
        let mut back = back.lay_over(generate_planet(&spec));
        same_as_saved(&dug, &back).unwrap();
        // The pit is still dug where it was, and the next save is edits again.
        assert_eq!(back.edited_count(), dug.edited_count());
        assert_eq!(back.to_bytes(), edits, "saving the loaded planet wrote something else");
        // The caves still to make are made the same as they would have been.
        let mut fresh = dug.clone();
        let deep = fresh.pending_in_box(Vec3::splat(-spec.radius), Vec3::splat(spec.radius));
        assert!(!deep.is_empty(), "no caves left to make: {} pending", fresh.pending_count());
        fresh.realize(&deep);
        back.realize(&deep);
        same_as_saved(&fresh, &back).unwrap();
    }

    /// **A save from before edits were tracked still loads, and becomes
    /// small.** It holds the whole field and no record of what was edited;
    /// compared with a fresh generation, the chunks that differ are the edits,
    /// and the field saves only those from then on.
    #[test]
    fn a_whole_field_save_still_loads_and_finds_its_edits() {
        let spec = PlanetFill { seed: 42, radius: 90.0, voxel: 1.5, relief: 6.0, cave_depth: 25.0, core_r: 5.0, defer_caves: true, ..PlanetFill::default() };
        let mut dug = dug_planet(&spec);
        let edited = dug.edited_count();
        dug.clear_spec();
        let old = dug.to_bytes();
        let mut back = ChunkField::from_bytes(&old).expect("an old save loads");
        assert!(!back.is_delta());
        back.set_spec(ron_like(&spec));
        back.adopt_generation(&generate_planet(&spec));
        same_as_saved(&dug, &back).unwrap();
        assert!(back.edited_count() <= edited, "{} chunks taken for edits, {edited} were edited", back.edited_count());
        assert!(back.edited_count() > 0, "the dig was lost");
        assert!(back.to_bytes().len() * 5 < old.len(), "it still saves whole: {} of {} bytes, {} chunks edited of {edited}", back.to_bytes().len(), old.len(), back.edited_count());
    }

    /// Every voxel of `a` and `b`, distance to the bit and colour, the same.
    fn identical(a: &ChunkField, b: &ChunkField, coords: &[[i32; 3]]) -> Result<(), String> {
        let (mut da, mut ca, mut db, mut cb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for &c in coords {
            let lo = [c[0] * crate::CHUNK, c[1] * crate::CHUNK, c[2] * crate::CHUNK];
            a.gather(lo, crate::CHUNK as usize, &mut da, &mut ca);
            b.gather(lo, crate::CHUNK as usize, &mut db, &mut cb);
            for i in 0..da.len() {
                if da[i].to_bits() != db[i].to_bits() || (da[i] < a.band() && ca[i] != cb[i]) {
                    return Err(format!("chunk {c:?} voxel {i}: {} {:?} against {} {:?}", da[i], ca[i], db[i], cb[i]));
                }
            }
        }
        Ok(())
    }

    /// **A chunk put away comes back exactly as the fill made it.** A planet
    /// kilometres across keeps a coarse stand-in for the ground nobody is
    /// near, and makes the chunk again on approach; the generator's own
    /// isolated remaking, which deferred caves use, differs at a chunk's
    /// borders from the fill, which evened the whole field out at once. Every
    /// chunk of two worlds, one with its caves deferred and relief layers,
    /// put away together and made again, to the bit.
    #[test]
    fn a_chunk_put_away_comes_back_exactly_as_the_fill_made_it() {
        let worlds = [
            PlanetFill { seed: 21, radius: 70.0, voxel: 1.0, relief: 6.0, cave_depth: 16.0, core_r: 5.0, craters: 2, ..PlanetFill::default() },
            PlanetFill {
                seed: 22,
                radius: 110.0,
                voxel: 1.5,
                relief: 8.0,
                cave_depth: 40.0,
                core_r: 6.0,
                defer_caves: true,
                mountains: Some(Mountains { height: 14.0, scale: 3.0, sharpness: 2.0, coverage: 0.5, octaves: 4, ..Default::default() }),
                overhangs: Some(Overhangs { depth: 6.0, scale: 0.1, strength: 2.0 }),
                ..PlanetFill::default()
            },
        ];
        for spec in worlds {
            let mut f = generate_planet(&spec);
            f.set_spec("test".into());
            let original = f.clone();
            let coords = f.chunk_coords();
            let evicted = f.evict(&coords);
            assert_eq!(evicted.len(), coords.len(), "every data chunk is evictable");
            assert_eq!(f.evicted_count(), coords.len());
            assert!(f.memory_bytes() * 10 < original.memory_bytes(), "{} bytes put away to {}", original.memory_bytes(), f.memory_bytes());
            f.realize(&coords);
            assert_eq!(f.evicted_count(), 0);
            identical(&original, &f, &coords).unwrap();
        }
    }

    /// **A world made only near the player is the whole world once it is
    /// all made.** Resuming a kilometre-wide planet made every chunk before
    /// the player could stand on it. Made near one point, the rest stands in
    /// coarse, and every chunk made later is the one the whole fill makes, to
    /// the bit; the stand-ins cost a small part of the whole.
    #[test]
    fn a_world_made_near_the_player_is_the_whole_world_once_made() {
        let spec = PlanetFill {
            seed: 25,
            radius: 110.0,
            voxel: 1.5,
            relief: 8.0,
            cave_depth: 40.0,
            core_r: 6.0,
            defer_caves: true,
            mountains: Some(Mountains { height: 14.0, scale: 3.0, sharpness: 2.0, coverage: 0.5, octaves: 4, ..Default::default() }),
            ..PlanetFill::default()
        };
        let whole = generate_planet(&spec);
        let near = Vec3::new(0.0, spec.radius, 0.0);
        let mut lazy = generate_planet_near(&spec, near, 40.0);
        let stood_in = lazy.evicted_count();
        assert!(stood_in > lazy.chunk_coords().len() / 2, "only {stood_in} chunks stand in");
        assert!(lazy.memory_bytes() * 3 < whole.memory_bytes(), "{} bytes against {} whole", lazy.memory_bytes(), whole.memory_bytes());
        // The ground near the player is there already.
        let made: Vec<[i32; 3]> = lazy.chunk_coords().into_iter().filter(|c| !lazy.is_evicted(*c)).collect();
        identical(&whole, &lazy, &made).unwrap();
        // And the rest is the whole fill's, once made.
        let rest: Vec<[i32; 3]> = lazy.chunk_coords().into_iter().filter(|c| lazy.is_evicted(*c)).collect();
        lazy.realize(&rest);
        let mut all = whole.chunk_coords();
        all.extend(lazy.chunk_coords());
        all.sort_unstable();
        all.dedup();
        identical(&whole, &lazy, &all).unwrap();
    }

    /// **An edited chunk is never put away**: it is the player's, and nothing
    /// could make it again. Nor is a field with no recipe for its saves.
    #[test]
    fn an_edited_chunk_is_never_put_away() {
        let spec = PlanetFill { seed: 23, radius: 70.0, voxel: 1.0, relief: 6.0, cave_depth: 0.0, ..PlanetFill::default() };
        let mut f = generate_planet(&spec);
        let coords = f.chunk_coords();
        assert!(f.evict(&coords).is_empty(), "a field with no recipe put chunks away");
        f.set_spec("test".into());
        let top = f.raycast(Vec3::new(0.0, 100.0, 0.0), Vec3::NEG_Y, 100.0).unwrap();
        let dug = f.sculpt(crate::Brush::Lower, top, 3.0, 1.0, crate::BrushProfile::default());
        assert!(!dug.is_empty());
        let evicted: Vec<[i32; 3]> = f.evict(&coords).iter().map(|e| e.coord()).collect();
        for c in &dug {
            assert!(!evicted.contains(c), "edited chunk {c:?} was put away");
            assert!(!f.is_evicted(*c));
        }
        assert!(evicted.len() + dug.len() >= coords.len() - 2);
    }

    /// **From afar, a world put away is the world.** A ray from orbit meets the
    /// stand-ins near the ground (a lattice point every four voxels, of
    /// distances that stop four voxels from the surface: within a voxel at the
    /// 95th percentile, under three at worst), and a write into one makes the
    /// chunk first, so the dig lands in the real ground.
    #[test]
    fn a_world_put_away_reads_as_itself_from_afar() {
        let spec = PlanetFill { seed: 24, radius: 90.0, voxel: 1.5, relief: 8.0, cave_depth: 20.0, ..PlanetFill::default() };
        let mut f = generate_planet(&spec);
        f.set_spec("test".into());
        let whole = f.clone();
        f.evict(&f.chunk_coords());
        let mut r = Rng::new(3);
        let mut off = Vec::new();
        for _ in 0..300 {
            let d = Vec3::new(r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32).normalize_or_zero();
            if d == Vec3::ZERO {
                continue;
            }
            let (a, b) = (whole.raycast(d * 200.0, -d, 200.0), f.raycast(d * 200.0, -d, 200.0));
            let (Some(a), Some(b)) = (a, b) else { panic!("a ray at {d:?} missed: {a:?} {b:?}") };
            off.push((a - b).length());
        }
        off.sort_by(f32::total_cmp);
        let (p95, worst) = (off[off.len() * 95 / 100], off[off.len() - 1]);
        assert!(p95 < spec.voxel && worst < 3.0 * spec.voxel, "the stand-ins' ground is {p95:.2} m off the real at the 95th percentile, {worst:.2} m at worst");
        let p = whole.raycast(Vec3::Y * 200.0, Vec3::NEG_Y, 200.0).unwrap();
        let mut real = whole.clone();
        let dug = real.sculpt(crate::Brush::Lower, p, 2.0, 1.0, crate::BrushProfile::default());
        f.sculpt(crate::Brush::Lower, p, 2.0, 1.0, crate::BrushProfile::default());
        identical(&real, &f, &dug).map_err(|e| format!("the dig landed on the stand-in, not the ground: {e}")).unwrap();
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
            mountains: Some(Mountains { height: 20.0, scale: 2.0, sharpness: 1.5, coverage: 0.7, octaves: 3, ..Default::default() }),
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
            mountains: Some(Mountains { height: 8.0, scale: 3.0, sharpness: 2.0, coverage: 0.4, octaves: 4, ..Default::default() }),
            warp: Some(Warp { strength: 0.15, scale: 2.5, ..Default::default() }),
            terrace: Some(Terrace { step: 3.0, sharpness: 0.8, ..Default::default() }),
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
                mountains: Some(Mountains { height: 14.0, scale: 2.5, sharpness: 2.0, coverage: 0.6, octaves: 4, ..Default::default() }),
                warp: Some(Warp { strength: 0.15, scale: 2.0, ..Default::default() }),
                plateau: Some(Plateau { height: 10.0, smooth: 2.0, ..Default::default() }),
                terrace: Some(Terrace { step: 3.0, sharpness: 0.7, ..Default::default() }),
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
