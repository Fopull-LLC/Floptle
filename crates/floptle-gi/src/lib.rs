//! Baked global illumination — the bounce.
//!
//! Direct light says "the sun reaches here". Everything a room actually looks
//! like past that is *indirect*: light that hit a red wall, came off red, and
//! landed on the floor. Without it a scene reads flat no matter how good the
//! materials are, and the usual patch — a flat grey ambient term — is worse than
//! nothing, because it lifts the inside of a closed box exactly as much as it
//! lifts an open field.
//!
//! This crate holds the part of the answer that is *arithmetic*: a grid of
//! probes over a box, a spherical-harmonic representation of the light arriving
//! at each one, the cube-face integration that fills them in, and the file the
//! result is saved as. Nothing here touches the GPU or the scene — the editor
//! renders the probe cubes through the ordinary renderer and hands the pixels
//! here, and [`floptle_render`](../floptle_render) uploads what comes back.
//!
//! **Why a probe grid and not a lightmap.** A lightmap needs every surface in
//! the scene unwrapped into a shared texture atlas, which is a whole pipeline
//! and a whole authoring burden. A probe grid needs nothing from the geometry at
//! all: it is a lattice in space, so it lights meshes, terrain, sculpted matter,
//! tilemaps, particles and characters alike, static or moving, with no
//! preparation. The cost is resolution — a grid cannot represent a shadow
//! sharper than its own spacing. Contact detail is what SSAO and contact shadows
//! are for; this is the long-range bounce those cannot invent.
//!
//! **Why SH-L1 and not more.** Four coefficients per colour channel carry a
//! constant plus one direction: enough for "the light in this spot comes mostly
//! from over there, and it is warm". L2 costs nine coefficients for a sharpening
//! that a grid this coarse cannot honestly claim to know. The truncation is also
//! *safe*: L1 cannot ring into negative light the way L2 does around a bright
//! source, and negative light in an ambient term reads as a black smear.

#![forbid(unsafe_code)]

use floptle_core::math::{Mat3, Quat, Vec3};

/// The maximum probes a single volume may hold. A bake renders six real frames
/// per probe, so the count is a *time* budget before it is a memory one: at
/// 32,768 probes that is nearly 200,000 renders. The cap is high enough that no
/// sane grid hits it and low enough that a typo in the spacing box cannot ask
/// for a bake that never finishes.
pub const MAX_PROBES: usize = 32_768;

/// The largest probe count along one axis.
pub const MAX_DIM: u32 = 64;

// ---- Spherical harmonics ------------------------------------------------------

/// Band-0 basis function, constant over the sphere: `sqrt(1/(4π))`.
const Y0: f32 = 0.282_094_79;
/// Band-1 basis function scale: `sqrt(3/(4π))`, multiplied by the axis component.
const Y1: f32 = 0.488_602_51;

/// Radiance projected onto the first two SH bands, per colour channel.
///
/// `c[0]` is the constant term; `c[1..4]` are the x, y and z lobes. These are
/// *radiance* coefficients — the light arriving at the probe — not irradiance.
/// [`Sh1::irradiance`] does the cosine convolution on the way out, because the
/// convolution depends on the surface normal and the probe does not have one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sh1 {
    pub c: [[f32; 3]; 4],
}

impl Sh1 {
    pub const ZERO: Sh1 = Sh1 { c: [[0.0; 3]; 4] };

    /// Add one sample: radiance `rgb` arriving from direction `dir` (unit,
    /// pointing *away* from the probe toward where the light came from) over
    /// solid angle `dw` steradians.
    pub fn accumulate(&mut self, dir: Vec3, rgb: [f32; 3], dw: f32) {
        let w = [Y0 * dw, Y1 * dir.x * dw, Y1 * dir.y * dw, Y1 * dir.z * dw];
        for (band, wk) in self.c.iter_mut().zip(w) {
            for (ch, &c) in band.iter_mut().zip(rgb.iter()) {
                *ch += c * wk;
            }
        }
    }

    /// Multiply every coefficient (used to average bakes, or to apply an
    /// intensity before upload).
    pub fn scaled(mut self, k: f32) -> Sh1 {
        for band in &mut self.c {
            for ch in band.iter_mut() {
                *ch *= k;
            }
        }
        self
    }

    /// The diffuse response of a surface with normal `n`: the value that
    /// multiplies albedo directly.
    ///
    /// This folds three things that are easy to get wrong separately. The cosine
    /// convolution (`π` for band 0, `2π/3` for band 1) turns radiance into
    /// irradiance; the division by `π` turns irradiance into the outgoing
    /// radiance of a Lambert surface; and the result is clamped at zero, because
    /// a truncated SH fit can dip below zero on the dark side of a strong lobe
    /// and negative light is not a thing.
    pub fn irradiance(&self, n: Vec3) -> [f32; 3] {
        // (π·Y0)/π = Y0 ;  ((2π/3)·Y1)/π = (2/3)·Y1
        const A0: f32 = Y0;
        const A1: f32 = Y1 * 2.0 / 3.0;
        let d = [A1 * n.x, A1 * n.y, A1 * n.z];
        let mut out = [0.0f32; 3];
        for (ch, o) in out.iter_mut().enumerate() {
            let v = A0 * self.c[0][ch]
                + d[0] * self.c[1][ch]
                + d[1] * self.c[2][ch]
                + d[2] * self.c[3][ch];
            *o = v.max(0.0);
        }
        out
    }

