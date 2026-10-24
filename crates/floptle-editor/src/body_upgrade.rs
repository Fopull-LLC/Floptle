//! Upgrade step: sizes that started scaling with their node keep the world
//! size they had.
//!
//! Before 0.117 a sphere's or capsule's radius and height, a joint's anchors
//! and axis on a plain body, and a radial gravity volume's radius were read
//! as world units, whatever the node was scaled or turned to. From 0.117 they
//! are in the node's own space, like a box's half-extents always were. A
//! project made before that is rewritten once, so every one of those numbers
//! is divided back by the scale (and turned back by the rotation) its node had
//! in the file, and each body, joint and well ends up exactly the size and
//! place it was.
//!
//! What it does not touch: an assembly's parts and joints to an assembly,
//! whose anchors were already in the node's space; anything on a node at scale
//! 1 and no turn; and every file in which nothing needed changing, which is
//! left byte for byte.
//!
//! **Safe to run any number of times.** Dividing by a scale is not something
//! a file can tell has already happened, so every file this rewrites starts
//! with [`MARK`], and a file carrying it is never divided again. The upgrade
//! pass's version gate stops it running on a current project at all.

use std::path::{Path, PathBuf};

use floptle_core::math::{Quat, Vec3};
use floptle_scene::{BodyModeDoc, MatterDoc, NodeDoc, SceneDoc};

/// The first line of every file this step rewrote.
pub(crate) const MARK: &str = "// sizes in node space (upgraded for 0.117)";

/// The upgrade step (`upgrade::STEPS`): every scene and prefab under `root`
/// whose sizes need keeping is rewritten, the rest left byte for byte.
pub(crate) fn keep_world_sizes(root: &Path, _from: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let mut changed = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = floptle_vfs::read_dir(&dir) else { continue };
        for entry in rd {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(p);
                }
                continue;
            }
            if !name.ends_with(".ron") || name.ends_with(floptle_scene::VFX_EXT) || name == "project.ron" {
                continue;
            }
            let Ok(text) = floptle_vfs::read_to_string(&p) else { continue };
            if text.starts_with(MARK) {
                continue;
            }
            let next = if name.ends_with(".prefab.ron") { upgrade_prefab_text(&text) } else { upgrade_scene_text(&text) };
            if let Some(next) = next {
                floptle_scene::portable::write(&p, &format!("{MARK}\n{next}")).map_err(|e| format!("{}: {e}", p.display()))?;
                changed.push(p);
            }
        }
    }
    Ok(changed)
}

/// A scene file's text, rewritten, or `None` when nothing in it changes (or
/// it does not parse as a scene, which is some other step's business).
pub(crate) fn upgrade_scene_text(text: &str) -> Option<String> {
    let mut doc: SceneDoc = floptle_scene::from_ron(text).ok()?;
    if !upgrade_nodes(&mut doc.nodes) {
        return None;
    }
    floptle_scene::to_ron(&doc).ok()
}

/// A prefab file's text (a node list, maybe behind the clipboard's tag
/// line), rewritten, or `None` when nothing in it changes.
pub(crate) fn upgrade_prefab_text(text: &str) -> Option<String> {
    const TAG: &str = "//floptle-nodes-v1";
    let trimmed = text.trim_start();
    let (tag, body) = match trimmed.strip_prefix(TAG) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, trimmed),
    };
    let mut nodes: Vec<NodeDoc> = ron::from_str(body).ok()?;
    if !upgrade_nodes(&mut nodes) {
        return None;
    }
    let out = ron::ser::to_string_pretty(&nodes, ron::ser::PrettyConfig::default()).ok()?;
    Some(if tag { format!("{TAG}\n{out}") } else { out })
}

/// Each node's world scale and turn, from the file's own hierarchy.
fn world_poses(nodes: &[NodeDoc]) -> Vec<(Vec3, Quat)> {
    let parent_of = |i: usize| -> Option<usize> {
        match nodes[i].parent_id {
            Some(pid) => nodes.iter().position(|n| n.id == Some(pid)),
            None => nodes[i].parent.filter(|&p| p < nodes.len() && p != i),
        }
    };
    (0..nodes.len())
        .map(|i| {
            let (mut s, mut r) = (Vec3::ONE, Quat::IDENTITY);
            let mut cur = Some(i);
            for _ in 0..64 {
                let Some(c) = cur else { break };
                let t = &nodes[c].transform;
                s *= Vec3::from(t.scale);
                r = Quat::from_array(t.rotation).normalize() * r;
                cur = parent_of(c);
            }
            (s, r)
        })
        .collect()
}

