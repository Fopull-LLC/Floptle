//! Headless probe for text fields (docs/ui-system-2-proposal.md §D).
//!
//! A caret is the one UI element where "the assertions pass" is worth very
//! little on its own: it is two pixels wide, its position comes from a prefix
//! measurement that has to agree exactly with the glyph pass, and every way of
//! getting it slightly wrong still draws a plausible vertical bar somewhere in
//! the box. So this renders the cases and the numbers are checked against the
//! pixels — a caret drawn at the right x for the wrong reason (say, the
//! alignment offset counted twice in centred text) shows up here as a bar in
//! the middle of a word.
//!
//! Cases, left to right down the image:
//!   1. an empty field showing its PLACEHOLDER (dimmer than real text)
//!   2. a value with the caret at the end
//!   3. a caret in the MIDDLE, with a selection band behind three characters
//!   4. a CENTRED field — the alignment trap
//!   5. a MASKED field (dots, and the caret after the last one)
//!   6. a value LONGER than its box: the run scrolls so the caret stays visible
//!
//! Run: cargo run --release -p floptle-render --example ui_field_probe

use floptle_render::{Gpu, Raster, Ui};
use floptle_ui::{
    Align, Corners, EditState, ElementSpec, FieldSpec, Node, Place, ShapeSpec, Size, TextSpec,
};

const W: u32 = 900;
const H: u32 = 560;
/// Every field is this wide/tall and starts here, one under the other.
const X: f32 = 40.0;
const FW: f32 = 320.0;
const FH: f32 = 56.0;
const GAP: f32 = 26.0;

fn field(id: u32, row: u32, value: &str, align: Align, f: FieldSpec) -> Node {
    Node::with_children(
        id,
        ElementSpec {
            place: Place::Free { pos: [X, 30.0 + row as f32 * (FH + GAP)] },
            size: [Size::Fixed(FW), Size::Fixed(FH)],
            shape: Some(ShapeSpec {
                fill: [0.10, 0.11, 0.14, 1.0],
                radius: Corners::all(6.0),
                border: 2.0.into(),
                border_color: [0.30, 0.34, 0.42, 1.0],
                ..Default::default()
            }),
            text: Some(TextSpec {
                text: value.into(),
                size: 26.0,
                color: [0.92, 0.94, 0.98, 1.0],
                align,
                valign: Align::Center,
                ..Default::default()
            }),
            field: Some(f),
            ..Default::default()
        },
        vec![],
    )
}

/// Row `row`'s rect, so an assertion can talk in design units.
fn row_rect(row: u32) -> [f32; 4] {
    [X, 30.0 + row as f32 * (FH + GAP), FW, FH]
}

fn screen() -> Vec<Node> {
    vec![
        field(1, 0, "", Align::Start, FieldSpec { placeholder: "Lobby code".into(), ..Default::default() }),
        field(2, 1, "HELLO", Align::Start, FieldSpec::default()),
        field(3, 2, "selection", Align::Start, FieldSpec::default()),
        field(4, 3, "centred", Align::Center, FieldSpec::default()),
        field(5, 4, "hunter2", Align::Start, FieldSpec { mask: true, ..Default::default() }),
        field(
            6,
            5,
            "a value considerably longer than its own box",
            Align::Start,
            FieldSpec::default(),
        ),
    ]
}

struct Shot {
    px: Vec<[u8; 4]>,
    w: u32,
    h: u32,
    scale: f32,
}

