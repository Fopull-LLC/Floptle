//! Area lights: does a light with a SHAPE light differently from a point at the
//! same place, and does the half that claims to be exact actually agree with a
//! numerically integrated one.
//!
//! The claim being checked is specific. The DIRECTION an area emitter lights
//! from is computed analytically — a polygon's vector irradiance is linear in
//! the surface normal, so one loop over the edges gives it exactly — while the
//! terminator softening and the specular representative point are fits. So the
//! direction is checked against quadrature over the emitter's actual surface,
//! and everything else is checked as a control pair: the same frame twice with
//! only the emitter's shape changed.
//!
//! An area light is easy to fake. Multiply a point light by a constant and every
//! screenshot looks plausible — it just never lights anything from anywhere new.
//!
//! Run: cargo run -p floptle-render --example area_light_probe -- <out-dir>

use floptle_render::{
    Globals, Gpu, MaterialParams, Projection, Raster, RenderCamera, SurfaceExtras, TexId, cube,
    instance_of_mat, plane,
};
use glam::{Mat4, Quat, Vec3};

const S: u32 = 256;
/// Emitter kinds, as the shader's `point_shape.x` reads them.
const POINT: f32 = 0.0;
const SPHERE: f32 = 1.0;
const RECT: f32 = 2.0;
#[allow(dead_code)]
const DISK: f32 = 3.0;
const TUBE: f32 = 4.0;
/// Every light in this file has the same reach, so a brightness difference is
/// never the falloff curve in disguise.
const RANGE: f32 = 40.0;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);

    a_point_emitter_is_the_point_light_it_always_was(&gpu);
    a_rect_lights_from_where_it_is(&gpu);
    the_direction_matches_quadrature_over_the_emitter(&gpu);
    a_one_sided_light_has_a_back(&gpu, &dir);
    size_softens_the_terminator(&gpu);
    a_bar_streaks_its_highlight(&gpu, &dir);

    println!("area light probe OK");
}

// ---------------------------------------------------------------------------
// 1. `Point` is not a new code path with the old name on it.
//
// Every light in every existing scene is a point, and they now all run through
// the area machinery. So the first thing to establish is that a zero-size
// emitter reproduces `max(dot(n,l),0) · (1 - d/range)²` — the expression that
// was inline before — to the precision an 8-bit target can express.
// ---------------------------------------------------------------------------
fn a_point_emitter_is_the_point_light_it_always_was(gpu: &Gpu) {
    // Light straight in front of a wall facing the camera: n = +Z, l = +Z.
    let d = 6.0f32;
    let lit = wall(gpu, &[light([0.0, 0.0, d], [0.6; 3], [POINT, 0.0, 0.0, 0.0], Quat::IDENTITY)], "pt");
    let got = centre(&lit)[1];
    let atten = (1.0 - d / RANGE).powi(2);
    let want = 0.6 * atten; // n·l = 1, albedo 1
    println!("point: want {want:.4}  got {got:.4}");
    assert!(
        (got - want).abs() < 0.012,
        "a zero-size emitter must be the point light it replaced — want {want:.4}, got {got:.4}. \
         Everything below compares shapes AGAINST this, and every scene that exists is made of them."
    );
}