/// Whether node `i` is an assembly's root or one of its parts: its joints
/// hang from a compound, whose anchors were always in the node's space.
fn in_assembly(nodes: &[NodeDoc], i: usize) -> bool {
    let root = |n: &NodeDoc| {
        n.rigidbody.as_ref().is_some_and(|rb| rb.assembly && rb.mode == BodyModeDoc::Dynamic)
    };
    let mut cur = Some(i);
    for _ in 0..64 {
        let Some(c) = cur else { break };
        if root(&nodes[c]) {
            return true;
        }
        cur = match nodes[c].parent_id {
            Some(pid) => nodes.iter().position(|n| n.id == Some(pid)),
            None => nodes[c].parent.filter(|&p| p < nodes.len() && p != c),
        };
    }
    false
}

/// A joint's node, and its new anchor, other anchor and axis.
type JointRewrite = (usize, [f32; 3], [f32; 3], [f32; 3]);

fn changed(a: f32, b: f32) -> bool {
    (a - b).abs() > 1e-7 * a.abs().max(1.0)
}

/// Rewrite every size and offset that now scales. True when anything moved.
fn upgrade_nodes(nodes: &mut [NodeDoc]) -> bool {
    let poses = world_poses(nodes);
    let mut any = false;
    // How a plain body turned its anchors before: by its node's turn when it
    // turned, not at all when it did not.
    let turned_body = |n: &NodeDoc| {
        n.rigidbody
            .as_ref()
            .is_some_and(|rb| rb.rotates && rb.mode != BodyModeDoc::Static && !rb.pushbox_only)
    };
    let is_body = |n: &NodeDoc| n.rigidbody.as_ref().is_some_and(|rb| rb.mode != BodyModeDoc::Static);
    // Joint ends are found by name, before anything is rewritten.
    let named: Vec<(String, usize)> = nodes.iter().enumerate().map(|(i, n)| (n.name.clone(), i)).collect();
    let mut joints: Vec<JointRewrite> = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        let Some(j) = n.joint.as_ref() else { continue };
        let (s, r) = poses[i];
        let mut anchor = j.anchor;
        if is_body(n) && !in_assembly(nodes, i) {
            // Old: in the body's frame, its node's turn when it turned.
            let old = if turned_body(n) { Vec3::from(j.anchor) } else { r.inverse() * Vec3::from(j.anchor) };
            anchor = (old / s).to_array();
        }
        let mut other_anchor = j.other_anchor;
        let mut axis = j.axis;
        if let Some(&(_, o)) = named.iter().find(|(name, o)| !j.other.is_empty() && *name == j.other && *o != i) {
            let (so, ro) = poses[o];
            let on = &nodes[o];
            if !in_assembly(nodes, o) {
                let a = Vec3::from(j.other_anchor);
                // A body at the other end read it in its own frame; a node
                // with no body, turned by the node but not scaled.
                other_anchor = if is_body(on) && !turned_body(on) {
                    ((ro.inverse() * a) / so).to_array()
                } else {
                    (a / so).to_array()
                };
                // The axis was in the other end's own frame: a turning body's
                // turn, or the world's for one that did not turn and for a
                // node with no body.
                if !(is_body(on) && turned_body(on)) {
                    axis = (ro.inverse() * Vec3::from(j.axis)).to_array();
                }
            }
        }
        joints.push((i, anchor, other_anchor, axis));
    }
    for (i, anchor, other_anchor, axis) in joints {
        let j = nodes[i].joint.as_mut().expect("collected from a joint");
        let moved = (0..3).any(|k| {
            changed(j.anchor[k], anchor[k]) || changed(j.other_anchor[k], other_anchor[k]) || changed(j.axis[k], axis[k])
        });
        if moved {
            j.anchor = anchor;
            j.other_anchor = other_anchor;
            j.axis = axis;
            any = true;
        }
    }
    for (i, n) in nodes.iter_mut().enumerate() {
        let s = poses[i].0.abs();
        if let Some(rb) = n.rigidbody.as_mut()
            && !rb.boxed
            && !rb.polygon
            && !rb.mesh
        {
            let k = if rb.capsule { s.x.max(s.z) } else { s.max_element() };
            if k > 1e-6 && changed(rb.radius, rb.radius / k) {
                rb.radius /= k;
                any = true;
            }
            if rb.capsule && s.y > 1e-6 && changed(rb.height, rb.height / s.y) {
                rb.height /= s.y;
                any = true;
            }
        }
        if let MatterDoc::GravityVolume { radial: true, radius, .. } = &mut n.matter {
            let k = s.max_element();
            if k > 1e-6 && changed(*radius, *radius / k) {
                *radius /= k;
                any = true;
            }
        }
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::DVec3;
    use floptle_core::{RigidBody, Transform, World};

    /// A 0.116-style scene: a ball on a node scaled 2 and turned, held by a
    /// rod to a hook, beside a well scaled 3.
    const OLD: &str = r#"(
    name: "old",
    nodes: [
        (
            name: "Hook",
            transform: (translation: (0.0, 8.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (2.0, 2.0, 2.0)),
            matter: Empty,
            scripts: [],
        ),
        (
            name: "Ball",
            transform: (translation: (0.0, 5.0, 0.0), rotation: (0.0, 0.0, 0.70710677, 0.70710677), scale: (2.0, 2.0, 2.0)),
            matter: Primitive(shape: Sphere, color: (1.0, 1.0, 1.0)),
            scripts: [],
            rigidbody: Some((radius: 0.9)),
            joint: Some((kind: "distance", other: "Hook", anchor: (0.0, 1.0, 0.0), other_anchor: (0.5, 0.0, 0.0))),
        ),
        (
            name: "Pill",
            transform: (translation: (4.0, 3.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (0.5, 1.5, 0.5)),
            matter: Primitive(shape: Capsule, color: (1.0, 1.0, 1.0)),
            scripts: [],
            rigidbody: Some((capsule: true, radius: 0.3, height: 1.8)),
        ),
        (
            name: "Well",
            transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (3.0, 3.0, 3.0)),
            matter: GravityVolume(radial: true, strength: 9.81, radius: 30.0),
            scripts: [],
        ),
    ],
)
"#;

    fn world_of(doc: &SceneDoc) -> World {
        let mut w = World::default();
        for n in &doc.nodes {
            let e = w.spawn();
            let t = &n.transform;
            w.insert(e, Transform {
                translation: DVec3::from(t.translation),
                rotation: Quat::from_array(t.rotation),
                scale: Vec3::from(t.scale),
            });
            if let Some(rb) = &n.rigidbody {
                w.insert(e, rb.to_rigidbody());
            }
        }
        w
    }

    /// Migrated, the old scene's bodies, rod ends and well are the world
    /// size and place they were when the engine read them unscaled.
    #[test]
    fn an_old_scene_keeps_its_world_sizes_and_anchors() {
        let new = upgrade_scene_text(OLD).expect("the old scene has sizes to keep");
        let doc = floptle_scene::from_ron(&new).unwrap();
        let w = world_of(&doc);
        let ents: Vec<_> = w.query::<RigidBody>().map(|(e, rb)| (e, *rb)).collect();
        let (ball, ball_rb) = ents[0];
        let (pill, pill_rb) = ents[1];
        let scale = |e| floptle_core::world_transform(&w, e).scale;
        assert!((ball_rb.world_radius(scale(ball)) - 0.9).abs() < 1e-5, "ball radius {}", ball_rb.world_radius(scale(ball)));
        assert!((pill_rb.world_radius(scale(pill)) - 0.3).abs() < 1e-5);
        assert!((pill_rb.world_height(scale(pill)) - 1.8).abs() < 1e-5);
        // The ball does not turn: before, its anchor was a world offset.
        let jb = doc.nodes[1].joint.as_ref().unwrap();
        let rot = Quat::from_array(doc.nodes[1].transform.rotation);
        let world_anchor = rot * (Vec3::from(jb.anchor) * Vec3::splat(2.0));
        assert!((world_anchor - Vec3::new(0.0, 1.0, 0.0)).length() < 1e-5, "{world_anchor:?}");
        // The hook has no body: its anchor was turned with it and not scaled.
        assert!((Vec3::from(jb.other_anchor) * 2.0 - Vec3::new(0.5, 0.0, 0.0)).length() < 1e-5);
        match &doc.nodes[3].matter {
            MatterDoc::GravityVolume { radius, .. } => assert!((radius * 3.0 - 30.0).abs() < 1e-4),
            _ => unreachable!(),
        }
    }

    /// The step on a project: the old scene and prefab are rewritten once,
    /// marked, and load; a second run changes nothing; a scene with nothing
    /// to keep and a file that is not a scene are left byte for byte.
    #[test]
    fn the_step_rewrites_once_and_leaves_the_rest_alone() {
        let dir = std::env::temp_dir().join(format!("floptle_body_upgrade_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scenes")).unwrap();
        std::fs::create_dir_all(dir.join("prefabs")).unwrap();
        std::fs::write(dir.join("scenes/old.ron"), OLD).unwrap();
        let plain = OLD.replace("scale: (2.0, 2.0, 2.0)", "scale: (1.0, 1.0, 1.0)")
            .replace("scale: (0.5, 1.5, 0.5)", "scale: (1.0, 1.0, 1.0)")
            .replace("scale: (3.0, 3.0, 3.0)", "scale: (1.0, 1.0, 1.0)")
            .replace("rotation: (0.0, 0.0, 0.70710677, 0.70710677)", "rotation: (0.0, 0.0, 0.0, 1.0)");
        std::fs::write(dir.join("scenes/plain.ron"), &plain).unwrap();
        let prefab = "[(name: \"Coin\", transform: (scale: (0.5, 0.5, 0.5)), matter: Empty, scripts: [], rigidbody: Some((radius: 0.5)))]";
        std::fs::write(dir.join("prefabs/coin.prefab.ron"), prefab).unwrap();
        let project = "(retro: true)";
        std::fs::write(dir.join("project.ron"), project).unwrap();
        let mut once = keep_world_sizes(&dir, Some("0.116.1")).unwrap();
        once.sort();
        assert_eq!(once.len(), 2, "{once:?}");
        let old_once = std::fs::read_to_string(dir.join("scenes/old.ron")).unwrap();
        assert!(old_once.starts_with(MARK));
        let doc = floptle_scene::from_ron(&old_once).unwrap();
        assert!((doc.nodes[1].rigidbody.as_ref().unwrap().radius - 0.45).abs() < 1e-6);
        let nodes = crate::prefab::load_prefab_docs(&dir.join("prefabs/coin.prefab.ron")).unwrap();
        assert!((nodes[0].rigidbody.as_ref().unwrap().radius - 1.0).abs() < 1e-6);
        assert!(keep_world_sizes(&dir, Some("0.116.1")).unwrap().is_empty(), "a second run rewrote something");
        assert_eq!(std::fs::read_to_string(dir.join("scenes/old.ron")).unwrap(), old_once);
        assert_eq!(std::fs::read_to_string(dir.join("scenes/plain.ron")).unwrap(), plain);
        assert_eq!(std::fs::read_to_string(dir.join("project.ron")).unwrap(), project);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scene with nothing scaled, and one already current, come back
    /// untouched; the step is gated by the project stamp, so it is done once.
    #[test]
    fn a_current_or_unscaled_scene_is_left_byte_for_byte() {
        let unscaled = OLD
            .replace("scale: (2.0, 2.0, 2.0)", "scale: (1.0, 1.0, 1.0)")
            .replace("scale: (0.5, 1.5, 0.5)", "scale: (1.0, 1.0, 1.0)")
            .replace("scale: (3.0, 3.0, 3.0)", "scale: (1.0, 1.0, 1.0)")
            .replace("rotation: (0.0, 0.0, 0.70710677, 0.70710677)", "rotation: (0.0, 0.0, 0.0, 1.0)");
        assert_eq!(upgrade_scene_text(&unscaled), None);
        let prefab = "//floptle-nodes-v1\n[(name: \"Coin\", transform: (scale: (0.5, 0.5, 0.5)), matter: Empty, scripts: [], rigidbody: Some((radius: 0.5)))]";
        let up = upgrade_prefab_text(prefab).unwrap();
        assert!(up.starts_with("//floptle-nodes-v1\n"));
        let nodes: Vec<NodeDoc> = ron::from_str(up.trim_start_matches("//floptle-nodes-v1\n")).unwrap();
        assert!((nodes[0].rigidbody.as_ref().unwrap().radius - 1.0).abs() < 1e-6);
        assert_eq!(upgrade_prefab_text("[(name: \"A\", transform: (), matter: Empty, scripts: [])]"), None);
    }
}
