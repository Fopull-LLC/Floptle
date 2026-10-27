//! Compute shaders run, in the order they were asked for, against the
//! buffers and textures a game made, and hand their results back.
//!
//! On every backend the engine ships, CI's OpenGL included: a device without
//! compute is said so and skipped, never passed in silence.

use floptle_render::Gpu;
use floptle_render::compute::{Bound, Compute, Elem, Job};

/// Poll until every asked-for readback has answered, a bounded wait.
fn answers(c: &mut Compute, gpu: &Gpu, want: usize) -> Vec<(u64, Vec<u8>)> {
    let mut got = Vec::new();
    for _ in 0..2000 {
        for (t, r) in c.poll(gpu) {
            got.push((t, r.expect("the readback came back")));
        }
        if got.len() >= want {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    got.sort_by_key(|(t, _)| *t);
    got
}

fn u32s(bytes: &[u8]) -> Vec<u32> {
    bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

fn gpu() -> Option<Gpu> {
    let gpu = Gpu::headless(8, 8);
    match Compute::supported(&gpu) {
        Ok(()) => Some(gpu),
        Err(why) => {
            eprintln!("skipped — {why}");
            None
        }
    }
}

fn job(source: &str, entry: &str, groups: [u32; 3], bind: Vec<(String, Bound)>, params: Vec<(String, f32)>) -> Job {
    Job { name: "test.wgsl".into(), source: source.into(), entry: entry.into(), groups, bind, params }
}

/// A buffer filled by one shader and changed by the next comes back with
/// both, in that order: the second job read what the first wrote.
#[test]
fn jobs_run_in_the_order_they_were_asked_for() {
    let Some(gpu) = gpu() else { return };
    let mut c = Compute::new();
    let out = c.create_buffer(&gpu, Elem::U32, 1024).unwrap();
    let fill = "@compute @workgroup_size(64)\nfn fill(@builtin(global_invocation_id) id: vec3<u32>) {\n  cells[id.x] = id.x * 2u + u32(params.k);\n}\n";
    let bump = "@compute @workgroup_size(64)\nfn bump(@builtin(global_invocation_id) id: vec3<u32>) {\n  cells[id.x] = cells[id.x] + 1u;\n}\n";
    c.queue(job(fill, "fill", [16, 1, 1], vec![("cells".into(), Bound::Buffer(out))], vec![("k".into(), 7.0)]));
    c.queue(job(bump, "bump", [16, 1, 1], vec![("cells".into(), Bound::Buffer(out))], vec![]));
    let mut marks = Vec::new();
    let failed = c.run_queued(&gpu, 0.0, &mut |m| marks.push(m.to_string()));
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(marks, vec!["compute fill", "compute bump"], "each job is named for the GPU timer, in order");
    c.read_buffer(&gpu, out, 1).unwrap();
    let got = answers(&mut c, &gpu, 1);
    assert_eq!(got.len(), 1, "the readback never answered");
    let v = u32s(&got[0].1);
    assert_eq!(v.len(), 1024);
    for (i, &x) in v.iter().enumerate() {
        assert_eq!(x, i as u32 * 2 + 7 + 1, "element {i}");
    }
}

/// A shader writes a texture a frame can draw, and its pixels read back.
#[test]
fn a_shader_paints_a_texture() {
    let Some(gpu) = gpu() else { return };
    let mut c = Compute::new();
    let tex = c.create_texture(&gpu, 64, 32).unwrap();
    let src = "@compute @workgroup_size(8, 8)\nfn paint(@builtin(global_invocation_id) id: vec3<u32>) {\n  let uv = vec2<f32>(id.xy) / vec2<f32>(64.0, 32.0);\n  textureStore(skin, vec2<i32>(id.xy), vec4<f32>(uv.x, uv.y, 0.0, 1.0));\n}\n";
    c.queue(job(src, "paint", [8, 4, 1], vec![("skin".into(), Bound::WriteTexture(tex))], vec![]));
    assert!(c.run_queued(&gpu, 0.0, &mut |_| {}).is_empty());
    c.read_texture(&gpu, tex, 2).unwrap();
    let got = answers(&mut c, &gpu, 1);
    let px = &got[0].1;
    assert_eq!(px.len(), 64 * 32 * 4, "rows come back unpadded");
    let at = |x: usize, y: usize| &px[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
    assert_eq!(at(0, 0), &[0, 0, 0, 255]);
    let far = at(48, 16);
    assert!((far[0] as i32 - 191).abs() <= 1 && (far[1] as i32 - 128).abs() <= 1, "{far:?}");
    assert_eq!(far[3], 255);
}

/// A shader with a mistake is refused with the line in the shader's own
/// file, not in the declarations the engine wrote in front of it.
#[test]
fn a_shader_error_names_its_own_line() {
    let Some(gpu) = gpu() else { return };
    let mut c = Compute::new();
    let out = c.create_buffer(&gpu, Elem::F32, 64).unwrap();
    let src = "@compute @workgroup_size(64)\nfn go(@builtin(global_invocation_id) id: vec3<u32>) {\n  cells[id.x] = nonsense;\n}\n";
    c.queue(job(src, "go", [1, 1, 1], vec![("cells".into(), Bound::Buffer(out))], vec![]));
    let failed = c.run_queued(&gpu, 0.0, &mut |_| {});
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].line, Some(3), "{}", failed[0]);
    assert!(failed[0].to_string().starts_with("test.wgsl:3:"), "{}", failed[0]);
}

/// A struct is laid out as WGSL lays out storage, so the bytes a script
/// writes are what the shader reads: a `vec3` starts on 16 bytes.
#[test]
fn a_struct_buffer_is_laid_out_the_way_the_shader_reads_it() {
    let parts = Elem::Struct(vec![("pos".into(), Elem::Vec3), ("life".into(), Elem::F32), ("vel".into(), Elem::Vec3)]);
    let offsets: Vec<u32> = parts.field_offsets().iter().map(|(_, _, o)| *o).collect();
    assert_eq!(offsets, vec![0, 12, 16]);
    assert_eq!(parts.stride(), 32);
    assert_eq!(Elem::Vec3.stride(), 16, "an array of vec3 strides 16");

    let Some(gpu) = gpu() else { return };
    let mut c = Compute::new();
    let buf = c.create_buffer(&gpu, parts, 4).unwrap();
    let src = "@compute @workgroup_size(4)\nfn age(@builtin(global_invocation_id) id: vec3<u32>) {\n  p[id.x].life = f32(id.x) + 0.5;\n  p[id.x].vel = vec3<f32>(1.0, 2.0, 3.0);\n}\n";
    c.queue(job(src, "age", [1, 1, 1], vec![("p".into(), Bound::Buffer(buf))], vec![]));
    assert!(c.run_queued(&gpu, 0.0, &mut |_| {}).is_empty());
    c.read_buffer(&gpu, buf, 3).unwrap();
    let got = answers(&mut c, &gpu, 1);
    let f: Vec<f32> = got[0].1.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    assert_eq!(f.len(), 4 * 8);
    assert_eq!(f[8 + 3], 1.5, "element 1's life is at offset 12");
    assert_eq!(&f[8 + 4..8 + 7], &[1.0, 2.0, 3.0], "element 1's vel is at offset 16");
}

/// One file holds several entry points, each run with only the bindings it
/// uses, and one may share a WGSL built-in's name (`step`). Every entry point
/// used to be compiled with every call: `seed`, bound with `out` alone, failed
/// on `step`'s use of `before`, and `step` collided with the built-in the
/// library calls.
#[test]
fn a_file_of_several_entry_points_runs_each_with_its_own_bindings() {
    let Some(gpu) = gpu() else { return };
    let mut c = Compute::new();
    let a = c.create_buffer(&gpu, Elem::U32, 64).unwrap();
    let b = c.create_buffer(&gpu, Elem::U32, 64).unwrap();
    let src = "@compute @workgroup_size(64)\nfn seed(@builtin(global_invocation_id) id: vec3<u32>) {\n  out[id.x] = id.x;\n}\n\n@compute @workgroup_size(64)\nfn step(@builtin(global_invocation_id) id: vec3<u32>) {\n  out[id.x] = before[id.x] + 100u;\n}\n";
    c.queue(job(src, "seed", [1, 1, 1], vec![("out".into(), Bound::Buffer(a))], vec![]));
    c.queue(job(src, "step", [1, 1, 1], vec![("before".into(), Bound::Buffer(a)), ("out".into(), Bound::Buffer(b))], vec![]));
    let failed = c.run_queued(&gpu, 0.0, &mut |_| {});
    assert!(failed.is_empty(), "{failed:?}");
    c.read_buffer(&gpu, b, 4).unwrap();
    let got = answers(&mut c, &gpu, 1);
    let v = u32s(&got[0].1);
    assert_eq!(&v[..3], &[100, 101, 102]);
    assert_eq!(v[63], 163);
}

/// **An atomic buffer counts from every invocation at once.** 4096
/// invocations each add one to the same counter, and it comes back 4096:
/// a counter, a histogram, a "how many particles are alive". Before there was
/// an atomic type a buffer was a plain `u32` array, `atomicAdd` on it did not
/// compile, and the guide's "atomics are all there" was true of workgroup
/// memory alone.
#[test]
fn an_atomic_buffer_counts_from_every_invocation() {
    let Some(gpu) = gpu() else { return };
    let mut c = Compute::new();
    let count = c.create_buffer(&gpu, Elem::AtomicU32, 1).unwrap();
    let src = "@compute @workgroup_size(64)\nfn tally(@builtin(global_invocation_id) id: vec3<u32>) {\n  atomicAdd(&hits[0], 1u);\n}\n";
    c.queue(job(src, "tally", [64, 1, 1], vec![("hits".into(), Bound::Buffer(count))], vec![]));
    let failed = c.run_queued(&gpu, 0.0, &mut |_| {});
    assert!(failed.is_empty(), "{failed:?}");
    c.read_buffer(&gpu, count, 1).unwrap();
    let got = answers(&mut c, &gpu, 1);
    assert_eq!(got.len(), 1, "the readback never answered");
    assert_eq!(u32s(&got[0].1), vec![4096]);
}
