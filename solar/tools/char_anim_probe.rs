//! TEMP: author + eyeball procedural idle/run/jump for character_retro. Imports the
//! rig, poses the skeleton procedurally, CPU-skins, renders one PNG per phase.
use floptle_render::{instance_of_mat, Globals, Gpu, MaterialParams, MeshData, Projection,
    Raster, RenderCamera, Vertex};
use floptle_anim::{Skeleton, TransformTRS};
use glam::{Mat3, Mat4, Quat, Vec3};

const W: u32 = 360;
const H: u32 = 560;

fn nidx(sk: &Skeleton, n: &str) -> Option<usize> { sk.nodes.iter().position(|s| s.name == n) }
fn rot(pose: &mut [TransformTRS], sk: &Skeleton, name: &str, pre: Quat) {
    if let Some(i) = nidx(sk, name) { pose[i].r = pre * pose[i].r; }
}
fn tr(pose: &mut [TransformTRS], sk: &Skeleton, name: &str, d: Vec3) {
    if let Some(i) = nidx(sk, name) { pose[i].t += d; }
}

fn pose(sk: &Skeleton, kind: &str, ph: f32) -> Vec<TransformTRS> {
    let mut p = sk.rest_pose();
    let s = (ph * std::f32::consts::TAU).sin();
    let c = (ph * std::f32::consts::TAU).cos();
    let rx = Quat::from_rotation_x; let rz = Quat::from_rotation_z;
    match kind {
        "run" => {
            rot(&mut p, sk, "LeftUpLeg", rx(s * 0.9));
            rot(&mut p, sk, "RightUpLeg", rx(-s * 0.9));
            rot(&mut p, sk, "LeftLeg", rx((0.6 - 0.6*s).max(0.0)));
            rot(&mut p, sk, "RightLeg", rx((0.6 + 0.6*s).max(0.0)));
            rot(&mut p, sk, "LeftArm", rx(-s * 0.7));
            rot(&mut p, sk, "RightArm", rx(s * 0.7));
            tr(&mut p, sk, "Hips", Vec3::new(0.0, c.abs()*0.12, 0.0));
        }
        "jump" => {
            rot(&mut p, sk, "LeftUpLeg", rx(0.7)); rot(&mut p, sk, "RightUpLeg", rx(0.7));
            rot(&mut p, sk, "LeftLeg", rx(0.9)); rot(&mut p, sk, "RightLeg", rx(0.9));
            rot(&mut p, sk, "LeftArm", rz(-1.2)); rot(&mut p, sk, "RightArm", rz(1.2));
        }
        _ => { rot(&mut p, sk, "Spine", rx(s * 0.05)); tr(&mut p, sk, "Hips", Vec3::new(0.0, s*0.02, 0.0)); }
    }
    p
}

fn skin(base: &[Vertex], j: &[[u16;4]], w: &[[f32;4]], jn: &[usize], ibm: &[Mat4], nw: &[Mat4]) -> Vec<Vertex> {
    let pal: Vec<Mat4> = jn.iter().zip(ibm).map(|(&n,ib)| nw.get(n).copied().unwrap_or(Mat4::IDENTITY) * *ib).collect();
    base.iter().enumerate().map(|(vi, v)| {
        let (jj, ww) = (j[vi], w[vi]); let ws = ww[0]+ww[1]+ww[2]+ww[3];
        let m = if ws > 1e-4 { let mut a = Mat4::ZERO;
            for k in 0..4 { if ww[k] > 0.0 { if let Some(pm)=pal.get(jj[k] as usize){ a += *pm*(ww[k]/ws); } } } a
        } else { Mat4::IDENTITY };
        Vertex { pos: m.transform_point3(Vec3::from(v.pos)).to_array(),
            normal: (Mat3::from_mat4(m)*Vec3::from(v.normal)).normalize_or_zero().to_array(), uv: v.uv }
    }).collect()
}

