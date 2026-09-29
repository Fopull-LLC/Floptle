//! What a script does to a pose after the animator has made it: a bone set
//! outright, a limb reaching a point (two-bone IK), a head turned toward
//! something (look-at). Every position here is in the skeleton's own space,
//! the space [`Skeleton::world_matrices`] answers in.

use crate::{Skeleton, TransformTRS};
use floptle_core::math::{Mat4, Quat, Vec3};

/// One adjustment, applied in order after the animator's pose.
#[derive(Clone, Debug, PartialEq)]
pub enum PoseOp {
    /// Set (`add: false`) or turn/shift by (`add: true`) a bone's local
    /// rotation and/or translation, blended in by `weight`.
    Local { node: usize, rot: Option<Quat>, pos: Option<Vec3>, add: bool, weight: f32 },
    /// Turn `root` and `mid` so `tip` reaches `target`, bending toward `pole`
    /// (a point) when there is one, else in the plane the limb already bends in.
    TwoBone { root: usize, mid: usize, tip: usize, target: Vec3, pole: Option<Vec3>, weight: f32 },
    /// Turn the chain so the last bone's `axis` (skeleton space, as the rig
    /// faces at rest) points at `target`, shared along the chain and no more
    /// than `limit` radians off where the animation had it.
    LookAt { chain: Vec<usize>, target: Vec3, axis: Vec3, limit: f32, weight: f32 },
}

fn rot(m: &Mat4) -> Quat {
    m.to_scale_rotation_translation().1.normalize()
}

fn pos(m: &Mat4) -> Vec3 {
    m.w_axis.truncate()
}

/// Apply `ops` to `pose` in order.
pub fn apply(skel: &Skeleton, pose: &mut [TransformTRS], ops: &[PoseOp]) {
    let mut world = Vec::new();
    for op in ops {
        match op {
            PoseOp::Local { node, rot: r, pos: p, add, weight } => {
                let Some(b) = pose.get_mut(*node) else { continue };
                let w = weight.clamp(0.0, 1.0);
                if let Some(r) = r {
                    let r = r.normalize();
                    b.r = if *add { b.r * Quat::IDENTITY.slerp(r, w) } else { b.r.slerp(r, w) }.normalize();
                }
                if let Some(p) = p {
                    b.t = if *add { b.t + *p * w } else { b.t.lerp(*p, w) };
                }
            }
            PoseOp::TwoBone { root, mid, tip, target, pole, weight } => {
                two_bone(skel, pose, &mut world, [*root, *mid, *tip], *target, *pole, *weight)
            }
            PoseOp::LookAt { chain, target, axis, limit, weight } => {
                look_at(skel, pose, &mut world, chain, *target, *axis, *limit, *weight)
            }
        }
    }
}

/// Two-bone IK in three steps, each measured afresh: bend the middle joint
/// until the tip is as far from the root as the target is (law of cosines),
/// swing the root so the tip points at the target, then twist the limb about
/// the root→target line toward the pole.
fn two_bone(
    skel: &Skeleton,
    pose: &mut [TransformTRS],
    world: &mut Vec<Mat4>,
    [root, mid, tip]: [usize; 3],
    t: Vec3,
    pole: Option<Vec3>,
    weight: f32,
) {
    const EPS: f32 = 1e-4;
    if root >= pose.len() || mid >= pose.len() || tip >= pose.len() {
        return;
    }
    let (orig_a, orig_b) = (pose[root].r, pose[mid].r);
    // A local rotation that applies world rotation `r` to a bone whose world
    // rotation is `gr`.
    let spin = |lr: Quat, gr: Quat, r: Quat| (lr * (gr.inverse() * r * gr)).normalize();

    // 1. Bend: the elbow angle that makes |tip − root| the target's distance.
    skel.world_matrices(pose, world);
    let (a, b, c) = (pos(&world[root]), pos(&world[mid]), pos(&world[tip]));
    let (lab, lcb) = ((b - a).length(), (c - b).length());
    if lab < EPS || lcb < EPS {
        return;
    }
    let lat = (t - a).length().clamp((lab - lcb).abs() + EPS, lab + lcb - EPS);
    let acos = |x: f32| x.clamp(-1.0, 1.0).acos();
    let now = acos((a - b).normalize_or_zero().dot((c - b).normalize_or_zero()));
    let want = acos((lab * lab + lcb * lcb - lat * lat) / (2.0 * lab * lcb));
    // The plane the limb bends in: its own, or (straight, with no plane) the
    // one through the pole, else any.
    let bend = (a - b)
        .cross(c - b)
        .try_normalize()
        .or_else(|| pole.and_then(|p| (c - a).cross(p - a).try_normalize()).map(|n| -n))
        .unwrap_or_else(|| (c - a).normalize_or_zero().any_orthonormal_vector());
    pose[mid].r = spin(pose[mid].r, rot(&world[mid]), Quat::from_axis_angle(bend, now - want));

    // 2. Reach: swing the root so the tip lies on the line to the target.
    skel.world_matrices(pose, world);
    let (a, c) = (pos(&world[root]), pos(&world[tip]));
    if let (Some(from), Some(to)) = ((c - a).try_normalize(), (t - a).try_normalize()) {
        pose[root].r = spin(pose[root].r, rot(&world[root]), Quat::from_rotation_arc(from, to));
    }

    // 3. Twist toward the pole, about the root→target line (the tip stays put).
    if let Some(p) = pole {
        skel.world_matrices(pose, world);
        let (a, b) = (pos(&world[root]), pos(&world[mid]));
        if let Some(axis) = (t - a).try_normalize() {
            let flat = |v: Vec3| v - axis * v.dot(axis);
            let (vb, vp) = (flat(b - a), flat(p - a));
            if vb.length_squared() > EPS * EPS && vp.length_squared() > EPS * EPS {
                let angle = vb.cross(vp).dot(axis).atan2(vb.dot(vp));
                pose[root].r = spin(pose[root].r, rot(&world[root]), Quat::from_axis_angle(axis, angle));
            }
        }
    }
    let w = weight.clamp(0.0, 1.0);
    pose[root].r = orig_a.slerp(pose[root].r, w).normalize();
    pose[mid].r = orig_b.slerp(pose[mid].r, w).normalize();
}

