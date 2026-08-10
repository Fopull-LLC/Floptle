//! Depth-of-field probe: every knob is wired to the thing it is named after.
//!
//! Depth of field is eight settings sharing four uniform lanes, and the failure
//! mode of that arrangement is silent — a swapped lane gives a picture that is
//! still plausibly blurred, just not by what was asked for. So each check here
//! is a CONTROL PAIR: render the same scene twice, change exactly one knob, and
//! assert the specific thing that knob is supposed to change.
//!
//! The scene is two emissive cards, one in front of the focus distance and one
//! behind it, sized and placed so they cover the same amount of screen and land
//! in the top and bottom halves whatever their depth. That is what makes "which
//! one blurred more" a fair question.
//!
//! Run: cargo run -p floptle-render --example dof_probe -- <out-dir>

use floptle_render::{
    instance_of_mat, plane, Globals, Gpu, MaterialParams, PostSettings, PostStack, Projection,
    Raster, RenderCamera, SsaoFrame,
};
use glam::{Mat4, Vec3};

const S: u32 = 192;
const FOCUS: f32 = 10.0;
const FOV: f32 = 0.9;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    // HDR, because `dof_highlight` is defined against light BRIGHTER than white
    // and an 8-bit scene target has nowhere to put any.
    let gpu = Gpu::headless_hdr(S, S);
    let mut rig = Rig::new(&gpu);

    let base = PostSettings {
        dof_focus: FOCUS,
        dof_range: 8.0,
        dof_near_range: 1.0,
        dof_max_blur: 14.0,
        ..Default::default()
    };

    // ---- near and far are two different ranges ------------------------------
    //
    // One card four units in FRONT of focus, one four units BEHIND. With a tight
    // near range and a loose far one the front card is fully defocused while the
    // back one is only halfway there — and widening the near range has to
    // SHARPEN the front card. Asserting only the first half would pass on a
    // build that ignored both numbers and blurred by raw distance.
    let pair = [Card::at(FOCUS - 4.0, 0.45), Card::at(FOCUS + 4.0, -0.45)];
    let tight = rig.render(&gpu, &pair, &base);
    let (near_a, far_a) = (lit(&tight, Half::Top), lit(&tight, Half::Bottom));
    let loose = rig.render(&gpu, &pair, &PostSettings { dof_near_range: 8.0, ..base });
    let (near_b, far_b) = (lit(&loose, Half::Top), lit(&loose, Half::Bottom));
    write_png(&format!("{dir}/dof_near_tight.png"), &tight);
    write_png(&format!("{dir}/dof_near_loose.png"), &loose);
    println!(
        "near/far: tight near {near_a} vs far {far_a};  loose near {near_b} vs far {far_b}  (lit px)"
    );
    // Lit AREA, not blur radius: the card has a size of its own, so doubling the
    // circle of confusion is well short of doubling the count. A modest ratio is
    // the honest expectation here; the exact-match check below is the sharp one.
    assert!(
        near_a * 4 > far_a * 5,
        "a 1-unit near range must defocus the FRONT card more than an 8-unit far range \
         defocuses the back one — got {near_a} against {far_a}. Similar numbers mean both \
         sides are reading the same range lane, which is what this pass did before there \
         were two of them."
    );
    assert!(
        near_b + 40 < near_a,
        "widening the near range must SHARPEN the front card — it went from {near_a} to \
         {near_b} lit pixels. If it grew or held still, near and far are crossed."
    );
    // The exact one. The cards sit four units either side of focus, so once the
    // two ranges are equal the frame is symmetric and the halves must agree —
    // which no single-range implementation and no crossed pair can produce.
    assert!(
        near_b.abs_diff(far_b) * 20 < far_b,
        "with the near and far ranges both at 8, two cards four units either side of focus \
         must blur IDENTICALLY — got {near_b} against {far_b}. Any gap here is asymmetry \
         that does not come from the settings."
    );

    // ---- the iris has blades -----------------------------------------------
    //
    // A round aperture spreads a point into a disc; a bladed one spreads it into
    // a polygon, which is where hexagonal bokeh comes from. Measured as how much
    // the blur footprint's radius varies with angle: a circle is 1.0, and a
    // triangle is about 2 (a regular polygon's inradius is cos(pi/n) of its
    // circumradius, and three blades is the most lopsided case there is).
    let dot = [Card::point(FOCUS + 8.0)];
    // 64 taps, because the gather is a SAMPLING of the aperture: with a source
    // small enough to measure the shape against, a 16-tap kernel catches it in
    // only a few percent of the disc and the footprint comes back as speckle.
    let wide = PostSettings { dof_max_blur: 22.0, dof_quality: 64, ..base };
    let round = rig.render(&gpu, &dot, &wide);
    let bladed = rig.render(&gpu, &dot, &PostSettings { dof_blades: 3, ..wide });
    write_png(&format!("{dir}/dof_iris_round.png"), &round);
    write_png(&format!("{dir}/dof_iris_bladed.png"), &bladed);
    let (rr, rb) = (roundness(&round), roundness(&bladed));
    println!("iris: round {rr:.2}  3 blades {rb:.2}  (widest radius / narrowest)");
    // Not 1.0: the source is a square card, so even a perfectly round aperture
    // leaves a footprint that reaches a little further at the card's corners.
    // What matters is that it is nearly round, and that blades move it a lot.
    assert!(
        rr < 1.35,
        "a 0-blade iris must be near-ROUND — its footprint is {rr:.2}× wider one way than \
         another. The spiral kernel is isotropic and the source is only a few pixels across, \
         so anything much above this means the blade shaping is running unasked."
    );
    assert!(
        rb > rr + 0.4,
        "3 blades must visibly flatten the bokeh — {rb:.2} against the round {rr:.2}. A regular \
         polygon reaches `cos(pi/n)` of its circumradius at the flats, so three blades should \
         land near 2. Equal numbers mean the blade count never reached the shader."
    );

    // ---- highlights survive the blur ---------------------------------------
    //
    // Averaging a bright point with its dark neighbours is what turns bokeh into
    // grey mush. Weighting taps by how far past white they are keeps the disc
    // bright, and it only means anything because the frame arriving here is
    // scene-referred — there IS something past white to find.
    let card = [Card::at(FOCUS + 6.0, 0.0)];
    let dull = rig.render(&gpu, &card, &base);
    let boosted = rig.render(&gpu, &card, &PostSettings { dof_highlight: 6.0, ..base });
    write_png(&format!("{dir}/dof_highlight.png"), &boosted);
    let (md, mb) = (mean_lit(&dull), mean_lit(&boosted));
    println!("highlight: off {md:.1}  boosted {mb:.1}  (mean luma over the disc)");
    assert!(
        mb > md + 6.0,
        "highlight bokeh must keep the disc BRIGHTER — {mb:.1} against {md:.1}. No difference \
         means the boost lane is dead, or the scene never got brighter than white in the \
         first place (check that this probe is running on an HDR device)."
    );

    // ---- the focus view says which SIDE ------------------------------------
    //
    // The one thing a blurred frame cannot tell you is whether a pixel is in
    // front of the focus or behind it, which is exactly what you need in order
    // to move the focus. Cool in front, warm behind.
    let dim_pair = [Card::dim(FOCUS - 4.0, 0.45), Card::dim(FOCUS + 4.0, -0.45)];
    let shown = rig.render(&gpu, &dim_pair, &PostSettings { dof_show_focus: true, ..base });
    write_png(&format!("{dir}/dof_show_focus.png"), &shown);
    // Measured ON the cards, not over the half-frame: the focus view tints the
    // whole picture including the empty background, and the background is far —
    // so a half-frame average is mostly the far tint whichever half it is.
    let (fx, fy) = dim_pair[0].screen_center();
    let (bx, by) = dim_pair[1].screen_center();
    let front = mean_box(&shown, fx, fy, 5);
    let back = mean_box(&shown, bx, by, 5);
    println!("focus view: front {front:?}  back {back:?}");
    assert!(
        front[2] > front[0] + 8.0,
        "the near side must read COOL in the focus view — got {front:?}, which is not blue-led."
    );
    assert!(
        back[0] > back[2] + 8.0,
        "…and the far side WARM — got {back:?}. If the two agree, the sign of the signed \
         circle of confusion is being thrown away before the tint is chosen."
    );

    println!("dof probe OK");
}

