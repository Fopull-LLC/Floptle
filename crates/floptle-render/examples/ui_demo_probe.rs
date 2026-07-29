//! Render `assets/scenes/ui_demo.ron` headlessly, so the demo can be LOOKED at.
//!
//! It loads the real scene file, the real `.tokens.ron` and the real
//! `.uistyle.ron`, and pushes them through the shipping path
//! (`apply_styles` → `solve` → `draw_list_with` → `Ui::pack` → `Ui::draw`).
//! Rebuilding the same screen in Rust would prove the renderer works and
//! nothing about whether the demo does.
//!
//! It also renders the interaction states, because the whole argument of the
//! style system is that they cost nothing to author and everything to leave
//! out — and a screenshot of the base state proves none of it.
//!
//! Run: cargo run --release -p floptle-render --example ui_demo_probe
//!      cargo run --release -p floptle-render --example ui_demo_probe -- 2560 1080

use std::collections::HashMap;

use floptle_render::{Gpu, Raster, Ui};
use floptle_ui::{ElementSpec, Node, StateInput, StyleRuntime, TextSpec, UiLayer};

/// The demo's repeater is a Play-time thing, so the probe fills the list the
/// way the running game would — otherwise the most interesting panel is empty
/// in the one image anybody looks at.
const MANIFEST: [&str; 10] = [
    "Iron ore",
    "Refined fuel",
    "Hull plating",
    "Nav computer",
    "Ration crate",
    "Coolant loop",
    "Spare thruster",
    "Survey drone",
    "Ice core sample",
    "Distress beacon",
];

fn root() -> std::path::PathBuf {
    // The examples run from the workspace root under `cargo run`.
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("assets")
}

/// Build the layer's element tree out of the scene doc, mirroring what the
/// editor's `ui_layer_tree` does: children in scene order, `order` sorted.
fn build(docs: &[floptle_scene::NodeDoc], parent: Option<usize>) -> Vec<Node> {
    let mut out = Vec::new();
    for (i, d) in docs.iter().enumerate() {
        if d.parent != parent {
            continue;
        }
        let Some(spec) = d.ui.clone() else { continue };
        out.push(Node::with_children(i as u32, spec, build(docs, Some(i))));
    }
    floptle_ui::sort_roots(&mut out);
    out
}

/// Splice in the rows the repeater would have spawned.
fn fill_repeaters(nodes: &mut [Node], row: &ElementSpec) {
    for n in nodes.iter_mut() {
        if let Some(r) = n.spec.repeater.clone() {
            let count = if r.count > 0 { r.count as usize } else { MANIFEST.len() };
            for i in 0..count {
                let mut spec = row.clone();
                if let Some(t) = &mut spec.text {
                    t.text = format!("  {:02}   {}", i + 1, MANIFEST[i % MANIFEST.len()]);
                }
                // The engine's own `group` behaviour: row 3 is the chosen one,
                // so the `selected` block gets into the picture.
                spec.selected = i == 2;
                // Ids past the scene's node count — nothing else claims them.
                n.children.push(Node::with_children(10_000 + i as u32, spec, vec![]));
            }
        }
        fill_repeaters(&mut n.children, row);
    }
}

/// Every element id under `nodes` whose scene name matches.
fn id_named(docs: &[floptle_scene::NodeDoc], name: &str) -> Option<u32> {
    docs.iter().position(|d| d.name == name).map(|i| i as u32)
}

struct Shot {
    px: Vec<[u8; 4]>,
    w: u32,
    h: u32,
}

