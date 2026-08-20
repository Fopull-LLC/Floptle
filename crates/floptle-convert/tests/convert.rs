//! What a model converter has to get right, checked against real exporter
//! output rather than against files written to match the parser.
//!
//! Every test here reads the `.glb` back with the `gltf` crate — the same
//! library the engine imports with — so "it converted" means "a glTF reader
//! accepts it", not "it did not panic". A converter that writes a file only its
//! own author can open has done nothing.

use std::path::{Path, PathBuf};

fn models() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models")
}

/// Bounding size, triangle count, signed volume, and how many triangles wind
/// the way their own normals point.
///
/// The signed volume is the useful one: for a closed mesh it is positive when
/// the triangles wind counter-clockwise seen from outside, which is glTF's
/// front face. A model with every face backwards has the same size and triangle
/// count as a correct one and a *negative* volume — and is invisible from
/// outside in any renderer.
struct Shape {
    tris: usize,
    size: [f32; 3],
    volume: f64,
    normals_agree: f32,
    has_uv: bool,
    has_color: bool,
    meshes: usize,
    uvs: Vec<[f32; 2]>,
}

fn inspect(bytes: &[u8]) -> Shape {
    let (doc, buffers, _) = gltf::import_slice(bytes).expect("output is readable glTF");
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    let (mut tris, mut volume) = (0usize, 0f64);
    let (mut agree, mut total) = (0i64, 0i64);
    let (mut has_uv, mut has_color) = (false, false);
    let mut uvs: Vec<[f32; 2]> = Vec::new();

    for mesh in doc.meshes() {
        for prim in mesh.primitives() {
            let r = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let ps: Vec<[f32; 3]> = r.read_positions().map(|i| i.collect()).unwrap_or_default();
            let ns: Vec<[f32; 3]> = r.read_normals().map(|i| i.collect()).unwrap_or_default();
            let ix: Vec<u32> =
                r.read_indices().map(|i| i.into_u32().collect()).unwrap_or_default();
            if let Some(t) = r.read_tex_coords(0) {
                has_uv = true;
                uvs.extend(t.into_f32());
            }
            has_color |= r.read_colors(0).is_some();

            for p in &ps {
                for k in 0..3 {
                    lo[k] = lo[k].min(p[k]);
                    hi[k] = hi[k].max(p[k]);
                }
            }
            tris += ix.len() / 3;
            for t in ix.as_chunks::<3>().0 {
                let (a, b, c) =
                    (ps[t[0] as usize], ps[t[1] as usize], ps[t[2] as usize]);
                volume += (a[0] as f64 * (b[1] as f64 * c[2] as f64 - b[2] as f64 * c[1] as f64)
                    - a[1] as f64 * (b[0] as f64 * c[2] as f64 - b[2] as f64 * c[0] as f64)
                    + a[2] as f64 * (b[0] as f64 * c[1] as f64 - b[1] as f64 * c[0] as f64))
                    / 6.0;
                if !ns.is_empty() {
                    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
                    let g = [
                        u[1] * v[2] - u[2] * v[1],
                        u[2] * v[0] - u[0] * v[2],
                        u[0] * v[1] - u[1] * v[0],
                    ];
                    let n = ns[t[0] as usize];
                    total += 1;
                    if g[0] * n[0] + g[1] * n[1] + g[2] * n[2] > 0.0 {
                        agree += 1;
                    }
                }
            }
        }
    }
    Shape {
        tris,
        size: [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]],
        volume,
        normals_agree: if total > 0 { agree as f32 / total as f32 } else { -1.0 },
        has_uv,
        has_color,
        meshes: doc.meshes().count(),
        uvs,
    }
}