// ---------------------------------------------------------------------------
// 2. A wide emitter lights from its whole surface.
//
// The one thing a point light cannot do. A tall rect standing beside a wall
// lights the far end of that wall, because part of the emitter is near the far
// end; a point at the rect's centre leaves it dark. Measured as the EVENNESS of
// the wall — the ratio between a spot near the light's centre and one well off
// to the side — because that ratio is what somebody looking at the screen would
// call "a soft box" versus "a bulb".
// ---------------------------------------------------------------------------
fn a_rect_lights_from_where_it_is(gpu: &Gpu) {
    let pos = [0.0, 0.0, 3.0];
    let tall = [RECT, 0.05, 4.0, 0.0]; // half-width 5 cm, half-height 4 m
    let pt = wall(gpu, &[light(pos, [0.6; 3], [POINT, 0.0, 0.0, 0.0], Quat::IDENTITY)], "even-pt");
    let rect = wall(gpu, &[light(pos, [0.6; 3], tall, Quat::IDENTITY)], "even-rect");

    // The wall fills the frame; "up the wall" is up the image.
    let even = |px: &[[u8; 4]]| at(px, 0.5, 0.12)[1] / at(px, 0.5, 0.5)[1].max(1e-4);
    let (e_pt, e_rect) = (even(&pt), even(&rect));
    println!("evenness (top ÷ centre): point {e_pt:.3}  tall rect {e_rect:.3}");
    assert!(
        e_rect > e_pt * 1.5,
        "a four-metre emitter must reach up the wall in a way a point at its centre \
         cannot — {e_rect:.3} vs {e_pt:.3}. Equal means the shape is decorative."
    );
}

// ---------------------------------------------------------------------------
// 3. The exact half, against quadrature.
//
// Three surfaces at the SAME world point with three different normals, lit by
// one large rect. The prediction for each comes from integrating the emitter's
// vector irradiance numerically here in Rust — a fine grid over its real
// surface, no shared code with the shader — and then applying the documented
// terminator wrap. Three normals, because a single one can be matched by a
// direction that is wrong in the other two axes, which is exactly how a
// plausible-looking area light hides being a point light.
// ---------------------------------------------------------------------------
fn the_direction_matches_quadrature_over_the_emitter(gpu: &Gpu) {
    // A wide rect, close and off to one side: the case where its own direction
    // is furthest from the direction of its centre.
    let centre_pos = Vec3::new(2.5, 0.0, 2.0);
    let half = (0.05f32, 2.5f32); // a tall narrow strip, 5 m of it
    let shape = [RECT, half.0, half.1, 0.0];
    let extent = half.0.max(half.1);

    let mut worst = 0.0f32;
    for (label, tilt) in [
        ("flat", Quat::IDENTITY),
        ("tilted up", Quat::from_rotation_x(-0.6)),
        ("tilted aside", Quat::from_rotation_y(0.6)),
    ] {
        let n = tilt * Vec3::Z;
        let px = wall_rotated(
            gpu,
            &[light(centre_pos.into(), [0.6; 3], shape, Quat::IDENTITY)],
            tilt,
            "quad",
        );
        let got = centre(&px)[1];

        // Ground truth: quadrature over the emitter's surface for the vector
        // irradiance, then the same wrap the shader documents. The emitter lies
        // in the XY plane at `centre_pos`, so its own normal is Z.
        let (mut w, mut nearest) = (Vec3::ZERO, f32::MAX);
        let steps = 400;
        for i in 0..steps {
            for j in 0..steps {
                let u = (i as f32 + 0.5) / steps as f32 * 2.0 - 1.0;
                let v = (j as f32 + 0.5) / steps as f32 * 2.0 - 1.0;
                let p = centre_pos + Vec3::new(u * half.0, v * half.1, 0.0);
                let r = p.length(); // the shading point is the origin
                let dir = p / r;
                // dA · cosθ at the emitter, over r² — the differential form
                // factor, carried as a vector along `dir`.
                let cos_l = (-dir).dot(Vec3::Z).abs();
                w += dir * (cos_l / (r * r));
                nearest = nearest.min(r);
            }
        }
        let w_hat = w.normalize();
        let s = (extent / centre_pos.length()).clamp(0.0, 1.0);
        let ndl = ((n.dot(w_hat) + s) / (1.0 + s)).clamp(0.0, 1.0);
        let want = 0.6 * ndl * (1.0 - nearest / RANGE).powi(2);
        let err = (got - want).abs();
        worst = worst.max(err);
        println!(
            "{label:>13}: n·ŵ {:.3}  want {want:.4}  got {got:.4}  Δ {err:.4}",
            n.dot(w_hat)
        );
    }
    assert!(
        worst < 0.02,
        "the emitter's own lighting direction must match quadrature over its real \
         surface — worst error {worst:.4}. This is the half that is supposed to be \
         exact; the wrap and the highlight are fits and are checked as pairs."
    );
}

