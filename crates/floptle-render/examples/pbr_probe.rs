//! Surface-map + metal-rough probe: does a normal map actually tilt the light,
//! does roughness actually widen the highlight, does metallic actually take the
//! surface's colour — and do the retro artefacts actually fire.
//!
//! Every check has a **control** rendered in the same pass with the same light,
//! because every one of these can look plausible while doing nothing. A normal
//! map bound to a shader that never samples it gives a perfectly lit sphere. A
//! roughness that never reaches the BRDF gives a perfectly good highlight. The
//! control is what tells those apart from working.
//!
//! Run: cargo run -p floptle-render --example pbr_probe -- <out-dir>

use floptle_render::{
    instance_of_mat, plane, uv_sphere, Globals, Gpu, MaterialParams, Projection, Raster,
    RenderCamera, SurfaceExtras, TexId, TexSampling, TextureData,
};
use glam::{Mat4, Quat, Vec3};

const S: u32 = 256;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);

    the_default_normal_map_is_the_identity(&gpu);
    normal_map_tilts_the_light(&gpu, &dir);
    roughness_widens_the_highlight(&gpu, &dir);
    metal_takes_the_surface_colour(&gpu, &dir);
    retro_flags_change_the_picture(&gpu, &dir);

    println!("pbr probe OK");
}

// ---------------------------------------------------------------------------
// 0. A surface with NO normal map is shaded by its own geometry.
//
// Asserted against arithmetic rather than against a control, because there is no
// control to have: every material in the engine takes this path, so a wrong
// default is wrong everywhere at once and everything still looks lit.
//
// A cube's front face is dead-on to the camera with a light at the camera, so
// N·L is exactly 1 and the pixel is exactly the light's own brightness (times
// its distance falloff, which is arithmetic too). A shading normal that has been
// tilted off the face reads as `cos(tilt)` instead.
//
// This is not hypothetical. Every texture is uploaded as sRGB — correct for a
// base colour, wrong for a normal map — and until v0.45 the flat default
// (128,128,255) was decoded through that curve into (0.216, 0.216, 1), which is
// a surface bent 39°. It shaded every unmapped material in the engine, and it
// was invisible: a uniform tilt still looks like a lit object.
// ---------------------------------------------------------------------------
fn the_default_normal_map_is_the_identity(gpu: &Gpu) {
    let mut raster = Raster::new(gpu);
    let mesh = raster.register(gpu, &floptle_render::cube(0.55), None);
    let mut mat = MaterialParams::flat([1.0, 1.0, 1.0]);
    mat.ambient = 0.0;
    mat.ext_index = raster.push_surface_extras(SurfaceExtras::default());

    let (color, view) = target(gpu, "flat-normal");
    let eye = Vec3::new(0.0, 0.0, 3.0);
    // The light AT the camera: L is the view direction, so the front face's N·L
    // is 1 and nothing about the geometry is in question.
    const RANGE: f32 = 100.0;
    const INTENSITY: f32 = 0.6;
    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    point_pos[0] = [0.0, 0.0, 0.0, RANGE];
    point_color[0] = [INTENSITY, INTENSITY, INTENSITY, 0.0];
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 0.8, near: 0.02, far: 100.0 },
    );
    let globals = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        ambient: [0.0; 4],
        point_count: [1.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        ..Default::default()
    };
    raster.draw_scene(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &[(mesh, None::<TexId>, instance_of_mat(Mat4::from_translation(-eye), &mat))],
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );

    let px = read_rgba(gpu, &color);
    // The shader's smooth falloff, restated: `intensity · (1 − d/range)²`.
    let d = eye.z - 0.55;
    let x = 1.0 - d / RANGE;
    let want = INTENSITY * x * x;
    let got = srgb_to_linear(px[idx(0.5, 0.5)][0]);
    println!("flat normal: front face {got:.4}, N·L = 1 predicts {want:.4}");
    assert!(
        (got - want).abs() < 0.02,
        "a surface with no normal map must shade by its own geometry: got {got:.4}, \
         N·L = 1 predicts {want:.4} (a ratio of {:.3} — acos of that is the angle the \
         shading normal has been bent by).",
        got / want
    );
}