#[allow(clippy::too_many_arguments)]
fn look_at(
    skel: &Skeleton,
    pose: &mut [TransformTRS],
    world: &mut Vec<Mat4>,
    chain: &[usize],
    target: Vec3,
    axis: Vec3,
    limit: f32,
    weight: f32,
) {
    let Some(&end) = chain.last() else { return };
    if chain.iter().any(|&i| i >= pose.len()) {
        return;
    }
    // The axis in the end bone's own frame, taken at rest: "forward" is the
    // way the character faces in its bind pose, whatever the bone's axes are.
    let mut rest_world = Vec::new();
    skel.world_matrices(&skel.rest_pose(), &mut rest_world);
    let local_axis = (rot(&rest_world[end]).inverse() * axis).normalize_or_zero();
    if local_axis == Vec3::ZERO {
        return;
    }
    let orig: Vec<Quat> = chain.iter().map(|&i| pose[i].r).collect();
    skel.world_matrices(pose, world);
    let base_fwd = (rot(&world[end]) * local_axis).normalize_or_zero();
    let Some(want0) = (target - pos(&world[end])).try_normalize() else { return };
    // No more than `limit` off where the animation had the head.
    let arc = Quat::from_rotation_arc(base_fwd, want0);
    let (arc_axis, arc_angle) = arc.to_axis_angle();
    let want = if arc_angle > limit.max(0.0) {
        Quat::from_axis_angle(arc_axis, limit.max(0.0)) * base_fwd
    } else {
        want0
    };
    let n = chain.len();
    for (k, &bone) in chain.iter().enumerate() {
        skel.world_matrices(pose, world);
        let fwd = (rot(&world[end]) * local_axis).normalize_or_zero();
        let full = Quat::from_rotation_arc(fwd, want);
        let share = Quat::IDENTITY.slerp(full, 1.0 / (n - k) as f32);
        let gr = rot(&world[bone]);
        pose[bone].r = (pose[bone].r * (gr.inverse() * share * gr)).normalize();
    }
    let w = weight.clamp(0.0, 1.0);
    for (&bone, o) in chain.iter().zip(orig) {
        pose[bone].r = o.slerp(pose[bone].r, w).normalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SkelNode;

    /// A three-bone arm along +X, each bone one unit long, and a head on top.
    fn arm() -> Skeleton {
        let node = |name: &str, parent: Option<usize>, t: Vec3| SkelNode {
            name: name.into(),
            parent,
            rest: TransformTRS { t, ..TransformTRS::IDENTITY },
            pivot: Vec3::ZERO,
        };
        Skeleton::new(vec![
            node("Shoulder", None, Vec3::ZERO),
            node("Elbow", Some(0), Vec3::X),
            node("Hand", Some(1), Vec3::X),
            node("Neck", None, Vec3::new(0.0, 2.0, 0.0)),
            node("Head", Some(3), Vec3::new(0.0, 0.3, 0.0)),
        ])
    }

    fn world_of(skel: &Skeleton, pose: &[TransformTRS]) -> Vec<Mat4> {
        let mut w = Vec::new();
        skel.world_matrices(pose, &mut w);
        w
    }

    /// **The hand reaches the target**, the limb stays its own length, and a
    /// target out of reach gets the arm straight at it.
    #[test]
    fn two_bone_ik_puts_the_tip_on_a_reachable_target() {
        let skel = arm();
        for target in [Vec3::new(1.2, 0.8, 0.0), Vec3::new(0.3, -1.1, 0.9), Vec3::new(-0.5, 0.5, 1.2)] {
            let mut pose = skel.rest_pose();
            apply(&skel, &mut pose, &[PoseOp::TwoBone { root: 0, mid: 1, tip: 2, target, pole: None, weight: 1.0 }]);
            let w = world_of(&skel, &pose);
            assert!((pos(&w[2]) - target).length() < 1e-3, "tip at {} for {target}", pos(&w[2]));
            assert!(((pos(&w[1]) - pos(&w[0])).length() - 1.0).abs() < 1e-4, "the upper arm kept its length");
        }
        let mut pose = skel.rest_pose();
        let far = Vec3::new(0.0, 5.0, 0.0);
        apply(&skel, &mut pose, &[PoseOp::TwoBone { root: 0, mid: 1, tip: 2, target: far, pole: None, weight: 1.0 }]);
        let w = world_of(&skel, &pose);
        assert!((pos(&w[2]) - Vec3::new(0.0, 2.0, 0.0)).length() < 1e-2, "straight at it: {}", pos(&w[2]));
    }

    /// The elbow bends toward the pole, either way, and weight blends it in.
    #[test]
    fn the_pole_decides_which_way_the_elbow_bends() {
        let skel = arm();
        let target = Vec3::new(1.4, 0.0, 0.0);
        let elbow_z = |pole: Vec3| {
            let mut pose = skel.rest_pose();
            apply(&skel, &mut pose, &[PoseOp::TwoBone { root: 0, mid: 1, tip: 2, target, pole: Some(pole), weight: 1.0 }]);
            let w = world_of(&skel, &pose);
            assert!((pos(&w[2]) - target).length() < 1e-3, "tip at {}", pos(&w[2]));
            pos(&w[1]).z
        };
        assert!(elbow_z(Vec3::new(0.7, 0.0, 3.0)) > 0.5, "toward +Z");
        assert!(elbow_z(Vec3::new(0.7, 0.0, -3.0)) < -0.5, "toward -Z");
        let mut half = skel.rest_pose();
        apply(&skel, &mut half, &[PoseOp::TwoBone { root: 0, mid: 1, tip: 2, target: Vec3::new(0.0, 1.5, 0.0), pole: None, weight: 0.5 }]);
        let tip = pos(&world_of(&skel, &half)[2]);
        assert!(tip.x > 0.2 && tip.y > 0.2, "half weight is half way: {tip}");
    }

    /// The head turns to face the target, shared along the chain, and never
    /// more than the limit.
    #[test]
    fn look_at_turns_the_chain_toward_the_target_within_its_limit() {
        let skel = arm();
        let fwd = |pose: &[TransformTRS]| rot(&world_of(&skel, pose)[4]) * Vec3::Z;
        let target = Vec3::new(5.0, 2.3, 0.0);
        let mut pose = skel.rest_pose();
        apply(&skel, &mut pose, &[PoseOp::LookAt { chain: vec![3, 4], target, axis: Vec3::Z, limit: 3.0, weight: 1.0 }]);
        let want = (target - Vec3::new(0.0, 2.3, 0.0)).normalize();
        assert!(fwd(&pose).dot(want) > 0.999, "facing the target: {}", fwd(&pose));
        assert!(pose[3].r.angle_between(Quat::IDENTITY) > 0.3, "the neck took a share of the turn");
        let mut limited = skel.rest_pose();
        apply(&skel, &mut limited, &[PoseOp::LookAt { chain: vec![3, 4], target, axis: Vec3::Z, limit: 0.5, weight: 1.0 }]);
        let turned = fwd(&limited).angle_between(Vec3::Z);
        assert!((turned - 0.5).abs() < 1e-2, "no more than the limit: {turned}");
    }

    /// A bone set outright replaces what the animator made, by its weight.
    #[test]
    fn a_local_override_replaces_the_bone_by_its_weight() {
        let skel = arm();
        let mut pose = skel.rest_pose();
        let q = Quat::from_rotation_y(1.0);
        apply(&skel, &mut pose, &[PoseOp::Local { node: 1, rot: Some(q), pos: Some(Vec3::new(2.0, 0.0, 0.0)), add: false, weight: 0.5 }]);
        assert!(pose[1].r.angle_between(Quat::from_rotation_y(0.5)) < 1e-4);
        assert!((pose[1].t.x - 1.5).abs() < 1e-5);
        // …and turned or shifted by it, on top of what the animator made.
        apply(&skel, &mut pose, &[PoseOp::Local { node: 1, rot: Some(q), pos: Some(Vec3::new(0.0, -0.4, 0.0)), add: true, weight: 1.0 }]);
        assert!(pose[1].r.angle_between(Quat::from_rotation_y(1.5)) < 1e-4);
        assert!((pose[1].t.y + 0.4).abs() < 1e-5 && (pose[1].t.x - 1.5).abs() < 1e-5);
    }
}
