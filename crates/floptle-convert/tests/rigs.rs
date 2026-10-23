//! **A rigged, animated FBX plays in the engine as it played in the source.**
//!
//! The fixture is a bar two metres tall bound to two bones, Root at its foot
//! and Tip at its middle, with a clip that swings Tip ninety degrees about Z
//! over one second — and a small rigid part hung from Tip, which is how a
//! sword rides a hand. Where every vertex should be at any moment can be
//! worked out on paper, so the test checks the converted model against the
//! arithmetic rather than against another program's opinion of it.
//!
//! It is converted twice, once written in metres and once in centimetres: the
//! unit change has to reach bone translations, inverse binds and animation
//! keys alike, and a miss in any one of them bends the bar somewhere else.

mod support;

use std::path::Path;

use support::*;

/// The fixture, every length times `k` (1 for metres, 100 for centimetres).
fn bar_fbx(path: &Path, k: f64) {
    let a = 0.1 * k;
    let mut pos = Vec::new();
    for y in [0.0, 1.0, 2.0] {
        let y = y * k;
        pos.extend_from_slice(&[[-a, y, -a], [a, y, -a], [a, y, a], [-a, y, a]]);
    }
    let mut quads = Vec::new();
    for r in 0..2i64 {
        for i in 0..4i64 {
            let j = (i + 1) % 4;
            quads.push([4 * r + i, 4 * r + j, 4 * (r + 1) + j, 4 * (r + 1) + i]);
        }
    }
    quads.push([0, 3, 2, 1]);
    quads.push([8, 9, 10, 11]);
    let ident = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
    let at = |y: f64| {
        let mut m = ident;
        m[13] = y;
        m
    };
    let cluster = |id: u64, name: &str, idx: &[i64], w: &[f64], link_y: f64| {
        format!(
            "\tDeformer: {id}, \"SubDeformer::{name}\", \"Cluster\" {{\n\t\tVersion: 100\n\t\tIndexes: {}\n\t\tWeights: {}\n\t\tTransform: {}\n\t\tTransformLink: {}\n\t}}",
            iarr(idx),
            arr(w),
            arr(&at(-link_y)),
            arr(&at(link_y))
        )
    };
    let flag = [[0.0, 0.2 * k, 0.0], [0.5 * k, 0.2 * k, 0.0], [0.5 * k, 0.6 * k, 0.0], [0.0, 0.6 * k, 0.0]];
    let mut f = Fbx::new(if k == 1.0 { 100.0 } else { 1.0 });
    f.object(&geometry(100, "Bar", &pos, &quads, false, &[]))
        .object(&model(200, "Bar", "Mesh", [0.0; 3]))
        .object(&geometry(110, "Flag", &flag, &[[0, 1, 2, 3]], false, &[]))
        .object(&model(210, "Flag", "Mesh", [0.3 * k, 0.0, 0.0]))
        .object(&model(300, "Root", "LimbNode", [0.0; 3]))
        .object(&model(301, "Tip", "LimbNode", [0.0, k, 0.0]))
        .object("\tNodeAttribute: 400, \"NodeAttribute::Root\", \"LimbNode\" {\n\t\tTypeFlags: \"Skeleton\"\n\t}")
        .object("\tNodeAttribute: 401, \"NodeAttribute::Tip\", \"LimbNode\" {\n\t\tTypeFlags: \"Skeleton\"\n\t}")
        .object("\tDeformer: 500, \"Deformer::Skin\", \"Skin\" {\n\t\tVersion: 101\n\t\tLink_DeformAcuracy: 50\n\t}")
        .object(&cluster(501, "Root", &[0, 1, 2, 3, 4, 5, 6, 7], &[1.0, 1.0, 1.0, 1.0, 0.5, 0.5, 0.5, 0.5], 0.0))
        .object(&cluster(502, "Tip", &[4, 5, 6, 7, 8, 9, 10, 11], &[0.5, 0.5, 0.5, 0.5, 1.0, 1.0, 1.0, 1.0], k))
        .object(&format!(
            "\tAnimationStack: 600, \"AnimStack::Swing\", \"\" {{\n\t\tProperties70:  {{\n\
             \t\t\tP: \"LocalStart\", \"KTime\", \"Time\", \"\",0\n\t\t\tP: \"LocalStop\", \"KTime\", \"Time\", \"\",{SECOND}\n\
             \t\t\tP: \"ReferenceStart\", \"KTime\", \"Time\", \"\",0\n\t\t\tP: \"ReferenceStop\", \"KTime\", \"Time\", \"\",{SECOND}\n\t\t}}\n\t}}"
        ))
        .object("\tAnimationLayer: 601, \"AnimLayer::BaseLayer\", \"\" {\n\t}")
        .object(
            "\tAnimationCurveNode: 602, \"AnimCurveNode::R\", \"\" {\n\t\tProperties70:  {\n\
             \t\t\tP: \"d|X\", \"Number\", \"\", \"A\",0\n\t\t\tP: \"d|Y\", \"Number\", \"\", \"A\",0\n\t\t\tP: \"d|Z\", \"Number\", \"\", \"A\",0\n\t\t}\n\t}",
        )
        .object(&format!(
            "\tAnimationCurve: 603, \"AnimCurve::\", \"\" {{\n\t\tDefault: 0\n\t\tKeyVer: 4009\n\t\tKeyTime: *2 {{\n\t\t\ta: 0,{SECOND}\n\t\t}}\n\
             \t\tKeyValueFloat: *2 {{\n\t\t\ta: 0,90\n\t\t}}\n\t\tKeyAttrFlags: *1 {{\n\t\t\ta: 4\n\t\t}}\n\
             \t\tKeyAttrDataFloat: *4 {{\n\t\t\ta: 0,0,0,0\n\t\t}}\n\t\tKeyAttrRefCount: *1 {{\n\t\t\ta: 2\n\t\t}}\n\t}}"
        ))
        .oo(200, 0)
        .oo(100, 200)
        .oo(300, 0)
        .oo(301, 300)
        .oo(210, 301)
        .oo(110, 210)
        .oo(400, 300)
        .oo(401, 301)
        .oo(500, 100)
        .oo(501, 500)
        .oo(502, 500)
        .oo(300, 501)
        .oo(301, 502)
        .oo(601, 600)
        .oo(602, 601)
        .op(602, 301, "Lcl Rotation")
        .op(603, 602, "d|Z");
    f.write(path);
}

