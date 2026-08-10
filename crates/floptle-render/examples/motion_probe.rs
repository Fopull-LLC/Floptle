//! Motion-blur probe: the smear runs along the way the camera went, is as long
//! as the shutter says, and a still camera leaves the frame untouched.
//!
//! Motion blur here is reconstructed rather than rendered — the pass takes a
//! pixel's depth, puts it back in the world, and asks where that point was in
//! the previous frame's picture. Every failure mode of that arrangement is
//! quiet: a transposed matrix still gives a plausible smear, a sign error smears
//! the right amount in the wrong direction, and a dropped camera-relative shift
//! smears a still camera. So each check is a CONTROL PAIR — render the same
//! scene twice, change one thing, and assert the specific thing it changes.
//!
//! **The scene is a wall with a bright half**, not a card on black, and that is
//! deliberate. This is a *gather*: a pixel collects along its OWN velocity, so a
//! moving thing softens inside its own footprint rather than throwing light
//! outside it. Against black, a smeared card comes back the same size (its edge
//! pixels reach out into black, and the black pixels have the sky's velocity, not
//! the card's) and a width measurement reads "no blur" on a pass that is working
//! perfectly. An edge between two surfaces at the SAME depth is the honest
//! subject: both sides share a velocity, and the blur is exactly the width of the
//! ramp between them.
//!
//! Run: cargo run -p floptle-render --example motion_probe -- <out-dir>

use floptle_render::{
    Globals, Gpu, MaterialParams, PostSettings, PostStack, Projection, Raster, RenderCamera,
    SsaoFrame, instance_of_mat, plane,
};
use glam::{Mat4, Quat, Vec3};

const S: u32 = 192;
const FOV: f32 = 0.9;
/// How far in front of the camera the wall sits.
const Z: f32 = 8.0;

/// Which way the bright half's edge runs.
#[derive(Clone, Copy, PartialEq)]
enum Edge {
    /// A vertical edge — measured by how wide the ramp is along a row.
    Vertical,
    /// A horizontal one — measured down a column.
    Horizontal,
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless_hdr(S, S);
    let mut rig = Rig::new(&gpu);

    let base =
        PostSettings { motion_blur: 0.5, motion_samples: 24, motion_max: 64.0, ..Default::default() };
    let off = PostSettings::default();
    let vert = Edge::Vertical;
    let horiz = Edge::Horizontal;

    // ---- 1. a still camera changes nothing ---------------------------------
    //
    // The first frame after a load, a cut, a paused game. The pass still runs —
    // the settings say motion blur is on — and it has to be an identity. Without
    // this, every static shot in every project is softened for nothing, which
    // gets reported as "the update made my game blurry" and is impossible to
    // attribute to anything.
    let still = rig.render(&gpu, vert, &base, Vec3::ZERO, Quat::IDENTITY);
    let sharp = rig.render(&gpu, vert, &off, Vec3::ZERO, Quat::IDENTITY);
    write_png(&format!("{dir}/motion_still.png"), &still);
    let diff = max_diff(&still, &sharp);
    println!("still camera: max channel difference vs blur-off = {diff}");
    assert!(
        diff <= 1,
        "a camera that did not move must leave the frame EXACTLY alone — biggest channel \
         difference was {diff}. Anything here means the reprojection is finding motion in a \
         still shot, which is usually the camera-relative shift being applied twice or not \
         at all."
    );
    let base_ramp = ramp(&sharp, vert);
    println!("sharp edge ramp: {base_ramp} px");

    // ---- 2. a sideways dolly smears sideways --------------------------------
    //
    // The vertical edge goes soft and the horizontal one does not. Measuring
    // both axes is what catches a transposed matrix, which reads as perfectly
    // good blur along the wrong one.
    let moved = Vec3::new(1.2, 0.0, 0.0);
    let dolly = rig.render(&gpu, vert, &base, moved, Quat::IDENTITY);
    let dolly_h = rig.render(&gpu, horiz, &base, moved, Quat::IDENTITY);
    write_png(&format!("{dir}/motion_dolly.png"), &dolly);
    let (across, along) = (ramp(&dolly, vert), ramp(&dolly_h, horiz));
    println!("sideways dolly: vertical edge {across} px, horizontal edge {along} px");
    assert!(
        across > base_ramp + 4,
        "a sideways move must soften the VERTICAL edge — it went from {base_ramp} px to \
         {across} px. No growth means the velocity is coming back as zero: check that the \
         previous view-projection is shifted into this frame's camera-relative origin."
    );
    assert!(
        along <= base_ramp + 2,
        "…and must leave the HORIZONTAL edge alone — {base_ramp} px to {along} px. Softening \
         both is an isotropic blur wearing motion blur's name."
    );