    /// The direction the light is mostly coming from, and how directional it is
    /// (0 = even from every side, 1 = a single lobe). The editor's probe gizmo
    /// draws this; nothing in the render path needs it.
    pub fn dominant(&self) -> (Vec3, f32) {
        let lum = |v: [f32; 3]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
        let d = Vec3::new(lum(self.c[1]), lum(self.c[2]), lum(self.c[3]));
        let dc = lum(self.c[0]).max(1e-6);
        let len = d.length();
        // Band 1's magnitude relative to band 0, normalized by the basis ratio
        // so a single delta-function light reads as 1.
        let ratio = (len / dc) * (Y0 / Y1) / 3.0;
        (if len > 1e-9 { d / len } else { Vec3::Y }, ratio.clamp(0.0, 1.0))
    }
}

// ---- Cube faces ---------------------------------------------------------------

/// One face of the cube a probe is rendered through: the camera's forward and up
/// axes. Six of these cover the whole sphere exactly once.
#[derive(Clone, Copy, Debug)]
pub struct Face {
    pub forward: Vec3,
    pub up: Vec3,
}

/// The six probe-camera orientations, in a fixed order the bake and the reader
/// both rely on. The ups are chosen only to be perpendicular to their forward —
/// which face is "up" in the rendered image never matters, because every texel
/// is integrated with its own direction.
pub const FACES: [Face; 6] = [
    Face { forward: Vec3::X, up: Vec3::Y },
    Face { forward: Vec3::NEG_X, up: Vec3::Y },
    Face { forward: Vec3::Y, up: Vec3::Z },
    Face { forward: Vec3::NEG_Y, up: Vec3::NEG_Z },
    Face { forward: Vec3::Z, up: Vec3::Y },
    Face { forward: Vec3::NEG_Z, up: Vec3::Y },
];

impl Face {
    /// The camera rotation that looks along this face.
    ///
    /// A `RenderCamera` looks down its own −Z, so the basis is
    /// (right, up, −forward) — the same convention the fly camera builds, which
    /// is what makes a probe render an ordinary frame of the scene rather than a
    /// special case the renderer has to know about.
    pub fn rotation(&self) -> Quat {
        let f = self.forward.normalize();
        let r = f.cross(self.up).normalize();
        let u = r.cross(f);
        Quat::from_mat3(&Mat3::from_cols(r, u, -f))
    }

    /// The world direction of the face texel at normalized face coordinates
    /// `(u, v)` ∈ [−1, 1]², where +u is right and +v is up in the rendered image.
    ///
    /// The face is a 90° frustum, so `tan(fov/2) = 1` and the plane coordinates
    /// *are* the u/v — which is the whole reason a cube is the convenient shape
    /// to integrate a sphere through.
    pub fn texel_dir(&self, u: f32, v: f32) -> Vec3 {
        let f = self.forward.normalize();
        let r = f.cross(self.up).normalize();
        let up = r.cross(f);
        (f + r * u + up * v).normalize()
    }
}

/// Face coordinates of the texel at pixel `(x, y)` of an `n`×`n` face.
///
/// `v` is negated against the pixel row because a rendered image runs top-down
/// while the face's up axis runs bottom-up. Getting this backwards flips the
/// bake vertically, which looks *almost* right — the ceiling bounce lands on the
/// ceiling — and is the single most confusing way for a bake to be wrong.
pub fn texel_uv(x: u32, y: u32, n: u32) -> (f32, f32) {
    let s = |i: u32| 2.0 * (i as f32 + 0.5) / n as f32 - 1.0;
    (s(x), -s(y))
}

/// The solid angle the texel at pixel `(x, y)` of an `n`×`n` cube face subtends.
///
/// A cube face is a flat plane held at a fixed distance, so its texels are not
/// equal in angle: the ones near a corner are further away and seen edge-on, and
/// count for roughly a third as much as the one dead ahead. Weighting by this is
/// the difference between an integral and an average.
///
/// This is the *exact* spherical excess of the texel's quad, not the derivative
/// at its centre. The cheap version is off by 1.5% at 4×4 faces and 0.1% at
/// 32×32 — which does not sound like much until you notice it is a brightness
/// error that changes when you change the bake quality, so raising quality would
/// visibly re-light the scene. Exact means the quality slider only affects
/// sharpness, which is the only thing it should mean.
pub fn texel_solid_angle(x: u32, y: u32, n: u32) -> f32 {
    // Signed area of the spherical quad from the origin to (0,0)-(a,b) on the
    // face plane; the four corners combine into the texel's own area.
    let corner = |a: f32, b: f32| (a * b).atan2((a * a + b * b + 1.0).sqrt());
    let e = |i: u32| 2.0 * i as f32 / n as f32 - 1.0;
    let (u0, u1) = (e(x), e(x + 1));
    let (v0, v1) = (e(y), e(y + 1));
    corner(u1, v1) - corner(u0, v1) - corner(u1, v0) + corner(u0, v0)
}