impl Shot {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.px[(y.min(self.h - 1) * self.w + x.min(self.w - 1)) as usize]
    }
    /// Brightest pixel in a design-space column band, and where it was — how a
    /// caret is found without knowing its exact width.
    fn column_light(&self, x0: f32, x1: f32, y0: f32, y1: f32) -> (u32, f32) {
        let (mut best_x, mut best) = (0u32, -1.0f32);
        for x in (x0 * self.scale) as u32..=(x1 * self.scale) as u32 {
            let mut col = 0.0f32;
            for y in (y0 * self.scale) as u32..=(y1 * self.scale) as u32 {
                let p = self.at(x, y);
                col += (p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0;
            }
            if col > best {
                best = col;
                best_x = x;
            }
        }
        (best_x, best)
    }
    /// Brightest pixel in a design-space box.
    fn peak(&self, r: [f32; 4]) -> f32 {
        let (x0, y0) = ((r[0] * self.scale) as u32, (r[1] * self.scale) as u32);
        let (x1, y1) = (((r[0] + r[2]) * self.scale) as u32, ((r[1] + r[3]) * self.scale) as u32);
        let mut best = 0.0f32;
        for y in y0..y1 {
            for x in x0..x1 {
                let p = self.at(x, y);
                best = best.max((p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0);
            }
        }
        best
    }

    /// Mean luminance over a design-space box.
    fn mean(&self, r: [f32; 4]) -> f32 {
        let (x0, y0) = ((r[0] * self.scale) as u32, (r[1] * self.scale) as u32);
        let (x1, y1) = (((r[0] + r[2]) * self.scale) as u32, ((r[1] + r[3]) * self.scale) as u32);
        let mut sum = 0.0;
        let mut n = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                let p = self.at(x, y);
                sum += (p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0;
                n += 1.0;
            }
        }
        if n > 0.0 { sum / n } else { 0.0 }
    }
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture, w: u32, h: u32) -> Vec<[u8; 4]> {
    let row = (w * 4).next_multiple_of(256);
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (row * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
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
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let data = buf.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let i = (y * row + x * 4) as usize;
            out.push([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        }
    }
    drop(data);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], w: u32, h: u32, path: &str) {
    let mut flat = Vec::with_capacity(px.len() * 4);
    for p in px {
        flat.extend_from_slice(p);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
    println!("wrote {path}");
}

fn main() {
    let gpu = Gpu::headless(64, 64);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);

    let roots = screen();
    let design_vp = [W as f32, H as f32];
    let measure = |t: &TextSpec| ui.measure_spec(t);
    let placed = floptle_ui::solve(&roots, design_vp, &measure);

    // One render per case, because only ONE element can be edited at a time —
    // which is itself the rule worth proving. The shots are composited into a
    // single image so the whole set can be looked at in one go.
    let cases: [(&str, Option<EditState>); 6] = [
        ("placeholder", None),
        ("caret at end", Some(EditState { id: 2, caret: 5, anchor: 5, on: true })),
        // Three characters selected, caret at the left end of the band.
        ("selection", Some(EditState { id: 3, caret: 3, anchor: 6, on: true })),
        ("centred", Some(EditState { id: 4, caret: 3, anchor: 3, on: true })),
        ("masked", Some(EditState { id: 5, caret: 7, anchor: 7, on: true })),
        ("scrolled", Some(EditState { id: 6, caret: 43, anchor: 43, on: true })),
    ];

    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui-field-probe"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    {
        let mut enc =
            gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.015,
                        g: 0.017,
                        b: 0.025,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.queue.submit(Some(enc.finish()));
    }
    // Each case contributes only its own row's draws; drawing all six lists
    // over one target is the same thing as one list per row, and keeps this to
    // a single readback.
    for (name, edit) in cases {
        let dl = floptle_ui::draw_list_with(&roots, &placed, &[], edit);
        let keep: Vec<u32> = edit.map(|e| vec![e.id]).unwrap_or_else(|| vec![1]);
        let mut instances = Vec::new();
        let mut batches = Vec::new();
        // Only the row this case is about, so the six renders don't stack six
        // copies of the same six boxes.
        let mine = floptle_ui::DrawList {
            text_snap: 0.0,
            quads: dl
                .quads
                .iter()
                .filter(|q| keep.iter().any(|id| in_row(q.rect, *id)))
                .cloned()
                .collect(),
            texts: dl
                .texts
                .iter()
                .filter(|t| keep.iter().any(|id| in_row(t.rect, *id)))
                .cloned()
                .collect(),
        };
        ui.clear_backdrop();
        ui.pack(
            &gpu,
            &mine,
            [0.0, 0.0],
            1.0,
            &mut |_| None,
            &|_| None,
            &mut |_, _| None,
            &mut instances,
            &mut batches,
        );
        ui.draw(&gpu, &view, [W as f32, H as f32], &instances, &batches, &raster);
        println!("  {name}: {} quads, {} texts", mine.quads.len(), mine.texts.len());
    }

    let px = readback(&gpu, &tex, W, H);
    save_png(&px, W, H, "ui_field_probe.png");
    let shot = Shot { px, w: W, h: H, scale: 1.0 };

    // ---- what the pixels have to say --------------------------------------
    // 1. The placeholder is dimmer than a real value. PEAK brightness, not the
    //    mean: the box is mostly fill either way, so an average would be
    //    swamped by it and would pass whatever the text did.
    let hint = shot.peak(inset(row_rect(0)));
    let real = shot.peak(inset(row_rect(1)));
    println!("placeholder peak {hint:.1} vs value peak {real:.1}");
    assert!(
        hint < real * 0.75,
        "the placeholder should read as a hint, not as content ({hint:.1} vs {real:.1})"
    );

    // 2. The caret at the end of "HELLO" sits AFTER the last glyph, not at the
    //    box edge and not inside the word.
    let r = row_rect(1);
    let (caret_x, _) = shot.column_light(r[0] + 4.0, r[0] + FW - 4.0, r[1] + 14.0, r[1] + 42.0);
    let text_w = ui.measure("HELLO", 26.0)[0];
    println!("caret at x={caret_x}, text ends at {:.1}", r[0] + text_w);
    assert!(
        (caret_x as f32 - (r[0] + text_w)).abs() < 6.0,
        "the caret should land just past the last glyph (x={caret_x}, expected ~{:.1})",
        r[0] + text_w
    );

    // 3. The selection band lights up the middle of the word and NOT its ends,
    //    which is the assertion that catches an off-by-a-character band.
    let r = row_rect(2);
    let pre = ui.measure("sel", 26.0)[0];
    let mid = ui.measure("selection", 26.0)[0];
    let banded = shot.mean([r[0] + pre + 2.0, r[1] + 16.0, ui.measure("ect", 26.0)[0] - 4.0, 24.0]);
    let outside = shot.mean([r[0] + mid + 6.0, r[1] + 16.0, 40.0, 24.0]);
    println!("inside the band {banded:.1}, past the word {outside:.1}");
    assert!(
        banded > outside + 8.0,
        "the selection band should brighten exactly the selected characters"
    );

    // 4. Centred text puts its caret inside the centred run, which is where
    //    the align-counted-twice bug shows: it would land near the left edge.
    let r = row_rect(3);
    let run = ui.measure("centred", 26.0)[0];
    let left = r[0] + (FW - run) * 0.5;
    let want = left + ui.measure("cen", 26.0)[0];
    let (cx, _) = shot.column_light(r[0] + 4.0, r[0] + FW - 4.0, r[1] + 14.0, r[1] + 42.0);
    println!("centred caret at x={cx}, expected ~{want:.1} (run starts {left:.1})");
    assert!(
        (cx as f32 - want).abs() < 8.0,
        "a centred field's caret must be measured from the centred run, not the box"
    );

    // 5. A masked field draws neither the value nor a blank: the dots are wider
    //    than nothing and the letters are gone.
    let r = row_rect(4);
    let masked_w = ui.measure("•••••••", 26.0)[0];
    let (mx, _) = shot.column_light(r[0] + 4.0, r[0] + FW - 4.0, r[1] + 14.0, r[1] + 42.0);
    println!("masked caret at x={mx}, dots end at {:.1}", r[0] + masked_w);
    assert!(
        (mx as f32 - (r[0] + masked_w)).abs() < 8.0,
        "a masked caret follows the DOTS, since that is what is on screen"
    );

    // 6. The over-long value scrolls: the caret stays inside the box (this is
    //    the whole point) and the text is clipped at the box rather than
    //    running on across the screen.
    let r = row_rect(5);
    let (sx, _) = shot.column_light(r[0] + 2.0, r[0] + FW - 2.0, r[1] + 14.0, r[1] + 42.0);
    println!("scrolled caret at x={sx}, box ends at {:.1}", r[0] + FW);
    assert!(
        (sx as f32) < r[0] + FW && (sx as f32) > r[0] + FW - 24.0,
        "the caret of an over-long value must sit just inside the right edge"
    );
    // …and the overflow is CLIPPED rather than running on across the screen:
    // just outside the box reads as untouched backdrop, sampled from a corner
    // no element goes near rather than assumed.
    let past = shot.peak([r[0] + FW + 6.0, r[1], 120.0, FH]);
    let backdrop = shot.peak([W as f32 - 60.0, 8.0, 40.0, 40.0]);
    println!("just past the box {past:.1}, bare backdrop {backdrop:.1}");
    assert!(
        past <= backdrop + 2.0,
        "a field clips to its own rect ({past:.1} vs backdrop {backdrop:.1})"
    );

    println!("\nOK — look at ui_field_probe.png");
}

/// Whether a rect belongs to row `id`'s field (they are laid out one per row,
/// so the y band is enough to tell them apart).
fn in_row(rect: [f32; 4], id: u32) -> bool {
    let r = row_rect(id - 1);
    rect[1] >= r[1] - 1.0 && rect[1] <= r[1] + r[3] + 1.0
}

/// The inside of a field, clear of its border — where the text lives.
fn inset(r: [f32; 4]) -> [f32; 4] {
    [r[0] + 4.0, r[1] + 12.0, r[2] - 8.0, r[3] - 24.0]
}