/// An 8-bit sRGB byte back to linear light — the target is sRGB, so the bytes
/// are encoded and comparing them to a computed number without undoing that is
/// a 2.2-power error.
fn srgb_to_linear(b: u8) -> f32 {
    let c = b as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

// ---------------------------------------------------------------------------
// 1. A normal map tilts the light.
//
// One flat plane, dead-on to the camera, lit from the LEFT. A normal map whose
// left half tilts toward the light and right half tilts away must produce two
// visibly different halves. The control is the same plane with no normal map:
// being flat and uniformly lit, its halves must match.
//
// This is also the test that the tangent frame works at all — the frame here is
// derived from screen-space derivatives, and a plane has no tangent attribute to
// fall back on.
// ---------------------------------------------------------------------------
fn normal_map_tilts_the_light(gpu: &Gpu, dir: &str) {
    let mut raster = Raster::new(gpu);
    // Tangent-space normals: left half tilted toward -X, right half toward +X.
    // Encoded the usual way, (n + 1) / 2 into RGB.
    let mut pixels = Vec::new();
    for _y in 0..64u32 {
        for x in 0..64u32 {
            let nx: f32 = if x < 32 { -0.8 } else { 0.8 };
            let nz = (1.0 - nx * nx).max(0.0).sqrt();
            pixels.extend_from_slice(&[
                ((nx * 0.5 + 0.5) * 255.0) as u8,
                128,
                ((nz * 0.5 + 0.5) * 255.0) as u8,
                255,
            ]);
        }
    }
    let nmap = raster.register_texture(
        gpu,
        &TextureData { pixels, width: 64, height: 64 },
        TexSampling::default(),
    );
    // Nearest, so the two halves stay two halves and the seam does not bleed.
    let white = raster.register_texture(
        gpu,
        &TextureData { pixels: vec![255, 255, 255, 255], width: 1, height: 1 },
        TexSampling::default(),
    );
    let mapped = raster.material_set(gpu, Some(white), [Some(nmap), None, None, None]);

    // A CUBE, not a plane. The plane primitive's triangles wind away from a
    // camera on +Z, so `facing_normal` flips its shading normal to point away
    // from the viewer and a light in front of it contributes nothing — the whole
    // quad renders black and every measurement below would be 0 vs 0. A cube's
    // front face is unambiguous.
    let mesh = raster.register(gpu, &floptle_render::cube(0.55), None);
    let mut mat = MaterialParams::flat([1.0, 1.0, 1.0]);
    mat.ambient = 0.0;
    mat.ext_index = raster.push_surface_extras(SurfaceExtras::default());

    let (color, view) = target(gpu, "normal-map");
    let eye = Vec3::new(0.0, 0.0, 3.5);
    // Light in front and to the LEFT, so a leftward tilt catches it and a
    // rightward tilt loses it.
    let globals = lit_globals(eye, Vec3::new(-6.0, 0.0, 6.0), 1.6);
    // Two cubes side by side in ONE pass: mapped on the left, unmapped control
    // on the right. Same light, same frame, same everything else.
    let left = instance_of_mat(Mat4::from_translation(Vec3::new(-0.75, 0.0, 0.0) - eye), &mat);
    let right = instance_of_mat(Mat4::from_translation(Vec3::new(0.75, 0.0, 0.0) - eye), &mat);
    raster.draw_scene(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &[(mesh, Some::<TexId>(mapped), left), (mesh, Some::<TexId>(white), right)],
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );

    let px = read_rgba(gpu, &color);
    let at = |fx: f32, fy: f32| lum(px[idx(fx, fy)]);
    // Both sample pairs sit well inside their cube's front face.
    let m_left = at(0.14, 0.5);
    let m_right = at(0.36, 0.5);
    let c_left = at(0.64, 0.5);
    let c_right = at(0.86, 0.5);
    println!("normal map: mapped {m_left:.3} vs {m_right:.3} | flat control {c_left:.3} vs {c_right:.3}");

    save(&px, &format!("{dir}/pbr_normal_map.png"));
    assert!(
        c_left > 0.02 && c_right > 0.02,
        "the CONTROL must be LIT before its evenness means anything — got \
         {c_left:.3} and {c_right:.3}"
    );
    assert!(
        (c_left - c_right).abs() < 0.06,
        "the CONTROL face has no normal map and must be evenly lit — got {c_left:.3} vs \
         {c_right:.3}. If the control is uneven the test below proves nothing."
    );
    assert!(
        m_left > m_right + 0.10,
        "the half tilted TOWARD the light must be brighter than the half tilted away — \
         got {m_left:.3} vs {m_right:.3}. Equal halves mean the normal map never \
         reached the shading normal (an unbound slot, or a tangent frame that \
         collapsed)."
    );
}

// ---------------------------------------------------------------------------
// 2. Roughness widens the highlight.
//
// Two identical spheres, one fairly smooth and one rough, lit from beside the
// camera so each highlight sits in the middle of its own disc. Asserted on the
// SHAPE of the lobe, not its brightness: the same light concentrated into fewer,
// brighter pixels. Peak brightness alone would pass against a shader that merely
// multiplied everything by roughness, so the width is the half that matters.
// ---------------------------------------------------------------------------
fn roughness_widens_the_highlight(gpu: &Gpu, dir: &str) {
    let mut raster = Raster::new(gpu);
    let mesh = raster.register(gpu, &uv_sphere(0.8, 96, 128), None);
    let (color, view) = target(gpu, "roughness");
    let eye = Vec3::new(0.0, 0.0, 5.0);
    let globals = lit_globals(eye, Vec3::new(0.0, 0.0, 1.0), 6.0);

    let sphere = |raster: &mut Raster, rough: f32, x: f32| {
        // A nearly black surface, so what is measured is the specular lobe and
        // not a diffuse wash under it.
        let mut mat = MaterialParams::flat([0.02, 0.02, 0.02]);
        mat.ambient = 0.0;
        mat.ext_index = raster.push_surface_extras(SurfaceExtras {
            roughness: rough,
            metallic: 0.0,
            physical: true,
            ..SurfaceExtras::default()
        });
        instance_of_mat(Mat4::from_translation(Vec3::new(x, 0.0, 0.0) - eye), &mat)
    };
    let smooth = sphere(&mut raster, 0.25, -1.0);
    let rough = sphere(&mut raster, 0.85, 1.0);
    raster.draw_scene(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &[(mesh, None, smooth), (mesh, None, rough)],
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );

    let px = read_rgba(gpu, &color);
    // Peak, and how many pixels are within half of it — the lobe's width,
    // measured against that surface's OWN peak so the number says nothing about
    // how bright the lamp happens to be.
    let lobe = |x0: u32, x1: u32| -> (f32, usize) {
        let ls: Vec<f32> =
            (0..S).flat_map(|y| (x0..x1).map(move |x| (y, x))).map(|(y, x)| lum(px[(y * S + x) as usize])).collect();
        let peak = ls.iter().copied().fold(0.0f32, f32::max);
        let width = ls.iter().filter(|&&l| l > peak * 0.5).count();
        (peak, width)
    };
    let (sp, sw) = lobe(0, S / 2);
    let (rp, rw) = lobe(S / 2, S);
    println!("roughness: smooth peak {sp:.3} width {sw} | rough peak {rp:.3} width {rw}");

    save(&px, &format!("{dir}/pbr_roughness.png"));
    assert!(
        sp > 0.2 && rp > 0.02,
        "both spheres must actually be lit before their lobes can be compared — \
         got peaks {sp:.3} and {rp:.3}"
    );
    assert!(
        sp > rp * 1.5,
        "a smooth surface concentrates the light, so its peak must clearly exceed \
         the rough one's — got {sp:.3} vs {rp:.3}"
    );
    assert!(
        rw > sw * 3,
        "and it must concentrate it into FEWER pixels: the rough sphere's lobe must \
         be far wider — got {rw} vs {sw}. A brighter peak with the SAME width means \
         roughness never reached the BRDF's distribution term."
    );
}

// ---------------------------------------------------------------------------
// 3. A metal's highlight takes the surface's colour; a dielectric's stays white.
//
// The single most visible difference between the two, and the thing a half-wired
// metallic lane gets wrong: same red albedo, same light, and only the metal's
// highlight should be red.
//
// Measured over the middle of each disc, NOT at the brightest pixel anywhere.
// Fresnel drives every surface to a white reflection at grazing angles, so the
// brightest pixel on either sphere is a white rim pixel and the two would come
// out identical — which is exactly what this probe did before the window was
// narrowed, and it looked like a broken metallic lane.
// ---------------------------------------------------------------------------
fn metal_takes_the_surface_colour(gpu: &Gpu, dir: &str) {
    let mut raster = Raster::new(gpu);
    let mesh = raster.register(gpu, &uv_sphere(0.8, 64, 96), None);
    let (color, view) = target(gpu, "metal");
    let eye = Vec3::new(0.0, 0.0, 5.0);
    let globals = lit_globals(eye, Vec3::new(0.0, 0.0, 1.0), 0.8);

    let sphere = |raster: &mut Raster, metallic: f32, x: f32| {
        let mut mat = MaterialParams::flat([0.9, 0.1, 0.1]);
        mat.ambient = 0.0;
        mat.ext_index = raster.push_surface_extras(SurfaceExtras {
            roughness: 0.4,
            metallic,
            physical: true,
            ..SurfaceExtras::default()
        });
        instance_of_mat(Mat4::from_translation(Vec3::new(x, 0.0, 0.0) - eye), &mat)
    };
    let metal = sphere(&mut raster, 1.0, -1.0);
    let plastic = sphere(&mut raster, 0.0, 1.0);
    raster.draw_scene(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &[(mesh, None, metal), (mesh, None, plastic)],
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );

    let px = read_rgba(gpu, &color);
    // Mean colour over a small box at the centre of each sphere, where the
    // highlight sits and the view is head-on.
    let centre = |cx: f32| -> [f32; 3] {
        let (x0, x1) = (((cx - 0.045) * S as f32) as u32, ((cx + 0.045) * S as f32) as u32);
        let (y0, y1) = ((S as f32 * 0.455) as u32, (S as f32 * 0.545) as u32);
        let mut acc = [0.0f32; 3];
        let mut n = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                let p = px[(y * S + x) as usize];
                for c in 0..3 {
                    acc[c] += p[c] as f32;
                }
                n += 1.0;
            }
        }
        [acc[0] / n, acc[1] / n, acc[2] / n]
    };
    let m = centre(0.26);
    let p = centre(0.74);
    // How much of the reflection is NOT the surface's red. A white highlight
    // scores near 1; a red one scores near 0.
    //
    // In LINEAR light, not in the sRGB bytes. A ratio of two encoded values is
    // not the ratio of the two values, and the encoding squashes exactly the
    // dark end where this measurement lives — which flattered the metal and
    // penalised the dielectric until the two were only 1.3× apart in a
    // measurement whose real separation is 1.8×.
    let whiteness = |c: [f32; 3]| srgb_to_linear(c[2] as u8) / srgb_to_linear(c[0] as u8).max(1e-4);
    let (wm, wp) = (whiteness(m), whiteness(p));
    println!("metal centre {m:?} whiteness {wm:.3} | dielectric {p:?} whiteness {wp:.3}");

    save(&px, &format!("{dir}/pbr_metal.png"));
    assert!(
        m[0] > 20.0 && p[0] > 20.0,
        "both spheres must be lit at their centres — got {m:?} and {p:?}"
    );
    assert!(
        wp > wm * 1.5,
        "a dielectric reflects a WHITE highlight and a metal reflects its own \
         colour, so the dielectric's blue-to-red ratio must clearly exceed the \
         metal's — got {wp:.3} vs {wm:.3}"
    );
    assert!(
        wm < 0.25,
        "the metal's highlight must be red like its albedo, got {m:?} (whiteness \
         {wm:.3}) — a white highlight on a metal means the metallic lane never \
         reached F0"
    );
}

