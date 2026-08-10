//! HDR probe: the scene renders in floating point, light brighter than white
//! survives the whole post chain, and exactly one pass lands it on the display.
//!
//! This probe exists because the format split is invisible until it isn't. Every
//! scene pass — raster, raymarch, particles, lines, grid, triangles, world-space
//! UI, the 2D light composite — declares its colour target when its pipeline is
//! built, and a target that disagrees with the texture it is given is a
//! validation error at DRAW time, in a running editor, on whichever scene
//! happens to contain that one pass. Building all of them here against an HDR
//! GPU turns that into a compile-and-run check.
//!
//! It also asserts the thing the whole exercise is for: that a value of 4.0
//! reaches the end of the chain as 4.0, and that a tonemap can tell it from 1.0.
//! In the old 8-bit pipeline both stored as white and nothing downstream could
//! ever separate them again.
//!
//! Run: cargo run -p floptle-render --example hdr_probe -- <out-dir>

use floptle_render::{
    instance_of_mat, plane, Globals, Gpu, MaterialParams, PostSettings, PostStack, Raster, Tonemap,
};
use glam::Mat4;

const S: u32 = 128;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless_hdr(S, S);
    assert_eq!(
        gpu.scene_format(),
        Gpu::HDR_FORMAT,
        "an HDR gpu must hand the scene a floating-point format"
    );
    assert_ne!(
        gpu.scene_format(),
        gpu.surface_format(),
        "…and it must differ from the display format, or this probe proves nothing"
    );

    every_scene_pass_builds(&gpu);

    // --- a 4× WHITE: does anything brighter than white survive at all? -------
    let white = chain_reads(&gpu, [1.0, 1.0, 1.0]);
    let lum = |c: [u8; 3]| c[0] as u32 + c[1] as u32 + c[2] as u32;
    println!(
        "4× white  clip {:?} reinhard {:?} aces {:?} agx {:?}",
        white[0], white[1], white[2], white[3]
    );
    // CLIP is the old behaviour and the control: 4.0 clamps to white, so it is
    // indistinguishable from a 1.0 white and every curve must land under it. If
    // clip were not saturated the scene never got bright in the first place and
    // nothing below means anything.
    assert!(white[0][0] >= 254, "a 4× white must saturate under Clip, got {:?}", white[0]);
    for (name, c) in [("reinhard", white[1]), ("aces", white[2]), ("agx", white[3])] {
        assert!(
            lum(c) < lum(white[0]),
            "{name} must pull a 4× white back below the clip, got {c:?} vs {:?} — a \
             tonemap that changes nothing means the scene reached it ALREADY clamped, \
             which is the whole bug this pipeline exists to fix",
            white[0]
        );
        assert!(c[0] > 100, "{name} must still leave it bright, got {c:?}");
    }
    // ACES has a shoulder and Reinhard does not, so ACES holds a highlight up.
    // AgX is deliberately NOT compared here: its distinguishing move is a
    // desaturation, and a neutral white has no saturation to give away — on grey
    // it reduces to Reinhard exactly, which is correct and not worth asserting.
    assert!(
        lum(white[1]) < lum(white[2]),
        "ACES's shoulder must keep a highlight brighter than Reinhard's flat \
         compression — got {:?} vs {:?}",
        white[1],
        white[2]
    );

    // --- a 4× saturated BLUE: the reason AgX exists -------------------------
    //
    // A very bright coloured light should read as bright. Clip cannot say so —
    // blue is already at 255 and the other channels are at 0, so four times the
    // light looks exactly like one time it: a flat block of pure blue. AgX
    // answers by desaturating toward white as it climbs, the way film and a
    // sensor do, so the extra light shows up in the OTHER channels.
    let blue = chain_reads(&gpu, [0.0, 0.02, 1.0]);
    println!(
        "4× blue   clip {:?} reinhard {:?} aces {:?} agx {:?}",
        blue[0], blue[1], blue[2], blue[3]
    );
    assert!(
        blue[0][0] < 40 && blue[0][2] >= 254,
        "the CONTROL must be a flat clipped blue — four times the light and \
         nowhere for it to go. Got {:?}",
        blue[0]
    );
    // How much colour is left, as a fraction of the brightest channel. This is
    // the measure, not the raw red: Reinhard and ACES compress by different
    // amounts, so comparing red channels alone would be comparing their overall
    // exposure and not what happens to the HUE.
    let sat = |c: [u8; 3]| {
        let hi = *c.iter().max().unwrap() as f32;
        let lo = *c.iter().min().unwrap() as f32;
        if hi > 0.0 { (hi - lo) / hi } else { 0.0 }
    };
    println!(
        "  saturation left: clip {:.2} reinhard {:.2} aces {:.2} agx {:.2}",
        sat(blue[0]),
        sat(blue[1]),
        sat(blue[2]),
        sat(blue[3])
    );
    assert!(
        sat(blue[3]) < sat(blue[2]) * 0.5,
        "AgX must whiten a bright saturated light much further than ACES — got \
         {:.2} of its colour left against ACES's {:.2}. Similar numbers mean the \
         spill never fired, and a bright blue light will keep reading as a \
         sticker rather than as a light.",
        sat(blue[3]),
        sat(blue[2])
    );
    assert!(
        sat(blue[3]) < sat(blue[1]) * 0.5,
        "…and further than Reinhard, which compresses each channel on its own and \
         so keeps the hue exactly — got {:.2} against {:.2}",
        sat(blue[3]),
        sat(blue[1])
    );
    assert!(
        blue[3][0] > blue[0][0] + 40,
        "and it must do it by raising the DARK channels, not by dimming the bright \
         one — the extra light has to read as more light. Got red {} against the \
         clipped control's {}",
        blue[3][0],
        blue[0][0]
    );

    println!("hdr probe OK");
}

