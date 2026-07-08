//! Game-UI editor integration (docs/ui-system-proposal.md, phase 1).
//!
//! - `gather_game_ui`: walk the scene for `UiLayer` nodes, build each layer's
//!   element tree, solve layout (CPU — cheap, readable), emit draw lists, and
//!   pre-register any image textures. Runs before the GPU borrows.
//! - `ui_inspector`: the Inspector section for UI layers/elements — plain
//!   properties, no imposed look (shape/image/text toggles map to the spec's
//!   Options).
//! - `add_ui_node`: the Add ⏵ UI menu's spawn (Empty node + UI components —
//!   the modular-components model, no new Matter variants).

use std::collections::HashMap;

use floptle_core::{Entity, Parent, Transform};
use floptle_scene::MatterDoc;
use floptle_ui::{
    Align, Anchor, Dir, ElementSpec, ImageSpec, Justify, Place, ShapeSpec, Size, StackCfg,
    TextSpec, UiLayer,
};

use crate::Editor;



/// What Add ⏵ UI creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddUi {
    Layer,
    Panel,
    Text,
    Image,
}

impl Editor {
    /// Solve every UI layer for this frame: (draw list, px-per-design-unit),
    /// z-sorted. Pre-resolves image textures into the registry (needs
    /// `&mut self`, so this runs BEFORE the draw core's field borrows).
    pub(crate) fn gather_game_ui(&mut self, viewport: [f32; 2]) -> Vec<(floptle_ui::DrawList, f32)> {
        if viewport[0] <= 1.0 || viewport[1] <= 1.0 {
            return Vec::new();
        }
        // Scene order + children map (node order = deterministic draw order).
        let order: Vec<Entity> = self.world.query::<Transform>().map(|(e, _)| e).collect();
        let mut kids: HashMap<u32, Vec<Entity>> = HashMap::new();
        for e in &order {
            if let Some(p) = self.world.get::<Parent>(*e) {
                kids.entry(p.0.index()).or_default().push(*e);
            }
        }
        fn build(
            world: &floptle_core::World,
            kids: &HashMap<u32, Vec<Entity>>,
            e: Entity,
        ) -> Option<floptle_ui::Node> {
            let spec = world.get::<ElementSpec>(e)?.clone();
            let children = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(world, kids, *c)).collect())
                .unwrap_or_default();
            Some(floptle_ui::Node { id: e.index(), spec, children })
        }
        let mut layers: Vec<(i32, Vec<floptle_ui::Node>, f32)> = Vec::new();
        for e in &order {
            let Some(layer) = self.world.get::<UiLayer>(*e).copied() else { continue };
            if !layer.enabled {
                continue;
            }
            let scale = (viewport[1] / layer.design_height.max(1.0)).max(0.01);
            let roots: Vec<_> = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(&self.world, &kids, *c)).collect())
                .unwrap_or_default();
            if !roots.is_empty() {
                layers.push((layer.z, roots, scale));
            }
        }
        if layers.is_empty() {
            return Vec::new();
        }
        layers.sort_by_key(|(z, ..)| *z);
        let Some(uir) = self.ui_render.as_ref() else { return Vec::new() };
        let mut out = Vec::new();
        let mut textures: Vec<String> = Vec::new();
        for (_, roots, scale) in &layers {
            let design_vp = [viewport[0] / scale, viewport[1] / scale];
            let measure = |t: &TextSpec| uir.measure(&t.text, t.size);
            let placed = floptle_ui::solve(roots, design_vp, &measure);
            let dl = floptle_ui::draw_list(roots, &placed);
            for q in &dl.quads {
                if !q.texture.is_empty() {
                    textures.push(q.texture.clone());
                }
            }
            out.push((dl, *scale));
        }
        for t in textures {
            let _ = self.ensure_texture(&t);
        }
        out
    }

    /// Scene-view authoring: each UI layer as a WORLD CANVAS at its node's
    /// transform — origin = translation (canvas top-left), plane axes from its
    /// rotation, [`UI_WORLD_SCALE`] world units per design unit. Returns per
    /// layer: (draw list, solved rects in design units, origin, right, down).
    /// The layer node itself is arranged with the normal move/rotate gizmos.
    #[allow(clippy::type_complexity)]
    pub(crate) fn gather_ui_world(
        &mut self,
        window_aspect: f32,
    ) -> Vec<(floptle_ui::DrawList, Vec<floptle_ui::Placed>, [f64; 3], [f32; 3], [f32; 3], [f32; 2])>
    {
        let order: Vec<Entity> = self.world.query::<Transform>().map(|(e, _)| e).collect();
        let mut kids: HashMap<u32, Vec<Entity>> = HashMap::new();
        for e in &order {
            if let Some(p) = self.world.get::<Parent>(*e) {
                kids.entry(p.0.index()).or_default().push(*e);
            }
        }
        fn build(
            world: &floptle_core::World,
            kids: &HashMap<u32, Vec<Entity>>,
            e: Entity,
        ) -> Option<floptle_ui::Node> {
            let spec = world.get::<ElementSpec>(e)?.clone();
            let children = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(world, kids, *c)).collect())
                .unwrap_or_default();
            Some(floptle_ui::Node { id: e.index(), spec, children })
        }
        let Some(uir) = self.ui_render.as_ref() else { return Vec::new() };
        let mut out = Vec::new();
        let mut textures: Vec<String> = Vec::new();
        for e in &order {
            let Some(layer) = self.world.get::<UiLayer>(*e).copied() else { continue };
            if !layer.enabled {
                continue;
            }
            let roots: Vec<_> = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(&self.world, &kids, *c)).collect())
                .unwrap_or_default();
            if roots.is_empty() {
                continue;
            }
            let design_vp =
                [layer.design_height * window_aspect.max(0.1), layer.design_height];
            let measure = |t: &TextSpec| uir.measure(&t.text, t.size);
            let placed = floptle_ui::solve(&roots, design_vp, &measure);
            let dl = floptle_ui::draw_list(&roots, &placed);
            for q in &dl.quads {
                if !q.texture.is_empty() {
                    textures.push(q.texture.clone());
                }
            }
            let wt = floptle_core::world_transform(&self.world, *e);
            let ws = layer.canvas_scale.max(0.0001);
            let right = wt.rotation * floptle_core::math::Vec3::X * ws;
            let down = wt.rotation * (-floptle_core::math::Vec3::Y) * ws;
            out.push((
                dl,
                placed,
                [wt.translation.x, wt.translation.y, wt.translation.z],
                [right.x, right.y, right.z],
                [down.x, down.y, down.z],
                design_vp,
            ));
        }
        for t in textures {
            let _ = self.ensure_texture(&t);
        }
        out
    }

    /// Add ⏵ UI: an Empty node carrying the UI components. Elements land
    /// under the selected node (so building a screen is: add a Layer, keep
    /// adding elements inside it).
    pub(crate) fn add_ui_node(&mut self, what: AddUi) {
        let (name, layer, spec): (&str, Option<UiLayer>, Option<ElementSpec>) = match what {
            AddUi::Layer => ("UI Layer", Some(UiLayer::default()), None),
            AddUi::Panel => (
                "Panel",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    size: [Size::Fixed(240.0), Size::Fixed(140.0)],
                    shape: Some(ShapeSpec { fill: [0.12, 0.12, 0.14, 0.85], ..Default::default() }),
                    ..Default::default()
                }),
            ),
            AddUi::Text => (
                "Text",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    text: Some(TextSpec { text: "Text".into(), ..Default::default() }),
                    ..Default::default()
                }),
            ),
            AddUi::Image => (
                "Image",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    size: [Size::Fixed(128.0), Size::Fixed(128.0)],
                    image: Some(ImageSpec::default()),
                    ..Default::default()
                }),
            ),
        };
        self.add_node(name, MatterDoc::Empty);
        // add_node selects what it created — attach the components there.
        let Some(&e) = self.selection.first() else { return };
        if let Some(l) = layer {
            self.world.insert(e, l);
        }
        if let Some(s) = spec {
            self.world.insert(e, s);
        }
    }

    /// The Inspector's UI section: shown for nodes carrying UiLayer and/or
    /// ElementSpec. Returns true when something changed (undo coalescing).
    pub(crate) fn ui_inspector(
        world: &mut floptle_core::World,
        e: Entity,
        ui: &mut egui::Ui,
        tex_list: &[String],
    ) -> bool {
        let mut changed = false;
        if let Some(mut layer) = world.get::<UiLayer>(e).copied() {
            ui.separator();
            ui.label("🖼 UI Layer");
            ui.small("screen-space canvas — in game it always fills the window");
            ui.horizontal(|ui| {
                changed |= ui
                    .checkbox(&mut layer.enabled, "enabled")
                    .on_hover_text("master switch: an off layer draws nothing")
                    .changed();
                ui.label("z");
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.z))
                    .on_hover_text("layers draw lowest z first")
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("design height");
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.design_height).range(100.0..=4320.0))
                    .on_hover_text(
                        "resolution independence: this many design units ALWAYS span the                          window height, on any monitor — bigger number = smaller-looking                          UI. Width follows the window's aspect. Element positions/sizes                          are in these units.",
                    )
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("canvas size");
                changed |= ui
                    .add(
                        egui::Slider::new(&mut layer.canvas_scale, 0.001..=0.1)
                            .logarithmic(true),
                    )
                    .on_hover_text(
                        "Scene-view only: how big the authoring canvas stands in the                          world (world units per design unit). Gameplay rendering is                          unaffected. Move/rotate this node to place the canvas.",
                    )
                    .changed();
            });
            if changed {
                world.insert(e, layer);
            }
        }
        let Some(mut spec) = world.get::<ElementSpec>(e).cloned() else {
            return changed;
        };
        let mut c = false;
        ui.separator();
        ui.label("▭ UI Element");
        // --- placement ---
        let mut pinned = matches!(spec.place, Place::Pin { .. });
        ui.horizontal(|ui| {
            if ui.checkbox(&mut pinned, "pin to edge").on_hover_text("stick to a corner/edge preset instead of a free position").changed() {
                spec.place = if pinned {
                    Place::Pin { anchor: Anchor::TopLeft, offset: [0.0, 0.0] }
                } else {
                    Place::Free { pos: [40.0, 40.0] }
                };
                c = true;
            }
        });
        match &mut spec.place {
            Place::Free { pos } => {
                ui.horizontal(|ui| {
                    ui.label("pos");
                    c |= ui.add(egui::DragValue::new(&mut pos[0]).speed(1.0)).changed();
                    c |= ui.add(egui::DragValue::new(&mut pos[1]).speed(1.0)).changed();
                });
            }
            Place::Pin { anchor, offset } => {
                ui.horizontal(|ui| {
                    ui.label("anchor");
                    egui::ComboBox::from_id_salt(("ui_anchor", e.index()))
                        .selected_text(format!("{anchor:?}"))
                        .show_ui(ui, |ui| {
                            for a in [
                                Anchor::TopLeft, Anchor::Top, Anchor::TopRight,
                                Anchor::Left, Anchor::Center, Anchor::Right,
                                Anchor::BottomLeft, Anchor::Bottom, Anchor::BottomRight,
                            ] {
                                c |= ui.selectable_value(anchor, a, format!("{a:?}")).changed();
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("offset");
                    c |= ui.add(egui::DragValue::new(&mut offset[0]).speed(1.0)).changed();
                    c |= ui.add(egui::DragValue::new(&mut offset[1]).speed(1.0)).changed();
                });
            }
        }
        // --- size (Fixed/Pct simplified to a number + % toggle; Fit/Grow via menu) ---
        for (axis, label) in [(0usize, "width"), (1usize, "height")] {
            ui.horizontal(|ui| {
                ui.label(label);
                let current = spec.size[axis];
                let kind = match current {
                    Size::Fixed(_) => "px",
                    Size::Pct(_) => "%",
                    Size::Fit => "fit",
                    Size::Grow(_) => "grow",
                };
                egui::ComboBox::from_id_salt(("ui_size", e.index(), axis))
                    .selected_text(kind)
                    .width(56.0)
                    .show_ui(ui, |ui| {
                        for (k, v) in [
                            ("px", Size::Fixed(100.0)),
                            ("%", Size::Pct(0.5)),
                            ("fit", Size::Fit),
                            ("grow", Size::Grow(1.0)),
                        ] {
                            if ui.selectable_label(kind == k, k).clicked()
                                && std::mem::discriminant(&spec.size[axis])
                                    != std::mem::discriminant(&v)
                            {
                                spec.size[axis] = v;
                                c = true;
                            }
                        }
                    });
                match &mut spec.size[axis] {
                    Size::Fixed(v) => c |= ui.add(egui::DragValue::new(v).speed(1.0)).changed(),
                    Size::Pct(v) => {
                        c |= ui.add(egui::DragValue::new(v).speed(0.01).range(0.0..=1.0)).changed()
                    }
                    Size::Grow(v) => c |= ui.add(egui::DragValue::new(v).speed(0.1)).changed(),
                    Size::Fit => {}
                }
            });
        }
        ui.horizontal(|ui| {
            c |= ui.checkbox(&mut spec.visible, "visible").changed();
            ui.label("opacity");
            c |= ui.add(egui::Slider::new(&mut spec.opacity, 0.0..=1.0)).changed();
        });
        // --- stack (opt-in flow) ---
        let mut has_stack = spec.stack.is_some();
        if ui
            .checkbox(&mut has_stack, "stack children")
            .on_hover_text("opt-in auto-layout: children flow in a row/column with gap + padding")
            .changed()
        {
            spec.stack = has_stack.then(StackCfg::default);
            c = true;
        }
        if let Some(s) = &mut spec.stack {
            ui.horizontal(|ui| {
                c |= ui.selectable_value(&mut s.dir, Dir::Row, "row").changed();
                c |= ui.selectable_value(&mut s.dir, Dir::Column, "column").changed();
                ui.label("gap");
                c |= ui.add(egui::DragValue::new(&mut s.gap).speed(0.5)).changed();
                ui.label("pad");
                c |= ui.add(egui::DragValue::new(&mut s.pad).speed(0.5)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("align");
                for (v, l) in [(Align::Start, "start"), (Align::Center, "center"), (Align::End, "end"), (Align::Stretch, "stretch")] {
                    c |= ui.selectable_value(&mut s.align, v, l).changed();
                }
            });
            ui.horizontal(|ui| {
                ui.label("justify");
                for (v, l) in [(Justify::Start, "start"), (Justify::Center, "center"), (Justify::End, "end"), (Justify::SpaceBetween, "between")] {
                    c |= ui.selectable_value(&mut s.justify, v, l).changed();
                }
            });
        }
        // --- shape ---
        let mut has = spec.shape.is_some();
        if ui.checkbox(&mut has, "shape").changed() {
            spec.shape = has.then(ShapeSpec::default);
            c = true;
        }
        if let Some(s) = &mut spec.shape {
            ui.horizontal(|ui| {
                ui.label("fill");
                c |= ui.color_edit_button_rgba_unmultiplied(&mut s.fill).changed();
                ui.label("radius");
                c |= ui.add(egui::DragValue::new(&mut s.radius).speed(0.5).range(0.0..=512.0)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("border");
                c |= ui.add(egui::DragValue::new(&mut s.border).speed(0.5).range(0.0..=64.0)).changed();
                c |= ui.color_edit_button_rgba_unmultiplied(&mut s.border_color).changed();
            });
        }
        // --- text ---
        let mut has = spec.text.is_some();
        if ui.checkbox(&mut has, "text").changed() {
            spec.text = has.then(TextSpec::default);
            c = true;
        }
        if let Some(t) = &mut spec.text {
            c |= ui.text_edit_singleline(&mut t.text).changed();
            ui.horizontal(|ui| {
                ui.label("size");
                c |= ui.add(egui::DragValue::new(&mut t.size).speed(0.5).range(4.0..=256.0)).changed();
                c |= ui.color_edit_button_rgba_unmultiplied(&mut t.color).changed();
                for (v, l) in [(Align::Start, "left"), (Align::Center, "center"), (Align::End, "right")] {
                    c |= ui.selectable_value(&mut t.align, v, l).changed();
                }
            });
        }
        // --- image ---
        let mut has = spec.image.is_some();
        if ui.checkbox(&mut has, "image").on_hover_text("any texture from your assets — the engine ships no UI art").changed() {
            spec.image = has.then(ImageSpec::default);
            c = true;
        }
        if let Some(img) = &mut spec.image {
            ui.horizontal(|ui| {
                ui.label("texture");
                let current = if img.texture.is_empty() {
                    "(none)".to_string()
                } else {
                    img.texture.rsplit('/').next().unwrap_or(&img.texture).to_string()
                };
                if let Some(pick) = crate::ui_widgets::searchable_picker(
                    ui,
                    egui::Id::new(("ui_tex_pick", e.index())),
                    &current,
                    Some("(none)"),
                    tex_list,
                    170.0,
                ) {
                    img.texture = pick.unwrap_or_default();
                    c = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("tint");
                c |= ui.color_edit_button_rgba_unmultiplied(&mut img.tint).changed();
            });
        }
        if c {
            world.insert(e, spec);
        }
        changed || c
    }
}