// ---------------------------------------------------------------------------
// 4. A one-sided light has a back.
//
// A window lights the room, not the wall it is set into. The control pair is the
// same emitter with `two_sided` on, which must light both.
// ---------------------------------------------------------------------------
fn a_one_sided_light_has_a_back(gpu: &Gpu, dir: &str) {
    // The wall faces +Z (toward the camera); the light sits in FRONT of it,
    // turned around so its emitting face points away.
    let away = Quat::from_rotation_y(std::f32::consts::PI);
    let pos = [0.0, 0.0, 4.0];
    let one = wall(gpu, &[light(pos, [0.8; 3], [RECT, 1.5, 1.5, 0.0], away)], "one-sided");
    let both = wall(gpu, &[light(pos, [0.8; 3], [RECT, 1.5, 1.5, 1.0], away)], "two-sided");
    let facing = wall(gpu, &[light(pos, [0.8; 3], [RECT, 1.5, 1.5, 0.0], Quat::IDENTITY)], "facing");
    save(&one, &format!("{dir}/area_one_sided.png"));

    let (a, b, c) = (centre(&one)[1], centre(&both)[1], centre(&facing)[1]);
    println!("back of a rect: one-sided {a:.4}  two-sided {b:.4}  ·  facing us {c:.4}");
    assert!(
        a < 0.02,
        "a one-sided emitter must not light what is behind it — {a:.4}. This is the \
         difference between a window and a floating panel."
    );
    assert!(
        b > 0.1 && c > 0.1,
        "…and both the two-sided version and the same emitter turned around must — \
         {b:.4} / {c:.4}. If those went dark too, the facing test is rejecting \
         everything and the assertion above means nothing."
    );
}

// ---------------------------------------------------------------------------
// 5. Size softens the terminator.
//
// A big light wraps around a curve; a point light stops dead at the horizon.
// Measured on a cube's SIDE face, which is edge-on to a light in front of it —
// exactly at the terminator, where the difference lives. Control: the same
// light shrunk to nearly nothing.
// ---------------------------------------------------------------------------
fn size_softens_the_terminator(gpu: &Gpu) {
    let pos = [0.0, 0.0, 5.0];
    let small = block(gpu, &[light(pos, [0.9; 3], [SPHERE, 0.01, 0.0, 0.0], Quat::IDENTITY)], "sharp");
    let big = block(gpu, &[light(pos, [0.9; 3], [SPHERE, 3.0, 0.0, 0.0], Quat::IDENTITY)], "soft");

    // The cube is turned so the frame shows its front face and one side; the
    // side is near-perpendicular to the light.
    let side = |px: &[[u8; 4]]| at(px, 0.22, 0.5)[1];
    let front = |px: &[[u8; 4]]| at(px, 0.6, 0.5)[1];
    println!(
        "terminator: pinpoint side {:.4} / front {:.4}   ·   3 m sphere side {:.4} / front {:.4}",
        side(&small),
        front(&small),
        side(&big),
        front(&big)
    );
    assert!(
        side(&big) > side(&small) + 0.03,
        "a three-metre emitter must reach around onto a face a pinpoint leaves dark — \
         {:.4} vs {:.4}",
        side(&big),
        side(&small)
    );
    assert!(
        front(&small) > 0.1,
        "the CONTROL has to be lit at all — {:.4}. A dark control makes the line above \
         a comparison between two kinds of nothing.",
        front(&small)
    );
}