// ---- The grid -----------------------------------------------------------------

/// Where the probes are: an axis-aligned lattice over a box.
///
/// Probes sit *on* the lattice including its outer faces, not at cell centres,
/// so a shading point anywhere inside the box is surrounded by eight real probes
/// and needs no clamping. Cell centres would leave a half-cell rind around the
/// volume where the interpolation runs off the end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeGrid {
    pub dims: [u32; 3],
    pub center: [f32; 3],
    pub half_extent: [f32; 3],
}

impl ProbeGrid {
    /// A grid over `center ± half_extent` with probes about `spacing` apart.
    ///
    /// The spacing is a *request*: the real spacing is whatever divides the box
    /// into a whole number of steps, and an over-budget grid is COARSENED rather
    /// than trimmed. Coarsening keeps the grid isotropic and keeps a deliberately
    /// flat volume — a corridor, a room one storey high — flat. Trimming the
    /// longest axis instead would quietly turn every large volume into a cube of
    /// probes with the wrong aspect, which is worse light and much harder to see.
    pub fn from_spacing(center: [f32; 3], half_extent: [f32; 3], spacing: f32) -> ProbeGrid {
        let half = half_extent.map(|h| h.abs().max(0.01));
        let mut s = spacing.max(0.05);
        loop {
            let mut dims = [0u32; 3];
            for i in 0..3 {
                dims[i] = ((half[i] * 2.0 / s).round() as i64 + 1).max(2) as u32;
            }
            let count = dims[0] as u64 * dims[1] as u64 * dims[2] as u64;
            if count <= MAX_PROBES as u64 && dims.iter().all(|&d| d <= MAX_DIM) {
                return ProbeGrid { dims, center, half_extent: half };
            }
            s *= 1.25;
        }
    }

    pub fn count(&self) -> usize {
        self.dims[0] as usize * self.dims[1] as usize * self.dims[2] as usize
    }

    /// Distance between adjacent probes on each axis.
    pub fn spacing(&self) -> [f32; 3] {
        let mut out = [0.0; 3];
        for (a, o) in out.iter_mut().enumerate() {
            *o = 2.0 * self.half_extent[a] / (self.dims[a].max(2) - 1) as f32;
        }
        out
    }

    pub fn index(&self, x: u32, y: u32, z: u32) -> usize {
        ((z * self.dims[1] + y) * self.dims[0] + x) as usize
    }

    /// The lattice coordinates of probe `i`, in the same order [`index`] packs.
    ///
    /// [`index`]: ProbeGrid::index
    pub fn coords(&self, i: usize) -> [u32; 3] {
        let w = self.dims[0] as usize;
        let h = self.dims[1] as usize;
        [(i % w) as u32, ((i / w) % h) as u32, (i / (w * h)) as u32]
    }

    /// World position of probe `i`.
    pub fn probe_world(&self, i: usize) -> Vec3 {
        let c = self.coords(i);
        let mut p = Vec3::ZERO;
        for a in 0..3 {
            let n = self.dims[a].max(2) - 1;
            let t = c[a] as f32 / n as f32;
            p[a] = self.center[a] - self.half_extent[a] + t * 2.0 * self.half_extent[a];
        }
        p
    }
}

// ---- Probes and the baked volume ----------------------------------------------

/// One probe's baked result.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Probe {
    /// Radiance arriving at the probe, SH-L1.
    pub sh: Sh1,
    /// The closest surface the probe can see, in world units. This is what tells
    /// a probe buried inside a wall apart from one floating in a room, which is
    /// the whole basis of the leak test at sample time.
    pub nearest: f32,
    /// Mean distance over the sphere. Not used by the render path; it is what
    /// makes "this probe is in a cupboard" legible in the editor.
    pub mean: f32,
}

/// A baked irradiance volume: the grid, and one [`Probe`] per lattice point.
#[derive(Clone, Debug, PartialEq)]
pub struct BakedGi {
    pub grid: ProbeGrid,
    pub probes: Vec<Probe>,
    /// How many bounces produced this data. Recorded so the editor can say what
    /// it is looking at, and so a re-bake at a different setting is visibly a
    /// different thing rather than a mystery.
    pub bounces: u32,
}

/// The magic at the head of a `.fgi` file.
pub const MAGIC: &[u8; 4] = b"FGI1";