impl Shot {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.px[(y.min(self.h - 1) * self.w + x.min(self.w - 1)) as usize]
    }
    fn peak(&self, r: [u32; 4]) -> f32 {
        let mut best = 0.0f32;
        for y in r[1]..(r[1] + r[3]).min(self.h) {
            for x in r[0]..(r[0] + r[2]).min(self.w) {
                let p = self.at(x, y);
                best = best.max((p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0);
            }
        }
        best
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
    let assets = root();
    let scene_text =
        std::fs::read_to_string(assets.join("scenes/ui_demo.ron")).expect("ui_demo.ron");
    let doc: floptle_scene::SceneDoc = ron::from_str(&scene_text).expect("parse ui_demo.ron");
    let tokens = floptle_ui::Tokens::parse(
        &std::fs::read_to_string(assets.join("ui/demo.tokens.ron")).expect("demo.tokens.ron"),
    )
    .expect("parse tokens");
    let sheet = floptle_ui::StyleSheet::parse(
        &std::fs::read_to_string(assets.join("ui/demo.uistyle.ron")).expect("demo.uistyle.ron"),
    )
    .expect("parse styles");
    println!("{} nodes, {} tokens, {} styles", doc.nodes.len(), tokens.colors.len(), sheet.styles.len());

    let row_docs: Vec<floptle_scene::NodeDoc> = ron::from_str(
        &std::fs::read_to_string(assets.join("prefabs/DemoRow.prefab.ron")).expect("DemoRow"),
    )
    .expect("parse DemoRow");
    let row_spec = row_docs[0].ui.clone().expect("the row prefab is a UI element");

    let layer_at = doc.nodes.iter().position(|n| n.ui_layer.is_some()).expect("a UI layer");
    let layer: UiLayer = doc.nodes[layer_at].ui_layer.expect("layer");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (vw, vh): (f32, f32) = match args.as_slice() {
        [w, h] => (w.parse().unwrap_or(1280.0), h.parse().unwrap_or(720.0)),
        _ => (1280.0, 720.0),
    };

    let gpu = Gpu::headless(64, 64);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);

    // Each shot forces a different state, because the argument for the style
    // system is precisely the states — a base-state screenshot proves none of
    // it, and a state that was never rendered is a state nobody checked.
    let play = id_named(&doc.nodes, "Play");
    let options = id_named(&doc.nodes, "Options");
    let field = id_named(&doc.nodes, "Call Sign");
    let shots: [(&str, StateInput, bool); 3] = [
        // The hero shot lands in docs/, because that is the one the manual
        // shows and a stale screenshot of a UI system is its own bad advert.
        ("docs/ui-demo.png", StateInput { hovered: options, pressed: None, focused: play }, false),
        (
            "ui_demo_states.png",
            StateInput { hovered: play, pressed: options, focused: field },
            true,
        ),
        ("ui_demo_bare.png", StateInput::default(), false),
    ];

    let mut checked = false;
    for (out, input, editing) in shots {
        let mut roots = build(&doc.nodes, Some(layer_at));
        fill_repeaters(&mut roots, &row_spec);
        // The demo's field is empty in the file; give the "editing" shot
        // something to put a caret in the middle of.
        if editing && let Some(id) = field {
            set_text(&mut roots, id, "ARGO-7");
        }
        let mut rt = StyleRuntime::default();
        floptle_ui::apply_styles(&mut roots, &sheet, &tokens, &input, &mut rt, 10.0);

        let scale = layer.scale_for([vw, vh]);
        let design_vp = [vw / scale, vh / scale];
        let measure = |t: &TextSpec| ui.measure_spec(t);
        let mut placed = floptle_ui::solve(&roots, design_vp, &measure);
        floptle_ui::place_scrollbars(&roots, &mut placed, &scrollbars(&doc.nodes, &roots));
        let edit = (editing && field.is_some()).then(|| floptle_ui::EditState {
            id: field.unwrap(),
            caret: 4,
            anchor: 6,
            on: true,
        });
        let dl = floptle_ui::draw_list_with(&roots, &placed, &[], edit);

        let (w, h) = (vw as u32, vh as u32);
        let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui-demo"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut enc = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
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
        let (mut instances, mut batches) = (Vec::new(), Vec::new());
        ui.clear_backdrop();
        ui.pack(
            &gpu,
            &dl,
            [0.0, 0.0],
            scale,
            &mut |_| None,
            &|_| None,
            &mut |_, _| None,
            &mut instances,
            &mut batches,
        );
        ui.draw(&gpu, &view, [w as f32, h as f32], &instances, &batches, &raster);
        let px = readback(&gpu, &tex, w, h);
        save_png(&px, w, h, out);
        println!("  {out}: {} quads, {} texts", dl.quads.len(), dl.texts.len());

        // Checks on the FIRST shot only; the others are for the eye.
        if !checked {
            checked = true;
            let shot = Shot { px, w, h };
            // The scroll view clips its overflowing list. Ten rows at 34+6
            // units do not fit in 216, so the panel's bottom edge has to be
            // the end of the list — a clip that silently failed would show
            // rows running down over the panel below.
            let below = shot.peak([
                (560.0 * scale) as u32,
                (412.0 * scale) as u32,
                (300.0 * scale) as u32,
                (34.0 * scale) as u32,
            ]);
            println!("  between the panels: peak {below:.0}");
            assert!(below < 60.0, "the manifest list is not clipping ({below:.0})");
            // The focused element got the style's `focus` block, which here is
            // a glow — so it is BRIGHTER than the same button unfocused. This
            // is the assertion that catches "focus resolves but nothing sets
            // it", which is exactly how phase B shipped.
            assert!(
                shot.peak([
                    (60.0 * scale) as u32,
                    (300.0 * scale) as u32,
                    (440.0 * scale) as u32,
                    (70.0 * scale) as u32,
                ]) > 150.0,
                "the focused button should be lit by its style's focus block"
            );
        }
    }
    println!("\nOK — look at docs/ui-demo.png, ui_demo_states.png, ui_demo_bare.png");
}

/// Resolve the scene's `scrollbar` targets by name, as the editor does.
fn scrollbars(docs: &[floptle_scene::NodeDoc], roots: &[Node]) -> Vec<(u32, u32)> {
    fn walk(n: &Node, out: &mut Vec<u32>) {
        out.push(n.id);
        for c in &n.children {
            walk(c, out);
        }
    }
    let mut ids = Vec::new();
    for r in roots {
        walk(r, &mut ids);
    }
    let by_name: HashMap<&str, u32> =
        docs.iter().enumerate().map(|(i, d)| (d.name.as_str(), i as u32)).collect();
    let mut out = Vec::new();
    for id in ids {
        let Some(d) = docs.get(id as usize) else { continue };
        if let Some(spec) = &d.ui
            && let Some(sb) = &spec.scrollbar
            && let Some(&t) = by_name.get(sb.target.as_str())
        {
            out.push((id, t));
        }
    }
    out
}

fn set_text(nodes: &mut [Node], id: u32, s: &str) {
    for n in nodes.iter_mut() {
        if n.id == id && let Some(t) = &mut n.spec.text {
            t.text = s.to_string();
        }
        set_text(&mut n.children, id, s);
    }
}