// ---------------------------------------------------------------------------
// 6. A bar streaks its highlight.
//
// The specular half. A tube light's reflection is a long smear along its axis; a
// point light's is round. Measured as the highlight's extent across the frame in
// each direction — which is a shape, and a constant multiplier cannot fake it.
// ---------------------------------------------------------------------------
fn a_bar_streaks_its_highlight(gpu: &Gpu, dir: &str) {
    let pos = [0.0, 1.5, 1.0];
    let pt = gloss(gpu, &[light(pos, [3.0; 3], [POINT, 0.0, 0.0, 0.0], Quat::IDENTITY)], "gloss-pt");
    let bar = gloss(gpu, &[light(pos, [3.0; 3], [TUBE, 3.0, 0.05, 0.0], Quat::IDENTITY)], "gloss-bar");
    save(&bar, &format!("{dir}/area_bar_highlight.png"));
    save(&pt, &format!("{dir}/area_point_highlight.png"));

    // The tube lies along world X, so its streak runs across the frame.
    let (wx_pt, wy_pt) = highlight_extent(&pt);
    let (wx_bar, wy_bar) = highlight_extent(&bar);
    println!("highlight: point {wx_pt} × {wy_pt} px   ·   3 m bar {wx_bar} × {wy_bar} px");
    assert!(
        wx_bar > wx_pt * 2,
        "a bar must smear its highlight along its own length — {wx_bar} px vs {wx_pt} px"
    );
    assert!(
        wx_bar > wy_bar * 2,
        "…and only along it: {wx_bar} × {wy_bar} px is a blob, not a streak"
    );
}