fn render(gpu: &Gpu, raster: &mut Raster, md: &MeshData, name: &str) {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor { label: None,
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
    let view = tex.create_view(&Default::default());
    let id = raster.register_dynamic(gpu, md.vertices.len() as u32, md.indices.len() as u32, false);
    raster.replace_dynamic(gpu, id, md);
    let cam_pos = Vec3::new(3.4, 0.0, 11.5);
    let target = Vec3::new(0.0, 0.0, 0.0);
    let fwd = (target - cam_pos).normalize();
    let cam = RenderCamera::new(cam_pos.as_dvec3(), Quat::from_rotation_arc(Vec3::NEG_Z, fwd),
        Projection::Perspective { fov_y: 26f32.to_radians(), near: 0.05, far: 100.0 });
    let vp = cam.view_proj(W as f32 / H as f32);
    let globals = Globals { view_proj: vp.to_cols_array_2d(),
        light_dir: Vec3::new(0.3,0.5,0.8).normalize().extend(0.0).into(),
        light_color: [1.0,0.97,0.9,0.0], ambient: [0.34,0.36,0.42,0.0], ..Default::default() };
    let mat = MaterialParams::flat([0.72,0.56,0.36]);
    let model = Mat4::from_translation(-cam_pos);
    raster.draw_scene(gpu, &view, gpu.depth_view(), globals,
        &[(id, None, instance_of_mat(model, &mat))], Some([0.06,0.07,0.1,1.0]), None);
    let bpp=4u32; let padded=(W*bpp).div_ceil(256)*256;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (padded*H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture:&tex, mip_level:0, origin:wgpu::Origin3d::ZERO, aspect:wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer:&buf, layout:wgpu::TexelCopyBufferLayout { offset:0, bytes_per_row:Some(padded), rows_per_image:Some(H) } },
        wgpu::Extent3d { width:W, height:H, depth_or_array_layers:1 });
    gpu.queue.submit([enc.finish()]);
    let sl=buf.slice(..); sl.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data=sl.get_mapped_range();
    let bgra=matches!(gpu.config.format, wgpu::TextureFormat::Bgra8Unorm|wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut out=Vec::with_capacity((W*H*4) as usize);
    for y in 0..H { let r=&data[(y*padded) as usize..]; for x in 0..W { let i=(x*bpp) as usize;
        if bgra { out.extend_from_slice(&[r[i+2],r[i+1],r[i],255]); } else { out.extend_from_slice(&[r[i],r[i+1],r[i+2],255]); } } }
    image::save_buffer(name, &out, W, H, image::ColorType::Rgba8).unwrap();
    println!("wrote {name}");
}

// Bake the procedural pose() samples into an AnimClipDoc RON (channels keyed by
// node name; a rotation track wherever the pose differs from rest, plus Hips
// translation). Looping clips repeat key0 at the end for a seamless cycle.
fn bake(sk: &Skeleton, kind: &str, dur: f32, keys: usize, looped: bool, model: &str) -> String {
    let rest = sk.rest_pose();
    let q_eps = 1e-4;
    let mut chans = String::new();
    for (idx, node) in sk.nodes.iter().enumerate() {
        let mut rt = String::new();
        let mut rv = String::new();
        let mut tt = String::new();
        let mut tv = String::new();
        let (mut anim_r, mut anim_t) = (false, false);
        for i in 0..keys {
            let f = i as f32 / (keys - 1) as f32;
            let ph = if looped { f } else { f }; // f in [0,1]
            let t = f * dur;
            let p = pose(sk, kind, ph);
            let r = p[idx].r;
            let tr = p[idx].t;
            if (r.x - rest[idx].r.x).abs() > q_eps || (r.y - rest[idx].r.y).abs() > q_eps
                || (r.z - rest[idx].r.z).abs() > q_eps || (r.w - rest[idx].r.w).abs() > q_eps {
                anim_r = true;
            }
            if (tr - rest[idx].t).length() > 1e-4 { anim_t = true; }
            rt.push_str(&format!("{t:.4}, "));
            rv.push_str(&format!("({:.5}, {:.5}, {:.5}, {:.5}), ", r.x, r.y, r.z, r.w));
            tt.push_str(&format!("{t:.4}, "));
            tv.push_str(&format!("({:.5}, {:.5}, {:.5}), ", tr.x, tr.y, tr.z));
        }
        if !anim_r && !anim_t { continue; }
        chans.push_str(&format!("        (\n            node: \"{}\",\n", node.name));
        if anim_r {
            chans.push_str(&format!(
                "            rotation: Some((\n                times: [{rt}],\n                values: [{rv}],\n                step: false,\n            )),\n"));
        }
        if anim_t {
            chans.push_str(&format!(
                "            translation: Some((\n                times: [{tt}],\n                values: [{tv}],\n                step: false,\n            )),\n"));
        }
        chans.push_str("        ),\n");
    }
    format!("(\n    name: \"{kind}\",\n    duration: {dur:.4},\n    source_model: \"{model}\",\n    channels: [\n{chans}    ],\n)\n")
}

fn main() {
    let rig = floptle_assets::import_rigged(std::path::Path::new(
        "solar/models/characters/character_retro.glb")).unwrap().unwrap();
    let part = &rig.parts[0];
    let sks = part.skin.as_ref().unwrap();
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "bake" {
        let model = "models/characters/character_retro.glb";
        std::fs::create_dir_all("solar/animations/character_retro").unwrap();
        for (kind, dur, keys, looped) in
            [("idle", 2.4f32, 9usize, true), ("run", 0.62, 9, true), ("jump", 0.45, 5, false)] {
            let ron = bake(&rig.skeleton, kind, dur, keys, looped, model);
            let path = format!("solar/animations/character_retro/{kind}.anim.ron");
            std::fs::write(&path, ron).unwrap();
            println!("baked {path}");
        }
        return;
    }
    let gpu = Gpu::headless(W, H);
    let mut raster = Raster::new(&gpu);
    let ph = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0.25f32);
    for kind in ["idle","run","jump"] {
        let p = pose(&rig.skeleton, kind, ph);
        let mut nw = Vec::new(); rig.skeleton.world_matrices(&p, &mut nw);
        let verts = skin(&part.mesh.vertices, &sks.joints, &sks.weights, &sks.joint_nodes, &sks.inverse_bind, &nw);
        let md = MeshData { vertices: verts, indices: part.mesh.indices.clone(), colors: None };
        render(&gpu, &mut raster, &md, &format!("char_{kind}.png"));
    }
}