// ---------------------------------------------------------------------------
// 4. The retro artefacts fire.
//
// Two of them, chosen because each has an unambiguous signature:
//   - dither alpha punches HOLES (background pixels inside the silhouette),
//     where blending would leave the silhouette solid;
//   - vertex jitter MOVES the silhouette (its edge lands on different pixels).
// The control for both is the identical draw with the flag off.
// ---------------------------------------------------------------------------
fn retro_flags_change_the_picture(gpu: &Gpu, dir: &str) {
    let mut raster = Raster::new(gpu);
    let mesh = raster.register(gpu, &plane(1.0), None);
    let eye = Vec3::new(0.0, 0.0, 3.0);
    let globals = lit_globals(eye, Vec3::new(0.0, 0.0, 6.0), 2.0);

    // --- dither: a half-opaque plane over a black background.
    let render_alpha = |raster: &mut Raster, dither: bool| -> Vec<[u8; 4]> {
        let (color, view) = target(gpu, "dither");
        let mut mat = MaterialParams::flat([1.0, 1.0, 1.0]);
        mat.unlit = true;
        mat.alpha = 0.5;
        mat.ext_index = raster.push_surface_extras(SurfaceExtras {
            retro: floptle_core::Retro { dither_alpha: dither, ..Default::default() },
            ..SurfaceExtras::default()
        });
        let raw = instance_of_mat(Mat4::from_translation(-eye), &mat);
        raster.draw_scene(
            gpu,
            &view,
            gpu.depth_view(),
            globals,
            &[(mesh, None, raw)],
            Some([0.0, 0.0, 0.0, 1.0]),
            None,
        );
        read_rgba(gpu, &color)
    };
    let blended = render_alpha(&mut raster, false);
    let dithered = render_alpha(&mut raster, true);
    // Inside the plane's silhouette (the middle of the frame), count pixels that
    // stayed pure background.
    let holes = |px: &[[u8; 4]]| -> usize {
        let mut n = 0;
        for y in (S / 3)..(2 * S / 3) {
            for x in (S / 3)..(2 * S / 3) {
                if lum(px[(y * S + x) as usize]) < 0.02 {
                    n += 1;
                }
            }
        }
        n
    };
    let (hb, hd) = (holes(&blended), holes(&dithered));
    println!("dither alpha: holes blended {hb} | dithered {hd}");
    save(&dithered, &format!("{dir}/pbr_retro_dither.png"));
    assert!(
        hb == 0,
        "the CONTROL blends, so its silhouette must be solid — got {hb} background \
         pixels inside it"
    );
    let cells = ((2 * S / 3) - (S / 3)).pow(2) as usize;
    assert!(
        hd > cells / 4 && hd < cells * 3 / 4,
        "screen-door transparency at 50% must punch out roughly half the pixels — \
         got {hd} of {cells}"
    );

    // --- jitter: the same rotated plane, snapped to a coarse screen grid.
    let render_jitter = |raster: &mut Raster, jitter: f32| -> Vec<[u8; 4]> {
        let (color, view) = target(gpu, "jitter");
        let mut mat = MaterialParams::flat([1.0, 1.0, 1.0]);
        mat.unlit = true;
        mat.ext_index = raster.push_surface_extras(SurfaceExtras {
            retro: floptle_core::Retro { jitter, ..Default::default() },
            ..SurfaceExtras::default()
        });
        // Rotated off-axis so the silhouette edge falls between grid cells —
        // an axis-aligned quad could snap to exactly where it already was.
        let m = Mat4::from_translation(-eye)
            * Mat4::from_quat(Quat::from_rotation_z(0.31) * Quat::from_rotation_y(0.4));
        let raw = instance_of_mat(m, &mat);
        raster.draw_scene(
            gpu,
            &view,
            gpu.depth_view(),
            globals,
            &[(mesh, None, raw)],
            Some([0.0, 0.0, 0.0, 1.0]),
            None,
        );
        read_rgba(gpu, &color)
    };
    let straight = render_jitter(&mut raster, 0.0);
    let snapped = render_jitter(&mut raster, 12.0);
    let moved = straight
        .iter()
        .zip(&snapped)
        .filter(|(a, b)| (lum(**a) - lum(**b)).abs() > 0.25)
        .count();
    println!("vertex jitter: {moved} pixels changed");
    save(&snapped, &format!("{dir}/pbr_retro_jitter.png"));
    assert!(
        moved > 200,
        "snapping vertices to a 12-step screen grid must visibly move the \
         silhouette — only {moved} pixels changed, which is a jitter lane that \
         never reached the clip position"
    );
}