/// The bright core's width and height in pixels — every pixel over a FIXED
/// brightness, not a fraction of the frame's peak. A relative threshold would
/// rescale itself to whatever is on screen, so a dimmer, wider highlight and a
/// brighter, narrower one would measure the same.
fn highlight_extent(px: &[[u8; 4]]) -> (u32, u32) {
    let thresh = 140u8;
    let (mut x0, mut x1, mut y0, mut y1) = (S, 0u32, S, 0u32);
    for y in 0..S {
        for x in 0..S {
            if px[(y * S + x) as usize][1] >= thresh {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 { (0, 0) } else { (x1 - x0 + 1, y1 - y0 + 1) }
}

// ---- the scenes ---------------------------------------------------------------

/// One light, packed the way the editor packs it.
struct Lamp {
    pos: [f32; 3],
    color: [f32; 3],
    shape: [f32; 4],
    rot: Quat,
}

fn light(pos: [f32; 3], color: [f32; 3], shape: [f32; 4], rot: Quat) -> Lamp {
    Lamp { pos, color, shape, rot }
}

/// A big matte plane facing the camera, filling the frame — the surface every
/// diffuse check reads.
fn wall(gpu: &Gpu, lamps: &[Lamp], label: &str) -> Vec<[u8; 4]> {
    wall_rotated(gpu, lamps, Quat::IDENTITY, label)
}

fn wall_rotated(gpu: &Gpu, lamps: &[Lamp], tilt: Quat, label: &str) -> Vec<[u8; 4]> {
    let mut raster = Raster::new(gpu);
    // The built-in plane already lies in XY facing +Z — straight at the camera.
    let mesh = raster.register(gpu, &plane(1.0), None);
    let mut mat = MaterialParams::flat([1.0, 1.0, 1.0]);
    mat.ambient = 0.0;
    mat.ext_index = raster.push_surface_extras(SurfaceExtras::default());
    let model = Mat4::from_scale_rotation_translation(Vec3::splat(30.0), tilt, Vec3::ZERO);
    draw(gpu, raster, mesh, mat, model, lamps, 9.0, Quat::IDENTITY, label)
}

/// A cube turned so one frame shows its lit front and its near-dark side.
fn block(gpu: &Gpu, lamps: &[Lamp], label: &str) -> Vec<[u8; 4]> {
    let mut raster = Raster::new(gpu);
    let mesh = raster.register(gpu, &cube(1.2), None);
    let mut mat = MaterialParams::flat([1.0, 1.0, 1.0]);
    mat.ambient = 0.0;
    mat.ext_index = raster.push_surface_extras(SurfaceExtras::default());
    let model = Mat4::from_rotation_y(0.6);
    draw(gpu, raster, mesh, mat, model, lamps, 5.0, Quat::IDENTITY, label)
}

/// A glossy floor seen at a grazing angle, where a highlight has room to smear.
fn gloss(gpu: &Gpu, lamps: &[Lamp], label: &str) -> Vec<[u8; 4]> {
    let mut raster = Raster::new(gpu);
    let mesh = raster.register(gpu, &plane(1.0), None);
    // A polished METAL floor: the reflection is the light's own shape, which is
    // the thing being measured. A rough or dielectric floor would give a soft
    // blob whose width says more about the roughness than about the emitter.
    let mut mat = MaterialParams::flat([0.9, 0.9, 0.9]);
    mat.ambient = 0.0;
    let ext = SurfaceExtras { roughness: 0.14, metallic: 1.0, physical: true, ..Default::default() };
    mat.ext_index = raster.push_surface_extras(ext);
    // Tip the plane onto its back so it is a FLOOR (its +Z normal becomes +Y).
    let model = Mat4::from_scale_rotation_translation(
        Vec3::splat(40.0),
        Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        Vec3::ZERO,
    );
    // Looking down the floor from just above it — the shallow angle is what puts
    // the reflection far away and lets its shape read.
    draw(gpu, raster, mesh, mat, model, lamps, 6.0, Quat::from_rotation_x(-0.32), label)
}

#[allow(clippy::too_many_arguments)]
fn draw(
    gpu: &Gpu,
    mut raster: Raster,
    mesh: floptle_render::MeshId,
    mat: MaterialParams,
    model: Mat4,
    lamps: &[Lamp],
    back: f32,
    cam_rot: Quat,
    label: &str,
) -> Vec<[u8; 4]> {
    let eye = cam_rot * Vec3::new(0.0, 0.0, back);
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        cam_rot,
        Projection::Perspective { fov_y: 0.9, near: 0.05, far: 200.0 },
    );
    let mut g = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        // No sun, no ambient: every photon in the frame came out of a lamp.
        light_color: [0.0; 4],
        ambient: [0.0; 4],
        point_count: [lamps.len() as f32, 0.0, 0.0, 0.0],
        ..Default::default()
    };
    for (i, l) in lamps.iter().enumerate().take(16) {
        let p = Vec3::from(l.pos) - eye; // camera-relative, like the editor's gather
        g.point_pos[i] = [p.x, p.y, p.z, RANGE];
        g.point_color[i] = [l.color[0], l.color[1], l.color[2], 0.0];
        g.point_shape[i] = l.shape;
        g.point_rot[i] = [l.rot.x, l.rot.y, l.rot.z, l.rot.w];
    }
    let (tex, view) = target(gpu, label);
    let inst = instance_of_mat(Mat4::from_translation(-eye) * model, &mat);
    raster.draw_scene(
        gpu,
        &view,
        gpu.depth_view(),
        g,
        &[(mesh, None::<TexId>, inst)],
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );
    read_rgba(gpu, &tex)
}

// ---- reading the frame --------------------------------------------------------

fn target(gpu: &Gpu, label: &str) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

/// The pixel at `(fx, fy)`, in LINEAR light — the target is 8-bit sRGB, and
/// comparing those bytes to a linear expectation is a 2.2-power error.
fn at(px: &[[u8; 4]], fx: f32, fy: f32) -> [f32; 3] {
    let i = ((fy * S as f32) as u32 * S + (fx * S as f32) as u32) as usize;
    let p = px[i.min(px.len() - 1)];
    [srgb(p[0]), srgb(p[1]), srgb(p[2])]
}

fn centre(px: &[[u8; 4]]) -> [f32; 3] {
    at(px, 0.5, 0.5)
}

fn srgb(b: u8) -> f32 {
    let c = b as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn save(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flatten().copied().collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("header").write_image_data(&flat).expect("write");
}

fn read_rgba(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (S * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("area-readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("area-readback") });
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
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}