/// Build every pass that renders into the scene target. Constructing the pass is
/// the assertion: a pipeline whose colour target disagrees with the scene format
/// fails here rather than in somebody's editor.
fn every_scene_pass_builds(gpu: &Gpu) {
    let raster = Raster::new(gpu);
    let _raymarch = floptle_render::Raymarch::new(gpu);
    let _particles = floptle_render::particles::Particles::new(gpu);
    let _lines = floptle_render::Lines::new(gpu);
    let _tris = floptle_render::Tris::new(gpu);
    let _grid = floptle_render::grid::Grid::new(gpu);
    let _ui = floptle_render::Ui::new(gpu);
    // The post chain straddles both formats — its scratch is the scene format
    // and only its terminal pass is the display's.
    let _post = PostStack::new(gpu, S, S);
    drop(raster);
}

/// Draw a plane emitting `4 × color` into the HDR scene target, run the chain to
/// an 8-bit display target under each tonemap, and read back the middle pixel.
/// Returns the RGB under Clip / Reinhard / ACES / AgX, in that order.
fn chain_reads(gpu: &Gpu, color: [f32; 3]) -> [[u8; 3]; 4] {
    let mut raster = Raster::new(gpu);
    let post = PostStack::new(gpu, S, S);
    let mesh = raster.register(gpu, &plane(4.0), None);

    let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("hdr-out"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());

    // Unlit + emissive 4×: a surface emitting four times as much light as a
    // white one, needing nothing from the light rig to do it.
    let mut mat = MaterialParams::flat([1.0, 1.0, 1.0]);
    mat.unlit = true;
    mat.emissive = color;
    mat.emissive_strength = 4.0;
    mat.color = [0.0, 0.0, 0.0];
    let eye = glam::Vec3::new(0.0, 0.0, 3.0);
    let cam = floptle_render::RenderCamera::new(
        eye.as_dvec3(),
        glam::Quat::IDENTITY,
        floptle_render::Projection::Perspective { fov_y: 0.8, near: 0.02, far: 100.0 },
    );
    let globals =
        Globals { view_proj: cam.view_proj(1.0).to_cols_array_2d(), ..Default::default() };
    let raw = instance_of_mat(Mat4::from_translation(-eye), &mat);

    let mut read = |t: Tonemap| -> [u8; 3] {
        raster.draw_scene(
            gpu,
            post.input_view(),
            gpu.depth_view(),
            globals,
            &[(mesh, None, raw)],
            Some([0.0, 0.0, 0.0, 1.0]),
            None,
        );
        post.run(gpu, &PostSettings { tonemap: t, ..Default::default() }, None, &out_view);
        centre(gpu, &out)
    };
    [read(Tonemap::Clip), read(Tonemap::Reinhard), read(Tonemap::Aces), read(Tonemap::Agx)]
}

fn centre(gpu: &Gpu, tex: &wgpu::Texture) -> [u8; 3] {
    let bpp = 4u32;
    let padded = (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("hdr-readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("hdr-readback") });
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
    let i = ((S / 2) * padded + (S / 2) * bpp) as usize;
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let v = if bgra {
        [view[i + 2], view[i + 1], view[i]]
    } else {
        [view[i], view[i + 1], view[i + 2]]
    };
    drop(view);
    buf.unmap();
    v
}