/// One emissive card: a distance in front of the camera, and where it sits
/// vertically as a fraction of the half-frame at THAT distance — so two cards at
/// different depths still cover the same pixels and land in different halves.
#[derive(Clone, Copy)]
struct Card {
    z: f32,
    y_frac: f32,
    /// Half-size as a fraction of the distance, so apparent size is constant.
    size: f32,
    /// Emissive strength. Past 1 is light the display cannot hold, which is what
    /// `dof_highlight` is defined against.
    glow: f32,
}

impl Card {
    fn at(z: f32, y_frac: f32) -> Self {
        Self { z, y_frac, size: 0.055, glow: 4.0 }
    }

    /// A near-POINT source, bright enough to stay visible however far the blur
    /// spreads it. The iris test needs this: the blur footprint of a source is
    /// the source convolved with the aperture, so measuring the aperture's SHAPE
    /// means making the source small enough to disappear inside it. Against a
    /// card the size of the blur, a triangular iris reads as a barely-rounder
    /// square, and the test cannot see the feature it is for.
    fn point(z: f32) -> Self {
        Self { z, y_frac: 0.0, size: 0.013, glow: 900.0 }
    }

    /// A DIM card, for the focus view. The tint is mixed into the pixel's own
    /// colour, so a blown-out emitter tests nothing: it saturates to white on
    /// both sides of the focus and the two halves come back identical.
    fn dim(z: f32, y_frac: f32) -> Self {
        Self { z, y_frac, size: 0.055, glow: 0.4 }
    }
    /// Where this card lands on screen, in pixels. Derived from the same
    /// `y_frac` the transform uses, so the measurement follows the geometry
    /// rather than a number typed twice.
    fn screen_center(self) -> (usize, usize) {
        let half = S as f32 * 0.5;
        (half as usize, (half - self.y_frac * half) as usize)
    }