// ---- shared helpers --------------------------------------------------------

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

/// One white POINT light at camera-relative `light`, no ambient.
///
/// A point light rather than the key light on purpose: the key light lives in
/// the raymarch pass's globals (group 2), and a standalone probe passes
/// `field: None` — which binds the zeroed fallback, so the sun's colour is
/// black and every surface renders unlit-dark. The point lights are group 0,
/// which this probe owns. (This is worth knowing before writing another
/// lighting probe: an all-black frame here is not a shading bug.)
///
/// No ambient, because every check below measures a highlight and ambient would
/// be a floor under all of them.
fn lit_globals(eye: Vec3, light: Vec3, intensity: f32) -> Globals {
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 0.8, near: 0.02, far: 100.0 },
    );
    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    point_pos[0] = [light.x, light.y, light.z, 14.0];
    point_color[0] = [intensity, intensity, intensity, 0.0];
    Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        ambient: [0.0, 0.0, 0.0, 0.0],
        point_count: [1.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        ..Default::default()
    }
}

fn idx(fx: f32, fy: f32) -> usize {
    ((fy * S as f32) as u32 * S + (fx * S as f32) as u32) as usize
}

fn lum(p: [u8; 4]) -> f32 {
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

fn read_rgba(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
    let mut o = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * bpp) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            o.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    o
}

fn save(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