/// Where a point ends up when Tip has swung `deg` degrees about Z: around
/// (0, 1, 0), anticlockwise seen from +Z.
fn swung(p: [f32; 3], deg: f32) -> [f32; 3] {
    let (s, c) = deg.to_radians().sin_cos();
    let (x, y) = (p[0], p[1] - 1.0);
    [x * c - y * s, 1.0 + x * s + y * c, p[2]]
}

fn near(a: [f32; 3], b: [f32; 3], what: &str) {
    let d = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    assert!(d < 2e-3, "{what}: at {a:?}, expected {b:?}");
}

#[test]
fn a_skinned_animated_fbx_plays_its_clip_in_the_engine() {
    for (k, label) in [(1.0, "metres"), (100.0, "centimetres")] {
        let dir = scratch(&format!("rig-{label}"));
        let src = dir.join("bar.fbx");
        bar_fbx(&src, k);
        let (out, report) = floptle_convert::convert_file(&src).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(report.animations, ["Swing"], "{label}: the clip came across by name");
        assert_eq!(report.skins, 1, "{label}: one skinned mesh");
        assert!(!report.dropped.iter().any(|d| d.contains("animation") || d.contains("skin")), "{label}: {:?}", report.dropped);

        let rig = floptle_assets::import_rigged(&out).unwrap().expect("a rigged model keeps its tree");
        assert_eq!(rig.clips.len(), 1);
        let clip = &rig.clips[0];
        assert!((clip.duration - 1.0).abs() < 0.05, "{label}: one second long, got {}", clip.duration);
        let bar = rig.parts.iter().find(|p| p.skin.is_some()).expect("the bar is skinned");
        let skin = bar.skin.as_ref().unwrap();
        let flag = rig.parts.iter().find(|p| p.skin.is_none()).expect("the flag is a rigid part");

        for (t, deg) in [(0.0f32, 0.0f32), (0.5, 45.0), (1.0, 90.0)] {
            let mut pose = rig.skeleton.rest_pose();
            clip.sample_into(t, &mut pose);
            let mut world = Vec::new();
            rig.skeleton.world_matrices(&pose, &mut world);
            // Skin every bar vertex as the GPU does, and check the ones held
            // wholly by one bone.
            for (i, v) in bar.mesh.vertices.iter().enumerate() {
                let (j, w) = (skin.joints[i], skin.weights[i]);
                let mut p = [0.0f32; 3];
                for s in 0..4 {
                    if w[s] == 0.0 {
                        continue;
                    }
                    let m = world[skin.joint_nodes[j[s] as usize]] * skin.inverse_bind[j[s] as usize];
                    let q = m.transform_point3(v.pos.into());
                    for c in 0..3 {
                        p[c] += q[c] * w[s];
                    }
                }
                let rest = v.pos;
                if (rest[1] - 2.0).abs() < 1e-4 {
                    near(p, swung(rest, deg), &format!("{label}: a top vertex at t={t}"));
                } else if rest[1].abs() < 1e-4 {
                    near(p, rest, &format!("{label}: a foot vertex at t={t}"));
                }
            }
            // The flag rides Tip: rest point (0.3, 1.2, 0) swings with it.
            let m = world[flag.node];
            let lowest = flag
                .mesh
                .vertices
                .iter()
                .map(|v| m.transform_point3(v.pos.into()).to_array())
                .find(|p| p.iter().all(|c| c.is_finite()))
                .unwrap();
            let rest_corners = [[0.3, 1.2, 0.0], [0.8, 1.2, 0.0], [0.8, 1.6, 0.0], [0.3, 1.6, 0.0]];
            assert!(
                rest_corners.iter().any(|c| {
                    let e = swung(*c, deg);
                    (0..3).all(|k| (e[k] - lowest[k]).abs() < 2e-3)
                }),
                "{label}: the flag at t={t} is at {lowest:?}, not on Tip"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
