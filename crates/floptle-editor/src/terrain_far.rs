//! What a planet draws as while its ground is still arriving.
//!
//! A planet the camera was just put a few kilometres from has thousands of
//! chunks to mesh, nearest first, so the ground facing the camera lands in a
//! second or two and its rim, the chunks along the limb, many seconds later.
//! Until they land, the body's streaming backstop fills the holes: one shaded
//! ball. Close to the ground that ball is the body's Occluder, the ball it
//! declares wholly inside itself, so it can never stand out of real ground
//! the player is looking along. From afar that ball is far too small: a
//! planet whose Occluder is six tenths of its radius was drawn as a rough
//! rock six tenths of its size, inside anything drawn at its surface (a sea,
//! an atmosphere), until its limb had streamed in. So from afar the ball
//! grows to the ground's floor, the lowest of its surface, and the planet's
//! silhouette is where its ground is from the first frame.

use floptle_core::math::Vec3;

/// Rays along each edge of a cube face that [`ground_floor`] reads the
/// ground along: 384 in all, one every 15° or so.
const FLOOR_RAYS: usize = 8;

/// Above its floor, in metres, the camera starts seeing a body's backstop
/// grow from its Occluder ball: below this the player is near enough the
/// ground for a ball standing in a valley to show.
const BACKSTOP_NEAR_M: f64 = 120.0;
/// …and from this high the backstop is at the floor.
const BACKSTOP_FAR_M: f64 = 480.0;

/// The bottom of a round terrain's ground, in its field's units: the least
/// radius at which rays toward its centre, from every side, meet its surface,
/// less two voxels for ground lower between the rays. A narrow ravine the
/// rays miss can lie under it, which only matters near the ground, where
/// the backstop is the Occluder ball instead. 0 when no ray meets ground.
///
/// The rays start at the farthest corner of the chunks that hold ground, so
/// they march only the last chunk or so down to it: a few milliseconds for
/// a kilometre planet, once as it starts streaming.
pub(crate) fn ground_floor(field: &floptle_field::ChunkField) -> f32 {
    let size = floptle_field::CHUNK as f32 * field.voxel();
    let reach = field
        .chunk_coords()
        .iter()
        .map(|c| {
            let lo = Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) * size;
            lo.abs().max((lo + Vec3::splat(size)).abs()).length()
        })
        .fold(0.0f32, f32::max)
        + field.voxel();
    let mut low = f32::INFINITY;
    for face in 0..6 {
        for i in 0..FLOOR_RAYS {
            for j in 0..FLOOR_RAYS {
                let u = -1.0 + (i as f32 + 0.5) * 2.0 / FLOOR_RAYS as f32;
                let v = -1.0 + (j as f32 + 0.5) * 2.0 / FLOOR_RAYS as f32;
                let d = cube_dir(face, u, v);
                if let Some(hit) = field.raycast(d * reach, -d, reach) {
                    low = low.min(hit.length());
                }
            }
        }
    }
    if low.is_finite() { (low - 2.0 * field.voxel()).max(0.0) } else { 0.0 }
}

/// The direction through point (`u`, `v`), each in −1..1, of cube face `face`.
fn cube_dir(face: usize, u: f32, v: f32) -> Vec3 {
    match face {
        0 => Vec3::new(1.0, u, v),
        1 => Vec3::new(-1.0, u, v),
        2 => Vec3::new(u, 1.0, v),
        3 => Vec3::new(u, -1.0, v),
        4 => Vec3::new(u, v, 1.0),
        _ => Vec3::new(u, v, -1.0),
    }
    .normalize()
}

/// The radius of a body's streaming backstop, in world units, with the
/// camera `cam_dist` from its centre: its Occluder ball (`occluder`, 0 for
/// none) near the ground, its floor (`floor`) from afar, and between the
/// two in proportion to the camera's height over the floor, so it never
/// jumps. `None` while the backstop is the Occluder ball itself, so the
/// caller draws that exactly as it always has.
pub(crate) fn backstop_radius(occluder: f64, floor: f64, cam_dist: f64) -> Option<f64> {
    if floor <= occluder {
        return None;
    }
    let t = ((cam_dist - floor - BACKSTOP_NEAR_M) / (BACKSTOP_FAR_M - BACKSTOP_NEAR_M)).clamp(0.0, 1.0);
    if t <= 0.0 {
        return None;
    }
    Some(occluder + (floor - occluder) * t)
}

/// Whether a ball (centre `p`, radius `rc`, both camera-relative) is wholly
/// hidden behind a solid ball (centre `c`, radius `big_r`) the camera is
/// outside of. Exact point-behind-sphere against the solid ball shrunk by
/// `rc`, so anything that peeks past the limb counts as seen.
pub(crate) fn behind_ball(p: Vec3, rc: f32, c: Vec3, big_r: f32) -> bool {
    let r_eff = big_r - rc;
    if r_eff <= 0.0 || c.length_squared() <= big_r * big_r {
        return false; // too small, or the camera is inside it (caves)
    }
    let vlen = p.length().max(1e-6);
    let dir = p / vlen;
    let tc = c.dot(dir); // the ball's centre's depth along the ray to `p`
    if tc <= 0.0 {
        return false; // behind the camera along this ray
    }
    let d2 = c.length_squared() - tc * tc;
    if d2 >= r_eff * r_eff {
        return false; // the ray misses the shrunk ball
    }
    vlen > tc + (r_eff * r_eff - d2).sqrt() // past its FAR surface
}

#[cfg(test)]
mod tests;