fn convert(name: &str) -> (Vec<u8>, floptle_convert::Report) {
    floptle_convert::convert(&models().join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// A cube is 2×2×2 with a volume of exactly 8. Nothing to interpret.
#[test]
fn a_modern_binary_fbx_converts_at_the_right_size_and_the_right_way_out() {
    let (bytes, report) = convert("blender_272_cube_7400_binary.fbx");
    let s = inspect(&bytes);

    assert_eq!(s.tris, 12, "a cube is twelve triangles");
    for k in 0..3 {
        assert!((s.size[k] - 2.0).abs() < 1e-4, "2 metres on every axis, got {:?}", s.size);
    }
    // **Winding.** A mirrored conversion has exactly this size and triangle
    // count and a volume of -8: inside out, invisible from outside, and
    // impossible to spot in a vertex dump.
    assert!((s.volume - 8.0).abs() < 1e-3, "volume {} — should be +8", s.volume);
    assert_eq!(s.normals_agree, 1.0, "every normal points the way its triangle winds");
    assert!(report.dropped.is_empty(), "nothing to drop from a bare cube: {:?}", report.dropped);
}

/// **The test that matters most.** One model, exported three ways, must convert
/// to the same thing.
///
/// It is the only check here that cannot be satisfied by a converter that is
/// self-consistently wrong: units, axes and winding all have to be right in
/// each of three independent paths for the three to agree. It is also what
/// caught the real bug in this crate — the legacy ASCII exporter declares
/// centimetres and writes metres, so that file converted a hundred times too
/// small while the other two were fine.
#[test]
fn the_same_ball_converts_identically_from_fbx_binary_fbx_ascii_and_obj() {
    let a = inspect(&convert("blender_279_ball_7400_binary.fbx").0);
    let b = inspect(&convert("blender_279_ball_6100_ascii.fbx").0);
    let c = inspect(&convert("blender_279_ball_0_obj.obj").0);

    for (name, s) in [("fbx-binary", &a), ("fbx-ascii", &b), ("obj", &c)] {
        assert_eq!(s.tris, 80, "{name}: triangle count");
        assert!(s.volume > 0.0, "{name}: wound inside out (volume {})", s.volume);
        assert_eq!(s.normals_agree, 1.0, "{name}: normals face the wrong way");
        // A Blender unit sphere is 2 units across, and a Blender unit is a
        // metre. Two centimetres or two hundred means a unit was misread.
        assert!(
            (s.size[1] - 2.0).abs() < 1e-3,
            "{name}: should be 2 m tall, got {} — a unit conversion is wrong",
            s.size[1]
        );
    }
    for k in 0..3 {
        assert!((a.size[k] - b.size[k]).abs() < 1e-3, "binary vs ascii differ: {:?} {:?}", a.size, b.size);
        assert!((a.size[k] - c.size[k]).abs() < 1e-3, "fbx vs obj differ: {:?} {:?}", a.size, c.size);
    }
    assert!((a.volume - b.volume).abs() < 1e-3, "binary vs ascii volume");
    assert!((a.volume - c.volume).abs() < 1e-3, "fbx vs obj volume");
}

/// A mesh split across two materials stays split.
///
/// glTF gives a primitive exactly one material; merging the two would make a
/// two-coloured ball one colour, which looks like the model was exported wrong.
#[test]
fn a_mesh_with_two_materials_converts_to_two() {
    let (bytes, report) = convert("blender_279_ball_7400_binary.fbx");
    let s = inspect(&bytes);
    assert_eq!(s.meshes, 2, "one mesh per material");
    assert_eq!(report.meshes, 2, "and the report says so");
    assert_eq!(report.materials, 2);
}

/// Per-vertex colour survives. A converter written for game props drops it
/// without noticing, and a photoscan or a painted mesh is then a grey blob.
#[test]
fn vertex_colours_survive() {
    let (bytes, _) = convert("blender_279_color_sets_7400_binary.fbx");
    let s = inspect(&bytes);
    assert!(s.has_color, "the colour set was dropped");
}

/// Texture coordinates survive, and land in range.
///
/// A model that loses its UVs converts to something that cannot be textured at
/// all — and it looks completely fine until somebody assigns a material to it.
#[test]
fn texture_coordinates_survive() {
    let (bytes, _) = convert("blender340_tangent_sign_7400_binary.fbx");
    let s = inspect(&bytes);
    assert!(s.has_uv, "the UVs were dropped");
    // The V flip this converter applies must land inside the unit square, not
    // outside it: flipping the wrong term gives `-v`, which tiles as a mirror
    // and looks like the texture, not the converter, is wrong.
    for uv in &s.uvs {
        assert!(
            uv[1] >= -0.001 && uv[1] <= 1.001,
            "a V coordinate came out at {} — the flip is wrong",
            uv[1]
        );
    }
}

/// **Packing a `.gltf` must NOT flip V.**
///
/// The FBX path flips it because FBX and glTF disagree; the repack path must
/// not, because the source is already glTF. It is the same line of code in two
/// places with opposite correct answers, which is exactly the kind of thing
/// that gets "fixed" into consistency and silently inverts every packed model's
/// textures.
#[test]
fn packing_a_gltf_leaves_its_uvs_alone() {
    let dir = std::env::temp_dir().join(format!("floptle-conv-uv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let (glb, _) = convert("blender340_tangent_sign_7400_binary.fbx");
    let before = inspect(&glb);
    assert!(before.has_uv, "the fixture must have UVs for this to mean anything");

    let (json, bin) = split_glb(&glb);
    let mut doc: serde_json::Value = serde_json::from_slice(&json).unwrap();
    doc["buffers"][0]["uri"] = serde_json::Value::String("u.bin".into());
    std::fs::write(dir.join("u.bin"), &bin).unwrap();
    std::fs::write(dir.join("u.gltf"), serde_json::to_vec(&doc).unwrap()).unwrap();

    let (packed, _) = floptle_convert::convert(&dir.join("u.gltf")).expect("packs");
    let after = inspect(&packed);

    assert_eq!(after.uvs.len(), before.uvs.len(), "UV count changed");
    for (a, b) in after.uvs.iter().zip(before.uvs.iter()) {
        assert!(
            (a[0] - b[0]).abs() < 1e-5 && (a[1] - b[1]).abs() < 1e-5,
            "a repack changed a UV: {b:?} -> {a:?} — V was flipped when it should not be"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file of bones and nothing else must say so.
///
/// The failure this prevents is silent: an empty `.glb` is valid, opens fine,
/// and shows nothing — which reads as a broken engine rather than as a file
/// that never had geometry in it.
#[test]
fn a_file_with_no_meshes_says_so_instead_of_writing_an_empty_model() {
    let err = floptle_convert::convert(&models().join("blender_279_bone_radius_7400_binary.fbx"))
        .expect_err("bones-only must not convert");
    assert!(
        matches!(err, floptle_convert::ConvertError::NoGeometry),
        "wrong error: {err}"
    );
    assert!(err.to_string().contains("no geometry"), "{err}");
}

/// The units the file declared are reported, so a scale surprise is explicable
/// rather than mysterious.
#[test]
fn the_report_says_what_units_it_found() {
    let (_, r) = convert("blender_279_ball_7400_binary.fbx");
    assert_eq!(r.source_unit_meters, Some(0.01), "the file is in centimetres");
    assert!(
        r.warnings.iter().any(|w| w.contains("centimetres")),
        "the scaling has to be said out loud: {:?}",
        r.warnings
    );
}

/// Animation, cameras and lights are dropped — and named, never silently.
#[test]
fn what_is_dropped_is_reported() {
    let (_, r) = convert("blender_279_ball_6100_ascii.fbx");
    let joined = r.dropped.join(", ");
    assert!(joined.contains("animation"), "animations must be reported: {joined}");
    assert!(joined.contains("camera"), "cameras must be reported: {joined}");
}

// ---------------------------------------------------------------------------
// Formats simple enough to write exactly, so the test states the geometry it
// expects instead of asserting against a file nobody can read.
// ---------------------------------------------------------------------------

/// A unit cube's 12 triangles, as (position) triples — counter-clockwise seen
/// from outside, so a correct reader gives volume +1.
fn cube_triangles() -> Vec<[[f32; 3]; 3]> {
    let v = [
        [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [1.0, 1.0, 1.0], [0.0, 1.0, 1.0],
    ];
    let quads: [[usize; 4]; 6] = [
        [4, 5, 6, 7], // +z
        [1, 0, 3, 2], // -z
        [0, 4, 7, 3], // -x
        [5, 1, 2, 6], // +x
        [3, 7, 6, 2], // +y
        [0, 1, 5, 4], // -y
    ];
    let mut out = Vec::new();
    for q in quads {
        out.push([v[q[0]], v[q[1]], v[q[2]]]);
        out.push([v[q[0]], v[q[2]], v[q[3]]]);
    }
    out
}

fn write_binary_stl(path: &Path) {
    let tris = cube_triangles();
    let mut b = vec![0u8; 80];
    b.extend_from_slice(&(tris.len() as u32).to_le_bytes());
    for t in &tris {
        // A zeroed normal, which real STL files are full of — the converter has
        // to compute its own rather than trust it.
        for _ in 0..3 {
            b.extend_from_slice(&0f32.to_le_bytes());
        }
        for v in t {
            for c in v {
                b.extend_from_slice(&c.to_le_bytes());
            }
        }
        b.extend_from_slice(&0u16.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

#[test]
fn a_binary_stl_converts_and_gets_its_own_normals() {
    let dir = std::env::temp_dir().join(format!("floptle-conv-stl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("cube.stl");
    write_binary_stl(&src);

    let (bytes, report) = floptle_convert::convert(&src).expect("stl converts");
    let s = inspect(&bytes);
    assert_eq!(s.tris, 12);
    assert!((s.volume - 1.0).abs() < 1e-4, "unit cube, volume {} — winding is wrong", s.volume);
    // **STL stores one normal per face and this one stored zeros.** Trusting
    // them would give a mesh that renders black; they have to be computed.
    assert_eq!(s.normals_agree, 1.0, "normals were not recomputed from the geometry");
    assert!(
        report.warnings.iter().any(|w| w.contains("units")),
        "STL has no units and the report must say so: {:?}",
        report.warnings
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn write_ascii_ply(path: &Path, with_faces: bool) {
    let tris = cube_triangles();
    // De-indexed, with a colour per vertex, which is what a scanner writes.
    let mut verts = String::new();
    let mut faces = String::new();
    let mut n = 0;
    for t in &tris {
        for v in t {
            verts.push_str(&format!("{} {} {} 255 128 0\n", v[0], v[1], v[2]));
        }
        faces.push_str(&format!("3 {} {} {}\n", n, n + 1, n + 2));
        n += 3;
    }
    let mut s = format!(
        "ply\nformat ascii 1.0\nelement vertex {}\nproperty float x\nproperty float y\n\
         property float z\nproperty uchar red\nproperty uchar green\nproperty uchar blue\n",
        n
    );
    if with_faces {
        s.push_str(&format!(
            "element face {}\nproperty list uchar int vertex_indices\n",
            tris.len()
        ));
    }
    s.push_str("end_header\n");
    s.push_str(&verts);
    if with_faces {
        s.push_str(&faces);
    }
    std::fs::write(path, s).unwrap();
}

#[test]
fn an_ascii_ply_converts_and_keeps_its_colour() {
    let dir = std::env::temp_dir().join(format!("floptle-conv-ply-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("scan.ply");
    write_ascii_ply(&src, true);

    let (bytes, _) = floptle_convert::convert(&src).expect("ply converts");
    let s = inspect(&bytes);
    assert_eq!(s.tris, 12);
    assert!((s.volume - 1.0).abs() < 1e-4, "volume {}", s.volume);
    // A scan's whole value is its colour.
    assert!(s.has_color, "per-vertex colour was dropped");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binary PLY, both byte orders, must read the same as the ASCII one.
///
/// The reader is this crate's own, so the binary path is ours to get wrong —
/// and a byte-order or field-width mistake does not fail, it produces geometry
/// made of nonsense coordinates. Checking all three encodings of the same cube
/// against each other is what makes that impossible to miss.
#[test]
fn binary_ply_reads_the_same_as_ascii_in_both_byte_orders() {
    let dir = std::env::temp_dir().join(format!("floptle-conv-plybin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let ascii = dir.join("a.ply");
    write_ascii_ply(&ascii, true);
    let want = inspect(&floptle_convert::convert(&ascii).expect("ascii").0);

    for (be, name) in [(false, "le.ply"), (true, "be.ply")] {
        let p = dir.join(name);
        write_binary_ply(&p, be);
        let got = inspect(&floptle_convert::convert(&p).unwrap_or_else(|e| panic!("{name}: {e}")).0);
        assert_eq!(got.tris, want.tris, "{name}: triangles");
        assert!(
            (got.volume - want.volume).abs() < 1e-4,
            "{name}: volume {} vs {} — a byte order or field width is wrong",
            got.volume,
            want.volume
        );
        for k in 0..3 {
            assert!(
                (got.size[k] - want.size[k]).abs() < 1e-5,
                "{name}: size {:?} vs {:?}",
                got.size,
                want.size
            );
        }
        assert!(got.has_color, "{name}: colour lost");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same cube as [`write_ascii_ply`], in binary. Mixed field widths on
/// purpose: `double` positions and `uchar` colours, so a reader that assumes
/// one size for everything walks off the rails immediately.
fn write_binary_ply(path: &Path, be: bool) {
    let tris = cube_triangles();
    let n = tris.len() * 3;
    // **An element we do not want, sitting between the two we do.** PLY's body
    // is elements back to back with no markers, so position IS the addressing:
    // skipping an unwanted element rather than consuming it leaves the reader
    // pointing into the middle of it and every face after is nonsense. Real
    // scanner output carries these (`camera`, `range_grid`), so the fixture
    // does too.
    let mut head = format!(
        "ply\nformat {} 1.0\nelement vertex {}\nproperty double x\nproperty double y\n\
         property double z\nproperty uchar red\nproperty uchar green\nproperty uchar blue\n\
         element camera 1\nproperty float view_px\nproperty float view_py\n\
         element face {}\nproperty list uchar int vertex_indices\nend_header\n",
        if be { "binary_big_endian" } else { "binary_little_endian" },
        n,
        tris.len()
    );
    let mut body: Vec<u8> = Vec::new();
    for t in &tris {
        for v in t {
            for c in v {
                let d = *c as f64;
                body.extend_from_slice(&if be { d.to_be_bytes() } else { d.to_le_bytes() });
            }
            body.extend_from_slice(&[255u8, 128, 0]);
        }
    }
    // the camera element's two floats
    for v in [1.5f32, -2.5f32] {
        body.extend_from_slice(&if be { v.to_be_bytes() } else { v.to_le_bytes() });
    }
    for i in 0..tris.len() as u32 {
        body.push(3);
        for k in 0..3u32 {
            let idx = i * 3 + k;
            body.extend_from_slice(&if be { idx.to_be_bytes() } else { idx.to_le_bytes() });
        }
    }
    let mut out = std::mem::take(&mut head).into_bytes();
    out.extend_from_slice(&body);
    std::fs::write(path, out).unwrap();
}

/// A PLY with no faces is a point cloud, and glTF has no place to put one here.
/// It must say what the file is and what to do, not write an empty model.
#[test]
fn a_point_cloud_ply_explains_itself() {
    let dir = std::env::temp_dir().join(format!("floptle-conv-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("cloud.ply");
    write_ascii_ply(&src, false);

    let err = floptle_convert::convert(&src).expect_err("a point cloud has no surface");
    let msg = err.to_string();
    assert!(msg.contains("point cloud"), "{msg}");
    assert!(msg.contains("mesh"), "it has to say what to do about it: {msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A loose `.gltf` + `.bin` is packed into one `.glb`, unchanged.
///
/// The round trip is the check: convert a model to `.glb`, unpack that into a
/// loose `.gltf` pair, pack it again, and the geometry has to survive both
/// ways. Anything the packer does to positions, winding or UVs shows up as a
/// difference from the original.
#[test]
fn a_loose_gltf_packs_into_one_file_without_changing_the_model() {
    let dir = std::env::temp_dir().join(format!("floptle-conv-gltf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Start from a known-good .glb produced by this crate.
    let (glb, _) = convert("blender_272_cube_7400_binary.fbx");
    let before = inspect(&glb);

    // Split it into the loose form: JSON that points at an external .bin.
    let (json, bin) = split_glb(&glb);
    let mut doc: serde_json::Value = serde_json::from_slice(&json).unwrap();
    doc["buffers"][0]["uri"] = serde_json::Value::String("cube.bin".into());
    std::fs::write(dir.join("cube.bin"), &bin).unwrap();
    std::fs::write(dir.join("cube.gltf"), serde_json::to_vec(&doc).unwrap()).unwrap();

    let (packed, _) = floptle_convert::convert(&dir.join("cube.gltf")).expect("packs");
    let after = inspect(&packed);

    assert_eq!(after.tris, before.tris, "triangles changed in the repack");
    for k in 0..3 {
        assert!(
            (after.size[k] - before.size[k]).abs() < 1e-5,
            "size changed: {:?} -> {:?}",
            before.size,
            after.size
        );
    }
    assert!(
        (after.volume - before.volume).abs() < 1e-4,
        "volume changed — the repack flipped winding: {} -> {}",
        before.volume,
        after.volume
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A mirrored node must not come out inside out.**
///
/// A negative scale on a node — which is how anyone makes a left boot from a
/// right one — reverses triangle winding. Left alone, every face of that node
/// points inward: invisible from outside, solid from within, and identical to a
/// correct model in every vertex dump. The FBX path guards this by the same
/// rule; this is the one a test can construct, because no exporter fixture
/// happens to mirror.
#[test]
fn a_mirrored_node_is_not_wound_inside_out() {
    let dir = std::env::temp_dir().join(format!("floptle-conv-mirror-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let (glb, _) = convert("blender_272_cube_7400_binary.fbx");
    let (json, bin) = split_glb(&glb);
    let mut doc: serde_json::Value = serde_json::from_slice(&json).unwrap();
    doc["buffers"][0]["uri"] = serde_json::Value::String("m.bin".into());

    // Mirror every root node on X, and drop the TRS the writer emitted so the
    // matrix is the only transform (glTF forbids both on one node).
    let roots: Vec<usize> = doc["scenes"][0]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    for r in roots {
        let n = &mut doc["nodes"][r];
        for k in ["translation", "rotation", "scale"] {
            if let Some(o) = n.as_object_mut() {
                o.remove(k);
            }
        }
        n["matrix"] = serde_json::json!([
            -1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0
        ]);
    }
    std::fs::write(dir.join("m.bin"), &bin).unwrap();
    std::fs::write(dir.join("m.gltf"), serde_json::to_vec(&doc).unwrap()).unwrap();

    let (packed, _) = floptle_convert::convert(&dir.join("m.gltf")).expect("packs");
    let s = inspect(&packed);
    assert_eq!(s.tris, 12, "the mirror should not lose triangles");
    assert!(
        s.volume > 0.0,
        "a mirrored node came out inside out (volume {}) — winding was not corrected",
        s.volume
    );
    assert_eq!(s.normals_agree, 1.0, "normals were not turned with the mirror");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pull the JSON and BIN chunks back out of a `.glb`.
fn split_glb(glb: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut off = 12; // header
    let (mut json, mut bin) = (Vec::new(), Vec::new());
    while off + 8 <= glb.len() {
        let len = u32::from_le_bytes(glb[off..off + 4].try_into().unwrap()) as usize;
        let kind = u32::from_le_bytes(glb[off + 4..off + 8].try_into().unwrap());
        let body = &glb[off + 8..off + 8 + len];
        match kind {
            0x4E4F534A => json = body.to_vec(),
            0x004E4942 => bin = body.to_vec(),
            _ => {}
        }
        off += 8 + len;
    }
    (json, bin)
}

/// The file-picking rules, which decide what the editor offers.
#[test]
fn only_the_formats_we_read_are_offered() {
    for good in ["a.fbx", "a.FBX", "a.obj", "a.stl", "a.ply", "a.gltf"] {
        assert!(floptle_convert::is_convertible(Path::new(good)), "{good}");
    }
    // Already the output format — converting one to itself is an action whose
    // best case is doing nothing.
    for bad in ["a.glb", "a.png", "a.blend", "a", "a.ron"] {
        assert!(!floptle_convert::is_convertible(Path::new(bad)), "{bad}");
    }
}

/// A `.glb` is refused with an explanation rather than a generic "unsupported".
#[test]
fn converting_a_glb_says_it_is_already_one() {
    let err = floptle_convert::convert(Path::new("x.glb")).expect_err("refused");
    assert!(err.to_string().contains("already"), "{err}");
}

/// The output lands beside the source, keeping its name.
#[test]
fn the_output_goes_beside_the_source() {
    let out = floptle_convert::output_path(Path::new("/models/props/crate.fbx"));
    assert_eq!(out, PathBuf::from("/models/props/crate.glb"));
}

/// Junk with the right extension fails as "could not be understood", not as a
/// panic and not as an empty model.
#[test]
fn a_file_that_is_not_really_a_model_fails_cleanly() {
    let dir = std::env::temp_dir().join(format!("floptle-conv-junk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["junk.fbx", "junk.stl", "junk.ply", "junk.gltf"] {
        let p = dir.join(name);
        std::fs::write(&p, b"this is not a model, it is a sentence").unwrap();
        let err = floptle_convert::convert(&p).expect_err("{name} must fail");
        assert!(
            !err.to_string().is_empty(),
            "{name}: an error with nothing in it is no better than a panic"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