    /// World transform: y placed by fraction of the half-frame, and the card
    /// scaled with depth so its APPARENT size is the same at any distance.
    fn transform(self) -> Mat4 {
        let half_h = self.z * (FOV * 0.5).tan();
        Mat4::from_translation(Vec3::new(0.0, self.y_frac * half_h, -self.z))
            * Mat4::from_scale(Vec3::splat(self.z * self.size))
    }
}

struct Rig {
    raster: Raster,
    post: PostStack,
    mesh: floptle_render::MeshId,
    out: wgpu::Texture,
    out_view: wgpu::TextureView,
    globals: Globals,
    proj: glam::Mat4,
}

impl Rig {
    fn new(gpu: &Gpu) -> Self {
        let mut raster = Raster::new(gpu);
        let mesh = raster.register(gpu, &plane(1.0), None);
        let post = PostStack::new(gpu, S, S);
        let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dof-out"),
            size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.surface_format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
        let cam = RenderCamera::new(
            Vec3::ZERO.as_dvec3(),
            glam::Quat::IDENTITY,
            Projection::Perspective { fov_y: FOV, near: 0.05, far: 200.0 },
        );
        let globals =
            Globals { view_proj: cam.view_proj(1.0).to_cols_array_2d(), ..Default::default() };
        Self { raster, post, mesh, out, out_view, globals, proj: cam.proj_matrix(1.0) }
    }