    // ---- 3. a vertical move smears vertically -------------------------------
    let lifted = Vec3::new(0.0, 1.2, 0.0);
    let lift_h = rig.render(&gpu, horiz, &base, lifted, Quat::IDENTITY);
    let lift_v = rig.render(&gpu, vert, &base, lifted, Quat::IDENTITY);
    write_png(&format!("{dir}/motion_lift.png"), &lift_h);
    let (l_along, l_across) = (ramp(&lift_h, horiz), ramp(&lift_v, vert));
    println!("vertical lift: horizontal edge {l_along} px, vertical edge {l_across} px");
    assert!(
        l_along > base_ramp + 4,
        "a vertical move must soften the HORIZONTAL edge — {base_ramp} px to {l_along} px."
    );
    assert!(
        l_across <= base_ramp + 2,
        "…and not the vertical one — {base_ramp} px to {l_across} px. Both axes softening from \
         a purely vertical move means the two screen axes are crossed."
    );

    // ---- 4. the shutter is the length ---------------------------------------
    //
    // The knob is a fraction of the frame's motion, so halving it has to roughly
    // halve the smear. This is what separates "motion blur is wired up" from
    // "the shutter slider does something".
    let half =
        rig.render(&gpu, vert, &PostSettings { motion_blur: 0.25, ..base }, moved, Quat::IDENTITY);
    let half_ramp = ramp(&half, vert);
    let (long, short) = ((across - base_ramp) as f32, (half_ramp - base_ramp) as f32);
    println!("shutter: 0.5 widens the ramp by {long} px, 0.25 by {short} px");
    assert!(
        short > 1.0 && short < long * 0.8,
        "halving the shutter must roughly halve the smear — 0.5 added {long} px of ramp and \
         0.25 added {short}. Equal amounts mean the shutter never reached the shader."
    );

    // ---- 5. a pan blurs -----------------------------------------------------
    //
    // Rotation is the case a naive implementation gets wrong: there is no
    // translation to subtract, so a pass that only handled the camera's POSITION
    // reports no motion at all. It is also the most common camera move there is,
    // and — unlike a dolly — it moves every pixel by the same amount whatever its
    // depth, so it is the case that looks best.
    let pan = rig.render(&gpu, vert, &base, Vec3::ZERO, Quat::from_rotation_y(0.1));
    write_png(&format!("{dir}/motion_pan.png"), &pan);
    let pan_ramp = ramp(&pan, vert);
    println!("pan: {pan_ramp} px");
    assert!(
        pan_ramp > base_ramp + 4,
        "a pan must smear — the ramp went {base_ramp} px to {pan_ramp} px. Nothing here means \
         only camera translation is being reprojected, and a look-around camera is exactly \
         the case motion blur exists for."
    );

    // ---- 6. the ceiling is a ceiling ----------------------------------------
    //
    // A violent whip must not cost an unbounded gather across the whole frame.
    // The cap is in PIXELS, so the same setting is the same look at any window
    // size — and it has to actually bind.
    let whip = Vec3::new(4.0, 0.0, 0.0);
    let capped =
        rig.render(&gpu, vert, &PostSettings { motion_max: 6.0, ..base }, whip, Quat::IDENTITY);
    let loose =
        rig.render(&gpu, vert, &PostSettings { motion_max: 64.0, ..base }, whip, Quat::IDENTITY);
    write_png(&format!("{dir}/motion_capped.png"), &capped);
    let (rc, rl) = (ramp(&capped, vert), ramp(&loose, vert));
    println!("cap: 6 px cap → {rc} px ramp, 64 px cap → {rl} px ramp");
    assert!(
        rc + 10 < rl,
        "the streak ceiling must bind on a violent move — {rc} px against {rl} px with the cap \
         raised. Equal ramps mean the cap is decorative."
    );

    println!("motion probe OK");
}

struct Rig {
    raster: Raster,
    post: PostStack,
    mesh: floptle_render::MeshId,
    out: wgpu::Texture,
    out_view: wgpu::TextureView,
    proj: Mat4,
}

impl Rig {
    fn new(gpu: &Gpu) -> Self {
        let mut raster = Raster::new(gpu);
        let mesh = raster.register(gpu, &plane(1.0), None);
        let post = PostStack::new(gpu, S, S);
        let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("motion-out"),
            size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.surface_format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
        let cam = camera(Quat::IDENTITY);
        Self { raster, post, mesh, out, out_view, proj: cam.proj_matrix(1.0) }
    }

    /// Draw the wall from where the camera is NOW, and hand the pass a previous
    /// pose displaced by `moved` and rotated by `turned`.
    ///
    /// The scene is rendered once, which is what the real thing does — motion is
    /// entirely a matter of what the previous view-projection says. Rendering two
    /// frames and comparing them would test the test.
    fn render(
        &mut self,
        gpu: &Gpu,
        edge: Edge,
        s: &PostSettings,
        moved: Vec3,
        turned: Quat,
    ) -> Vec<[u8; 3]> {
        let now = camera(Quat::IDENTITY);
        let view_proj = now.view_proj(1.0);
        let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };

        // A dim wall filling the frame, and a bright panel over half of it at
        // very nearly the same depth — so both sides of the edge share a
        // velocity and the ramp between them measures the blur and nothing else.
        let quad = |z: f32, offset: Vec3, glow: f32| {
            let mut m = MaterialParams::flat([0.0, 0.0, 0.0]);
            m.unlit = true;
            m.emissive = [1.0, 1.0, 1.0];
            m.emissive_strength = glow;
            let xf = Mat4::from_translation(Vec3::new(0.0, 0.0, -z) + offset)
                * Mat4::from_scale(Vec3::splat(12.0));
            (self.mesh, None, instance_of_mat(xf, &m))
        };
        // The panel's own half-width (`plane(1.0)` spans ±1, scaled by 12), so
        // its edge lands exactly on the frame's centre line.
        let shift = 12.0;
        let panel = match edge {
            Edge::Vertical => Vec3::new(-shift, 0.0, 0.0),
            Edge::Horizontal => Vec3::new(0.0, -shift, 0.0),
        };
        // Both plateaus below white on purpose. The chain's tonemap clips, so a
        // panel brighter than 1.0 would stay at 255 through most of the ramp and
        // the measurement would read a wide smear as a narrow one.
        let draws = [quad(Z, Vec3::ZERO, 0.05), quad(Z - 0.1, panel, 0.8)];
        self.raster.draw_scene(
            gpu,
            self.post.input_view(),
            gpu.depth_view(),
            globals,
            &draws,
            Some([0.0, 0.0, 0.0, 1.0]),
            None,
        );

        // The previous camera, expressed the way the editor expresses it: the
        // world is camera-relative, so the old view-projection is shifted by how
        // far the camera itself travelled. `moved` is where the camera is NOW
        // relative to where it was, so the shift is `+moved`.
        let prev_vp = camera(turned.inverse()).view_proj(1.0) * Mat4::from_translation(moved);

        let mut s = *s;
        s.motion_inv_view_proj = view_proj.inverse().to_cols_array_2d();
        s.motion_prev_view_proj = prev_vp.to_cols_array_2d();

        let ssao = SsaoFrame {
            depth: gpu.depth_view(),
            proj: self.proj.to_cols_array_2d(),
            inv_proj: self.proj.inverse().to_cols_array_2d(),
        };
        self.post.run(gpu, &s, Some(&ssao), &self.out_view);
        read_rgb(gpu, &self.out)
    }
}

fn camera(rot: Quat) -> RenderCamera {
    RenderCamera::new(
        Vec3::ZERO.as_dvec3(),
        rot,
        Projection::Perspective { fov_y: FOV, near: 0.05, far: 200.0 },
    )
}

fn luma(c: [u8; 3]) -> f32 {
    0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32
}

/// How many pixels the bright→dim transition takes, across the middle of the
/// frame. A sharp edge is one or two; a smear is as wide as the streak.
///
/// Measured between a fifth and four fifths of the way down the step, so the
/// number is about the ramp itself and not about where the two plateaus happen
/// to sit — which the tonemap moves and the blur does not.
fn ramp(img: &[[u8; 3]], edge: Edge) -> usize {
    let n = S as usize;
    let at = |i: usize| -> f32 {
        let (x, y) = match edge {
            Edge::Vertical => (i, n / 2),
            Edge::Horizontal => (n / 2, i),
        };
        luma(img[y * n + x])
    };
    // The plateaus, sampled well away from the edge. Which side is the bright
    // one is a fact about the geometry, not about the blur, so it is measured
    // rather than assumed.
    let (a0, b0) = (at(n / 8), at(n - n / 8));
    let (hi, lo) = (a0.max(b0), a0.min(b0));
    assert!(hi > lo + 40.0, "the edge must be a real step to measure ({hi} against {lo})");
    let (a, b) = (lo + (hi - lo) * 0.2, lo + (hi - lo) * 0.8);
    (0..n).filter(|&i| at(i) > a && at(i) < b).count()
}

/// The biggest single-channel difference between two frames — the strict form of
/// "these pictures are the same".
fn max_diff(a: &[[u8; 3]], b: &[[u8; 3]]) -> u8 {
    a.iter()
        .zip(b)
        .flat_map(|(p, q)| (0..3).map(move |k| p[k].abs_diff(q[k])))
        .max()
        .unwrap_or(0)
}

fn read_rgb(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 3]> {
    let bpp = 4u32;
    let padded =
        (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("motion-readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("motion-readback") });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((S * S) as usize);
    for y in 0..S as usize {
        let row = &view[y * padded as usize..][..(S * bpp) as usize];
        for x in 0..S as usize {
            let p = &row[x * 4..][..4];
            out.push(if bgra { [p[2], p[1], p[0]] } else { [p[0], p[1], p[2]] });
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn write_png(path: &str, img: &[[u8; 3]]) {
    let mut flat = Vec::with_capacity(img.len() * 3);
    for p in img {
        flat.extend_from_slice(p);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("png header").write_image_data(&flat).expect("png data");
}
