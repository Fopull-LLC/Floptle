//! Where the ground is below a point: many points at once, straight off the
//! field.
//!
//! A game asks this thousands of times a frame (where a flood's front can
//! go, where simulated water stands, where a prop lands). A ray against the
//! collision meshes misses where a chunk is not meshed yet; a march in Lua
//! costs tens of milliseconds for a few thousand points. This traces the
//! field itself, a few samples a point.

use floptle_core::math::DVec3;

use crate::ChunkField;

/// Which way is down for [`ChunkField::surface_below`]: one direction for
/// every point, or one each.
#[derive(Clone, Copy, Debug)]
pub enum Dirs<'a> {
    One(DVec3),
    Each(&'a [DVec3]),
}

/// [`ChunkField::surface_below`]'s flags: the surface found crosses ground
/// not generated yet, a stand-in that will move when it is.
pub const SURFACE_STAND_IN: u8 = 1;

impl ChunkField {
    /// For each of `points` (in the field's own units), how far along its
    /// direction the surface is, into `out`: positive below the point,
    /// negative when the point is in the ground and the surface is above it,
    /// `NaN` when there is none within `max_dist` either way. Each point is
    /// a sphere trace through the field, so a point near the ground costs a
    /// few samples and one in open air a step a band. `flags`, when given,
    /// gets [`SURFACE_STAND_IN`] for a surface on ground not generated yet.
    pub fn surface_below(&self, points: &[DVec3], dirs: Dirs, max_dist: f64, out: &mut [f64], mut flags: Option<&mut [u8]>) {
        let voxel = self.voxel() as f64;
        let eps = voxel * 1e-2;
        for (k, &p) in points.iter().enumerate().take(out.len()) {
            let dir = match dirs {
                Dirs::One(d) => d,
                Dirs::Each(d) => d.get(k).copied().unwrap_or(DVec3::NEG_Y),
            }
            .normalize_or_zero();
            let at = |t: f64| self.d((p + dir * t).as_vec3()) as f64;
            let mut t = 0.0;
            let mut hit = f64::NAN;
            for _ in 0..256 {
                let d = at(t);
                if d.abs() < eps {
                    hit = t;
                    break;
                }
                // Out of the ground, down by the distance; in it, back up.
                // Never less than a hundredth of a voxel, so a flat stretch
                // of the field still moves on.
                t += if d > 0.0 { d.max(eps) } else { d.min(-eps) };
                if t.abs() > max_dist {
                    break;
                }
            }
            out[k] = hit;
            if let Some(f) = flags.as_deref_mut()
                && let Some(slot) = f.get_mut(k)
            {
                *slot = 0;
                if hit.is_finite() {
                    let from = p + dir * (hit - 4.0 * voxel);
                    if self.pending_along(from.as_vec3(), dir.as_vec3(), (8.0 * voxel) as f32, 0.0).is_some() {
                        *slot = SURFACE_STAND_IN;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::Vec3;

    /// **The ground below many points, a few samples each.** Over a slab
    /// with its top at 2: from 10 m up it is 8 m down, from in the slab it
    /// is above, past the reach it is not found; 10,000 points over ground
    /// within a voxel or two of them take a few milliseconds.
    #[test]
    fn surface_below_finds_the_ground_under_each_point() {
        let mut f = ChunkField::new(0.5);
        f.fill_slab(Vec3::new(-60.0, -10.0, -60.0), Vec3::new(60.0, 4.0, 60.0), 2.0, [0.5; 3]);
        let pts = [DVec3::new(0.0, 10.0, 0.0), DVec3::new(3.0, 1.0, 3.0), DVec3::new(0.0, 40.0, 0.0)];
        let mut out = [0.0; 3];
        f.surface_below(&pts, Dirs::One(DVec3::NEG_Y), 20.0, &mut out, None);
        assert!((out[0] - 8.0).abs() < 0.02 && (out[1] + 1.0).abs() < 0.02 && out[2].is_nan(), "{out:?}");
        let each: Vec<DVec3> = (0..10_000).map(|k| DVec3::new((k % 100) as f64 - 50.0, 2.6, (k / 100) as f64 - 50.0)).collect();
        let mut far = vec![0.0; each.len()];
        let mut flags = vec![9u8; each.len()];
        let t = std::time::Instant::now();
        f.surface_below(&each, Dirs::One(DVec3::NEG_Y), 10.0, &mut far, Some(&mut flags));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(far.iter().all(|d| (d - 0.6).abs() < 0.02), "{:?}", &far[..4]);
        assert!(flags.iter().all(|f| *f == 0), "ground that is all generated reads as a stand-in");
        println!("surface_below: 10,000 points in {ms:.2} ms");
        assert!(ms < 20.0, "10,000 points took {ms:.1} ms");
    }
}