    fn render(&mut self, gpu: &Gpu, cards: &[Card], s: &PostSettings) -> Vec<[u8; 3]> {
        // Unlit + 4× emissive on black: a light source rather than a lit surface,
        // so the blur has something past white to work on and nothing else in
        // the frame to confuse the measurements.
        let draws: Vec<_> = cards
            .iter()
            .map(|c| {
                let mut m = MaterialParams::flat([0.0, 0.0, 0.0]);
                m.unlit = true;
                m.emissive = [1.0, 1.0, 1.0];
                m.emissive_strength = c.glow;
                (self.mesh, None, instance_of_mat(c.transform(), &m))
            })
            .collect();
        self.raster.draw_scene(
            gpu,
            self.post.input_view(),
            gpu.depth_view(),
            self.globals,
            &draws,
            Some([0.0, 0.0, 0.0, 1.0]),
            None,
        );
        let ssao = SsaoFrame {
            depth: gpu.depth_view(),
            proj: self.proj.to_cols_array_2d(),
            inv_proj: self.proj.inverse().to_cols_array_2d(),
        };
        self.post.run(gpu, s, Some(&ssao), &self.out_view);
        read_rgb(gpu, &self.out)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Half {
    Top,
    Bottom,
}

const LIT: f32 = 18.0;

fn luma(c: [u8; 3]) -> f32 {
    0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32
}

fn in_half(y: usize, h: Half) -> bool {
    match h {
        Half::Top => y < S as usize / 2,
        Half::Bottom => y >= S as usize / 2,
    }
}

/// How many pixels the card's light reaches in one half of the frame. Blur
/// spreads a fixed amount of light over more pixels, so above a LOW threshold
/// this grows with the blur — which is the property being measured.
fn lit(img: &[[u8; 3]], h: Half) -> usize {
    (0..S as usize)
        .flat_map(|y| (0..S as usize).map(move |x| (x, y)))
        .filter(|&(x, y)| in_half(y, h) && luma(img[y * S as usize + x]) > LIT)
        .count()
}

fn mean_lit(img: &[[u8; 3]]) -> f32 {
    let v: Vec<f32> = img.iter().map(|c| luma(*c)).filter(|l| *l > LIT).collect();
    if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 }
}

/// Mean colour over a small box — used where the question is about one surface
/// rather than about a whole half of the frame.
fn mean_box(img: &[[u8; 3]], cx: usize, cy: usize, r: usize) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    let mut n = 0.0;
    for y in cy.saturating_sub(r)..=(cy + r).min(S as usize - 1) {
        for x in cx.saturating_sub(r)..=(cx + r).min(S as usize - 1) {
            let c = img[y * S as usize + x];
            for k in 0..3 {
                sum[k] += c[k] as f32;
            }
            n += 1.0;
        }
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// How round the blur footprint is: the widest reach divided by the narrowest,
/// over twelve directions from its own centre. 1.0 is a circle; a polygon's
/// inradius is `cos(pi/n)` of its circumradius, so three blades is about 2.
fn roundness(img: &[[u8; 3]]) -> f32 {
    let (mut cx, mut cy, mut n) = (0.0f32, 0.0f32, 0.0f32);
    for y in 0..S as usize {
        for x in 0..S as usize {
            if luma(img[y * S as usize + x]) > LIT {
                cx += x as f32;
                cy += y as f32;
                n += 1.0;
            }
        }
    }
    assert!(n > 200.0, "the footprint must actually be lit to measure ({n} px)");
    let (cx, cy) = (cx / n, cy / n);
    let mut radii = Vec::new();
    for i in 0..12 {
        let a = i as f32 * std::f32::consts::TAU / 12.0;
        let (dx, dy) = (a.cos(), a.sin());
        // The radius of the CONTINUOUS run from the centre — stop at the first
        // gap rather than taking the furthest lit pixel anywhere along the ray.
        // The gather is a sampling of the aperture, so its edge is ragged by a
        // pixel or two; "furthest lit" measures that ragged fringe and reports a
        // clean disc as 1.3× out of round.
        let mut r = 0.0f32;
        let mut t = 0.5f32;
        while t < S as f32 {
            let (x, y) = ((cx + dx * t).round() as i32, (cy + dy * t).round() as i32);
            if x < 0 || y < 0 || x >= S as i32 || y >= S as i32 {
                break;
            }
            if luma(img[y as usize * S as usize + x as usize]) <= LIT {
                break;
            }
            r = t;
            t += 0.5;
        }
        radii.push(r.max(0.5));
    }
    let hi = radii.iter().cloned().fold(0.0f32, f32::max);
    let lo = radii.iter().cloned().fold(f32::MAX, f32::min);
    hi / lo
}

fn read_rgb(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 3]> {
    let bpp = 4u32;
    let padded =
        (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dof-readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dof-readback") });
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
        for x in 0..S as usize {
            let i = y * padded as usize + x * bpp as usize;
            out.push(if bgra {
                [view[i + 2], view[i + 1], view[i]]
            } else {
                [view[i], view[i + 1], view[i + 2]]
            });
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn write_png(path: &str, rgb: &[[u8; 3]]) {
    let flat: Vec<u8> = rgb.iter().flat_map(|c| c.iter().copied()).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("png header").write_image_data(&flat).expect("png data");
}