impl BakedGi {
    /// An unlit volume of the right shape — what a scene has before its first
    /// bake, and what a cancelled bake leaves behind.
    pub fn empty(grid: ProbeGrid) -> BakedGi {
        BakedGi { grid, probes: vec![Probe::default(); grid.count()], bounces: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    /// The GPU texels for this volume, four per probe, laid out for a 3D texture
    /// of size `(dims.x * 4, dims.y, dims.z)`.
    ///
    /// The four coefficients of a probe sit *side by side along x* rather than
    /// in four separate textures. That costs one binding instead of four, and it
    /// costs nothing in sampling quality, because the shader has to compute its
    /// own eight-probe weights anyway — hardware trilinear cannot apply the leak
    /// test, so there is no filtering to preserve.
    ///
    /// `leak` (0…1) is applied here rather than at bake time so that turning
    /// leak rejection up or down is an upload, not a re-bake.
    pub fn texels(&self, leak: f32, intensity: f32) -> Vec<[f32; 4]> {
        let sp = self.grid.spacing();
        let min_sp = sp[0].min(sp[1]).min(sp[2]).max(1e-4);
        let [w, h, d] = self.grid.dims;
        let mut out = vec![[0.0f32; 4]; (w * 4 * h * d) as usize];
        for (i, p) in self.probes.iter().enumerate() {
            let c = self.grid.coords(i);
            let base = ((c[2] * h + c[1]) * (w * 4) + c[0] * 4) as usize;
            let sh = p.sh.scaled(intensity);
            let valid = probe_validity(p.nearest, leak, min_sp);
            let extra = [valid, p.nearest, p.mean, 0.0];
            for k in 0..4 {
                out[base + k] = [sh.c[k][0], sh.c[k][1], sh.c[k][2], extra[k]];
            }
        }
        out
    }

    /// Sample the volume the way the shader does: eight surrounding probes,
    /// trilinear, weighted so a probe on the wrong side of the surface or buried
    /// in geometry contributes little or nothing. Returns the value that
    /// multiplies albedo, and a coverage in 0…1 (0 = outside the volume).
    ///
    /// **The WGSL in `field.wgsl` is a transliteration of this function.** It
    /// lives here first because leak weighting is exactly the kind of thing that
    /// is miserable to debug on a GPU and trivial to pin with a test: a probe
    /// through a wall must not light the room next door, and that is a statement
    /// about arithmetic, not about pixels.
    pub fn sample(&self, pos: Vec3, normal: Vec3, leak: f32, normal_bias: f32) -> ([f32; 3], f32) {
        if self.probes.is_empty() {
            return ([0.0; 3], 0.0);
        }
        let g = &self.grid;
        let sp = g.spacing();
        let min_sp = sp[0].min(sp[1]).min(sp[2]).max(1e-4);
        // Step off the surface before looking up. A shading point sits exactly
        // ON the geometry, which is the one place where "which side of the wall
        // am I on" is genuinely ambiguous; half a cell along the normal is not.
        let p = pos + normal * (normal_bias * min_sp);

        let c = Vec3::from(g.center);
        let hx = Vec3::from(g.half_extent);
        // Coverage: full inside, fading out over the outer tenth of the box so a
        // volume's edge is a transition instead of a visible seam in the wall.
        let local = (p - c) / hx.max(Vec3::splat(1e-4));
        let m = local.abs().max_element();
        let coverage = 1.0 - ((m - 0.9) / 0.1).clamp(0.0, 1.0);
        if coverage <= 0.0 {
            return ([0.0; 3], 0.0);
        }

        let dims = Vec3::new(g.dims[0] as f32, g.dims[1] as f32, g.dims[2] as f32);
        let t = ((local * 0.5) + Vec3::splat(0.5)).clamp(Vec3::ZERO, Vec3::ONE) * (dims - Vec3::ONE);
        let base = t.floor();
        let frac = t - base;

        let mut acc = Sh1::ZERO;
        let mut wsum = 0.0f32;
        for corner in 0..8u32 {
            let off = Vec3::new(
                (corner & 1) as f32,
                ((corner >> 1) & 1) as f32,
                ((corner >> 2) & 1) as f32,
            );
            let ix = (base + off).min(dims - Vec3::ONE);
            let idx = g.index(ix.x as u32, ix.y as u32, ix.z as u32);
            let probe = self.probes[idx];

            // Trilinear.
            let tri = (off.x * frac.x + (1.0 - off.x) * (1.0 - frac.x))
                * (off.y * frac.y + (1.0 - off.y) * (1.0 - frac.y))
                * (off.z * frac.z + (1.0 - off.z) * (1.0 - frac.z));

            // Wrap-shading term: a probe behind the surface being shaded cannot
            // be lighting it. Squared and softened rather than a hard cutoff, so
            // a surface sliding past a probe plane does not pop.
            let to = g.probe_world(idx) - p;
            let dir = if to.length_squared() > 1e-12 { to.normalize() } else { normal };
            let facing = (normal.dot(dir) * 0.5 + 0.5).max(0.0);
            let wrap = facing * facing + 0.05;

            // Leak test: a probe with no clearance is inside geometry.
            let valid = probe_validity(probe.nearest, leak, min_sp);

            let w = tri * wrap * valid;
            if w > 0.0 {
                for k in 0..4 {
                    for ch in 0..3 {
                        acc.c[k][ch] += probe.sh.c[k][ch] * w;
                    }
                }
                wsum += w;
            }
        }
        if wsum <= 1e-6 {
            return ([0.0; 3], 0.0);
        }
        let e = acc.scaled(1.0 / wsum).irradiance(normal);
        (e, coverage)
    }

    /// Serialize to the `.fgi` blob saved next to the scene.
    ///
    /// Layout: magic, bounces u32, dims\[3\]u32, center\[3\]f32, half\[3\]f32, then
    /// per probe 12 SH floats + nearest + mean. Plain little-endian f32 — the
    /// whole file for a large volume is a few hundred kilobytes, and a format
    /// you can read with a hex editor is worth more than the bytes it saves.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.probes.len() * 56);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.bounces.to_le_bytes());
        for v in self.grid.dims {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in self.grid.center.iter().chain(&self.grid.half_extent) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for p in &self.probes {
            for band in p.sh.c {
                for ch in band {
                    out.extend_from_slice(&ch.to_le_bytes());
                }
            }
            out.extend_from_slice(&p.nearest.to_le_bytes());
            out.extend_from_slice(&p.mean.to_le_bytes());
        }
        out
    }

    /// Parse a blob written by [`to_bytes`](Self::to_bytes). `None` if malformed
    /// — a stale or truncated bake must read as "no bake", never as garbage
    /// light.
    pub fn from_bytes(data: &[u8]) -> Option<BakedGi> {
        if data.len() < 44 || &data[0..4] != MAGIC {
            return None;
        }
        let mut o = 4;
        let u32_at = |o: &mut usize| -> Option<u32> {
            let v = u32::from_le_bytes(data.get(*o..*o + 4)?.try_into().ok()?);
            *o += 4;
            Some(v)
        };
        let bounces = u32_at(&mut o)?;
        let dims = [u32_at(&mut o)?, u32_at(&mut o)?, u32_at(&mut o)?];
        let f32_at = |o: &mut usize| -> Option<f32> {
            let v = f32::from_le_bytes(data.get(*o..*o + 4)?.try_into().ok()?);
            *o += 4;
            Some(v)
        };
        let center = [f32_at(&mut o)?, f32_at(&mut o)?, f32_at(&mut o)?];
        let half_extent = [f32_at(&mut o)?, f32_at(&mut o)?, f32_at(&mut o)?];
        if dims.iter().any(|&d| !(2..=MAX_DIM).contains(&d)) {
            return None;
        }
        let n = dims[0] as usize * dims[1] as usize * dims[2] as usize;
        if n > MAX_PROBES || data.len() < o + n * 56 {
            return None;
        }
        let mut probes = Vec::with_capacity(n);
        for _ in 0..n {
            let mut sh = Sh1::ZERO;
            for band in &mut sh.c {
                for ch in band.iter_mut() {
                    *ch = f32_at(&mut o)?;
                }
            }
            let nearest = f32_at(&mut o)?;
            let mean = f32_at(&mut o)?;
            probes.push(Probe { sh, nearest, mean });
        }
        Some(BakedGi { grid: ProbeGrid { dims, center, half_extent }, probes, bounces })
    }
}

/// How much a probe with `nearest` clearance counts, under a `leak` setting.
///
/// `leak` is a multiple of the grid spacing: 1 rejects probes with less than
/// about two-thirds of a cell of room around them, higher is stricter. **Zero
/// turns the test off entirely** rather than becoming infinitely lenient — a
/// knob at zero has to mean "not doing this", or a scene with no clearance data
/// at all (an old bake, a volume in open air) goes black instead of unchanged.
fn probe_validity(nearest: f32, leak: f32, min_spacing: f32) -> f32 {
    if leak <= 0.0 {
        return 1.0;
    }
    let lo = leak * 0.20 * min_spacing;
    let hi = leak * 0.65 * min_spacing;
    let t = ((nearest - lo) / (hi - lo).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---- Integrating a rendered cube ----------------------------------------------

/// Accumulate one rendered cube face into a probe.
///
/// `color` is the face's pixels in row-major order, linear (scene-referred)
/// RGB — the same values the renderer wrote, *before* any tonemap. Feeding a
/// tonemapped frame in here is the classic way to bake washed-out GI: the
/// tonemap is a display transform, and light does not bounce in display space.
///
/// `distance` is the matching per-pixel radial distance in world units, or an
/// empty slice to skip the clearance measurement.
pub fn accumulate_face(
    probe: &mut Probe,
    face: &Face,
    n: u32,
    color: &[[f32; 3]],
    distance: &[f32],
    stats: &mut FaceStats,
) {
    for y in 0..n {
        for x in 0..n {
            let i = (y * n + x) as usize;
            let Some(&rgb) = color.get(i) else { continue };
            let (u, v) = texel_uv(x, y, n);
            let dw = texel_solid_angle(x, y, n);
            probe.sh.accumulate(face.texel_dir(u, v), rgb, dw);
            if let Some(&d) = distance.get(i)
                && d.is_finite()
                && d > 0.0
            {
                stats.nearest = stats.nearest.min(d);
                stats.sum += d * dw;
                stats.weight += dw;
            }
        }
    }
}

/// Running clearance measurements across a probe's six faces.
#[derive(Clone, Copy, Debug)]
pub struct FaceStats {
    pub nearest: f32,
    sum: f32,
    weight: f32,
}

impl Default for FaceStats {
    fn default() -> Self {
        FaceStats { nearest: f32::INFINITY, sum: 0.0, weight: 0.0 }
    }
}

impl FaceStats {
    /// Fold the six faces' measurements into the probe. Call once, after all six.
    pub fn finish(self, probe: &mut Probe) {
        probe.nearest = if self.nearest.is_finite() { self.nearest } else { 1.0e6 };
        probe.mean = if self.weight > 0.0 { self.sum / self.weight } else { 1.0e6 };
    }
}

/// Turn a depth-buffer value into radial distance from the probe.
///
/// The renderer's perspective projection maps view distance to `[0, 1]`
/// non-linearly, and a cube face's corner texel is further from the probe than
/// its centre one even at equal depth — so both the un-projection and the
/// off-axis stretch have to be undone, or a probe in the middle of a room
/// measures its clearance as the distance to the wall *straight ahead* and reads
/// as buried.
pub fn radial_distance(depth: f32, near: f32, far: f32, u: f32, v: f32) -> f32 {
    if !(0.0..1.0).contains(&depth) {
        return f32::INFINITY; // cleared depth = nothing was drawn = open sky
    }
    let t = far * near / (far - depth * (far - near));
    t * (1.0 + u * u + v * v).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The six faces must tile the sphere exactly: every texel's solid angle,
    /// summed, is 4π. If this drifts, every bake is scaled wrong by a constant
    /// and the whole scene is uniformly too bright or too dark — which reads as
    /// "the GI knob is badly calibrated" rather than as a bug.
    #[test]
    fn the_cube_covers_the_sphere_exactly() {
        for n in [4u32, 8, 16] {
            let mut total = 0.0;
            for _face in FACES {
                for y in 0..n {
                    for x in 0..n {
                        total += texel_solid_angle(x, y, n);
                    }
                }
            }
            let sphere = 4.0 * std::f32::consts::PI;
            assert!(
                (total - sphere).abs() < 1e-3,
                "n = {n}: solid angles sum to {total}, want {sphere}"
            );
        }
    }

    /// Uniform light in every direction must come back out as exactly that
    /// light, on every normal. This is the calibration test: it pins the basis
    /// constants, the cosine convolution and the 1/π together, and any one of
    /// them being wrong shows up here as a constant factor.
    #[test]
    fn a_uniform_sky_shades_flat_and_unchanged() {
        let mut probe = Probe::default();
        let n = 8;
        for face in FACES {
            let color = vec![[0.5, 0.25, 1.0]; (n * n) as usize];
            accumulate_face(&mut probe, &face, n, &color, &[], &mut FaceStats::default());
        }
        for dir in [Vec3::Y, Vec3::NEG_Y, Vec3::X, Vec3::new(1.0, 2.0, -3.0).normalize()] {
            let e = probe.sh.irradiance(dir);
            for (got, want) in e.iter().zip([0.5, 0.25, 1.0]) {
                assert!((got - want).abs() < 2e-3, "{dir:?}: {e:?} should be flat 0.5/0.25/1.0");
            }
        }
    }

    /// Light from one side must land on surfaces facing that side, and not on
    /// surfaces facing away. This is the whole point of keeping band 1.
    #[test]
    fn light_from_above_lights_upward_faces() {
        let mut probe = Probe::default();
        let n = 8;
        for (fi, face) in FACES.iter().enumerate() {
            // Face 2 is +Y.
            let lit = fi == 2;
            let color = vec![if lit { [1.0; 3] } else { [0.0; 3] }; (n * n) as usize];
            accumulate_face(&mut probe, face, n, &color, &[], &mut FaceStats::default());
        }
        let up = probe.sh.irradiance(Vec3::Y)[0];
        let down = probe.sh.irradiance(Vec3::NEG_Y)[0];
        let side = probe.sh.irradiance(Vec3::X)[0];
        assert!(up > side && side > down, "up {up}, side {side}, down {down}");
        assert!(down < 1e-6, "a downward face sees no light from the ceiling: {down}");
        let (dir, ratio) = probe.sh.dominant();
        assert!(dir.y > 0.9, "the dominant direction is up: {dir:?}");
        assert!(ratio > 0.2, "and it reads as directional: {ratio}");
    }

    /// Irradiance is clamped at zero. A truncated L1 fit dips negative opposite a
    /// strong lobe, and a negative ambient term paints a black smear across the
    /// dark side of everything in the scene.
    #[test]
    fn irradiance_never_goes_negative() {
        let mut sh = Sh1::ZERO;
        sh.accumulate(Vec3::Y, [4.0; 3], 1.0);
        let e = sh.irradiance(Vec3::NEG_Y);
        assert!(e.iter().all(|&v| v >= 0.0), "{e:?}");
    }

    #[test]
    fn a_grid_spans_its_box_and_lands_on_the_faces() {
        let g = ProbeGrid::from_spacing([0.0, 0.0, 0.0], [4.0, 2.0, 4.0], 2.0);
        assert_eq!(g.dims, [5, 3, 5]);
        assert_eq!(g.count(), 75);
        let first = g.probe_world(0);
        let last = g.probe_world(g.count() - 1);
        assert!((first - Vec3::new(-4.0, -2.0, -4.0)).length() < 1e-5, "{first:?}");
        assert!((last - Vec3::new(4.0, 2.0, 4.0)).length() < 1e-5, "{last:?}");
        assert_eq!(g.spacing(), [2.0, 2.0, 2.0]);
        // coords ↔ index round-trip, which the texel packing depends on.
        for i in 0..g.count() {
            let c = g.coords(i);
            assert_eq!(g.index(c[0], c[1], c[2]), i);
        }
    }

    /// A silly spacing on a big volume must produce a grid, not a hang. The cap
    /// thins the densest axis, so a deliberately flat volume stays flat.
    #[test]
    fn an_absurd_spacing_is_clamped_not_obeyed() {
        let g = ProbeGrid::from_spacing([0.0; 3], [500.0, 4.0, 500.0], 0.05);
        assert!(g.count() <= MAX_PROBES, "{:?} = {}", g.dims, g.count());
        assert!(g.dims.iter().all(|&d| (2..=MAX_DIM).contains(&d)), "{:?}", g.dims);
        // Coarsened, not trimmed: the volume is 125× wider than it is tall, and
        // the grid still knows that.
        assert!(g.dims[1] * 4 < g.dims[0], "the flat axis stays the thin one: {:?}", g.dims);
        // A modest volume gets the spacing it asked for, untouched.
        let ok = ProbeGrid::from_spacing([0.0; 3], [8.0, 3.0, 8.0], 1.0);
        assert_eq!(ok.dims, [17, 7, 17]);
        assert_eq!(ok.spacing(), [1.0, 1.0, 1.0]);
    }

    fn lit_grid() -> BakedGi {
        let grid = ProbeGrid::from_spacing([0.0; 3], [2.0, 2.0, 2.0], 2.0);
        let mut b = BakedGi::empty(grid);
        for p in &mut b.probes {
            p.sh.accumulate(Vec3::Y, [1.0, 1.0, 1.0], 4.0 * std::f32::consts::PI);
            p.nearest = 10.0;
        }
        b.bounces = 1;
        b
    }

    #[test]
    fn the_file_round_trips_and_rejects_rubbish() {
        let b = lit_grid();
        let bytes = b.to_bytes();
        let back = BakedGi::from_bytes(&bytes).expect("parses");
        assert_eq!(back, b);
        assert!(BakedGi::from_bytes(b"nope").is_none());
        assert!(BakedGi::from_bytes(&bytes[..bytes.len() - 8]).is_none(), "truncated is not partial");
        let mut wrong = bytes.clone();
        wrong[0] = b'X';
        assert!(BakedGi::from_bytes(&wrong).is_none());
    }

    #[test]
    fn sampling_inside_the_volume_returns_the_light_and_outside_returns_nothing() {
        let b = lit_grid();
        let (e, cov) = b.sample(Vec3::ZERO, Vec3::Y, 1.0, 0.0);
        assert!(cov > 0.99, "the middle of the box is fully covered: {cov}");
        assert!(e[0] > 0.5, "an upward face sees the light: {e:?}");
        let (_, cov) = b.sample(Vec3::new(50.0, 0.0, 0.0), Vec3::Y, 1.0, 0.0);
        assert_eq!(cov, 0.0, "well outside the box contributes nothing");
        // The edge is a fade, not a cliff — a hard edge is a visible seam.
        let (_, edge) = b.sample(Vec3::new(1.94, 0.0, 0.0), Vec3::Y, 1.0, 0.0);
        assert!(edge > 0.0 && edge < 1.0, "the rind fades: {edge}");
    }

    /// The leak test, stated as arithmetic. Probes with no clearance are inside
    /// geometry; they must not light anything, even though they are the nearest
    /// probes to the surface asking. This is the failure everyone recognises:
    /// light from the lit room next door glowing through a wall.
    #[test]
    fn a_probe_buried_in_geometry_does_not_light_the_room() {
        let mut b = lit_grid();
        // Bury every probe: no clearance at all.
        for p in &mut b.probes {
            p.nearest = 0.0;
        }
        let (e, _) = b.sample(Vec3::ZERO, Vec3::Y, 1.0, 0.0);
        assert!(e.iter().all(|&v| v < 1e-6), "buried probes contribute nothing: {e:?}");
        // …and with leak rejection turned off, the same data lights again, so
        // the knob is doing this and not something else.
        let (e, _) = b.sample(Vec3::ZERO, Vec3::Y, 0.0, 0.0);
        assert!(e[0] > 0.5, "leak 0 keeps every probe: {e:?}");
    }

    /// A probe behind the surface being shaded contributes almost nothing, which
    /// is the second half of not leaking through a wall.
    #[test]
    fn probes_behind_the_surface_are_weighted_away() {
        let grid = ProbeGrid::from_spacing([0.0; 3], [2.0, 2.0, 2.0], 4.0); // 2×2×2
        let mut b = BakedGi::empty(grid);
        b.bounces = 1;
        for (i, p) in b.probes.iter_mut().enumerate() {
            p.nearest = 10.0;
            // Only the +x half of the lattice carries light.
            if grid.coords(i)[0] == 1 {
                p.sh.accumulate(Vec3::Y, [1.0; 3], 4.0 * std::f32::consts::PI);
            }
        }
        // A surface at the middle facing −x is turned away from the lit probes.
        let away = b.sample(Vec3::ZERO, Vec3::NEG_X, 1.0, 0.0).0[0];
        let toward = b.sample(Vec3::ZERO, Vec3::X, 1.0, 0.0).0[0];
        assert!(toward > away * 3.0, "facing the light must win: {toward} vs {away}");
    }

    #[test]
    fn texels_pack_four_per_probe_across_x() {
        let b = lit_grid();
        let t = b.texels(1.0, 1.0);
        let [w, h, d] = b.grid.dims;
        assert_eq!(t.len(), (w * 4 * h * d) as usize);
        // Probe (0,0,0)'s constant term is positive and its validity lane is 1.
        assert!(t[0][0] > 0.0 && (t[0][3] - 1.0).abs() < 1e-5, "{:?}", t[0]);
        // The nearest-distance lane rides coefficient 1.
        assert!((t[1][3] - 10.0).abs() < 1e-5, "{:?}", t[1]);
        // Intensity scales the light and leaves the extras alone.
        let dim = b.texels(1.0, 0.5);
        assert!((dim[0][0] - t[0][0] * 0.5).abs() < 1e-5);
        assert!((dim[1][3] - 10.0).abs() < 1e-5);
    }

    /// Depth un-projection, including the off-axis stretch. A texel at a face
    /// corner is √3 further away than one straight ahead at the same depth.
    #[test]
    fn radial_distance_undoes_the_projection_and_the_corner_stretch() {
        let (near, far) = (0.1f32, 2000.0f32);
        let depth_of = |t: f32| far * (t - near) / (t * (far - near));
        let straight = radial_distance(depth_of(10.0), near, far, 0.0, 0.0);
        assert!((straight - 10.0).abs() < 0.01, "{straight}");
        let corner = radial_distance(depth_of(10.0), near, far, 1.0, 1.0);
        assert!((corner - 10.0 * 3f32.sqrt()).abs() < 0.02, "{corner}");
        // A cleared depth buffer is open sky, not a surface at zero distance.
        assert!(radial_distance(1.0, near, far, 0.0, 0.0).is_infinite());
    }

    /// A probe that sees nothing at all still has to produce usable clearance —
    /// an open sky must read as "miles of room", not as "buried".
    #[test]
    fn an_open_probe_reads_as_clear() {
        let mut p = Probe::default();
        let mut stats = FaceStats::default();
        for face in FACES {
            accumulate_face(&mut p, &face, 4, &[[0.2; 3]; 16], &[f32::INFINITY; 16], &mut stats);
        }
        stats.finish(&mut p);
        assert!(p.nearest > 1000.0, "{}", p.nearest);
        assert!(p.mean > 1000.0, "{}", p.mean);
    }

    /// The face basis has to agree with the camera the editor builds, or the
    /// bake is rotated and nobody can tell why the light comes from the wrong
    /// side. Rotating the face's own forward by its rotation must give −Z back.
    #[test]
    fn face_rotations_look_along_their_face() {
        for face in FACES {
            let q = face.rotation();
            let looked = q * Vec3::NEG_Z;
            assert!(
                (looked - face.forward).length() < 1e-5,
                "{:?}: camera looks {looked:?}",
                face.forward
            );
            // The centre texel is the forward direction, and the four edges
            // stay on the same side of it.
            let c = face.texel_dir(0.0, 0.0);
            assert!((c - face.forward).length() < 1e-5, "{c:?}");
            for (u, v) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                assert!(face.texel_dir(u, v).dot(face.forward) > 0.5);
            }
        }
    }
}
