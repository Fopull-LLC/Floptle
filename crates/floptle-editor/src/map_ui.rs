//! The ▦ Model tab: draw/spawn blockout shapes, the vertex/edge/face sub-mode,
//! modeling ops on the current sub-object selection, and the material-slot
//! list (assign faces to slots; override each slot's material per node).
//!
//! Laid out as titled sections in the order you work in — DRAW, SELECT,
//! TRANSFORM, MODIFY, SHAPE, SIZE, FACE MATERIALS — with one visual language
//! throughout: a rule under each section title, equal-width chips for anything
//! that picks a mode, equal-width buttons for anything that acts, and a left
//! label column so controls line up down the panel. Rarely-touched knobs live
//! in collapsed disclosures so the common path stays short.
//!
//! Like every tab, this takes its state as explicit borrows ([`MapCtx`]) and
//! records intents on `EditorCmd` — geometry ops need `&mut Editor` (undo
//! snapshots + the store), so they apply after the frame.

use crate::inspector;
use crate::gizmo::Tool;
use crate::map_edit::{MapOp, MapOrient, MapShape, MapSubMode, MapXform};
use crate::map_keys::{MapCmd, reserved, save_map_keys};
use crate::{map_edit, map_keys};
use egui::{Color32, RichText, Vec2};
use floptle_core::math::Vec3;
use floptle_core::{Entity, Matter, World};

/// The measurements the panel is built on, so everything lines up without
/// magic numbers scattered through the code. They live in [`crate::responsive`]
/// now, shared with the ◫ Tiles tab, because the two panels are meant to look
/// like one program and a second copy of `74.0` is how that stops being true.
use crate::responsive::{BTN_H, CHIP_W, Chip, MIN_CONTENT_W, strip};


const ACCENT: Color32 = Color32::from_rgb(255, 200, 80);
const DRAW_ACCENT: Color32 = Color32::from_rgb(120, 220, 255);

/// A titled section rule: `TITLE ─────────────`.
use crate::responsive::section;

/// A labelled row: a caption on the left, controls on the right — until the
/// panel gets too thin for both, at which point the caption moves above them.
///
/// The controls run in a **wrapped** horizontal either way, which is the whole
/// reason this tab survives a narrow dock: a MODIFY row is four action buttons,
/// and four buttons that cannot share a line now take two.
fn row<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    crate::responsive::row(ui, label, MIN_CONTENT_W, add)
}

/// An action button, sized like every other action button.
fn action(ui: &mut egui::Ui, enabled: bool, text: &str, hover: &str) -> bool {
    crate::responsive::action(ui, enabled, text, hover)
}

/// Everything the ▦ Model tab reads or writes, as borrows.
///
/// The tab used to render straight off `EditorTabViewer`, which holds around a
/// hundred disjoint field borrows for the whole editor. That works, and it cost
/// the tab the one thing every other tab has: **it could not be driven from a
/// test.** `TileCtx` and `SettingsCtx` take exactly what they need, so ◫ Tiles
/// and ⚙ Settings can be run headlessly and asserted on; ▦ Model could not, and
/// so it was the one panel whose narrow-dock layout was checked only through the
/// primitives it happens to use.
///
/// Taking the borrows explicitly is the whole fix. Nothing about the rendering
/// changed — this is the same code reading the same state through a smaller
/// door — and [`crate::responsive::tests::assert_fits`] can now drive it.
///
/// Changes go on [`EditorCmd`](crate::EditorCmd) and apply after the frame, the
/// same deferral every other panel uses: geometry ops need `&mut Editor` for the
/// undo snapshot and the store, and the frame is already borrowing those.
pub(crate) struct MapCtx<'a> {
    pub(crate) world: &'a mut World,
    /// Read only — the tab acts on `selection.last()`, and changing the
    /// selection is an `EditorCmd`.
    pub(crate) selection: &'a [Entity],
    pub(crate) maps: &'a map_edit::MapStore,
    pub(crate) map_sel: &'a Option<map_edit::MapSel>,
    pub(crate) map_mode: map_edit::MapSubMode,
    pub(crate) map_slot_name: &'a mut String,
    pub(crate) map_opts: &'a mut map_edit::MapOpts,
    pub(crate) map_size_buf: &'a mut Option<Vec3>,
    pub(crate) map_spec_buf: &'a mut Option<floptle_map::ShapeSpec>,
    pub(crate) map_arm: Option<map_edit::MapShape>,
    pub(crate) map_knife_on: bool,
    pub(crate) map_orient: &'a mut map_edit::MapOrient,
    pub(crate) map_xform: &'a mut map_edit::MapXform,
    pub(crate) map_select_hidden: &'a mut bool,
    pub(crate) map_bevel: &'a mut map_edit::BevelWidth,
    /// True while the ▦ Map TOOL is active — every sub-object op needs it, so
    /// the tab offers to turn it on rather than silently greying out.
    pub(crate) map_tool_on: bool,
    pub(crate) map_playing: bool,
    /// The Map tool's keybinds — every hint in the UI reads its chord from
    /// here, so a rebind can never leave the labels lying.
    pub(crate) map_keys: &'a mut map_keys::MapKeys,
    pub(crate) map_rebind: &'a mut Option<map_keys::MapCmd>,
    pub(crate) map_rebind_err: &'a mut Option<String>,
    // The FACE MATERIALS section drives the ordinary material inspector, which
    // wants the project's asset and shader caches.
    pub(crate) materials: &'a [(String, floptle_scene::MaterialDoc)],
    pub(crate) mat_name_buf: &'a mut String,
    pub(crate) flsl_cache: &'a crate::shaders::FlslCache,
    pub(crate) sdf_cache: &'a crate::shaders::SdfCache,
    pub(crate) asset_tree: &'a [crate::assets::AssetEntry],
    pub(crate) texture_settings: &'a std::collections::HashMap<String, crate::assets::TexSetting>,
    pub(crate) project_root: &'a std::path::Path,
    pub(crate) cmd: &'a mut crate::EditorCmd,
}

impl MapCtx<'_> {
    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        if self.map_playing {
            ui.colored_label(ACCENT, "⏹  Stop the scene to edit model geometry");
            ui.small(
                "edits during Play would not be undoable and the physics collider \
                 would not rebuild, so the tool stays out of the way until you stop.",
            );
            return;
        }
        // The whole tab runs on the ▦ Map TOOL's sub-object selection. Say so
        // once, at the top, with the button that fixes it — greying everything
        // out with no explanation is what made this feel broken.
        if !self.map_tool_on {
            crate::responsive::group(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("▦  Turn on the Model tool")
                        .on_hover_text(
                            "key 8 — needed to draw shapes and select faces in the viewport",
                        )
                        .clicked()
                    {
                        self.cmd.set_tool = Some(Tool::MapEdit);
                    }
                    ui.small("(key 8)");
                });
            });
        }

        self.map_draw_section(ui);
        self.map_select_section(ui);
        self.map_transform_section(ui);
        self.map_modify_section(ui);
        let target = self.selection.last().and_then(|&e| match self.world.get::<Matter>(e) {
            Some(Matter::MapMesh { id }) => Some((e, *id)),
            _ => None,
        });
        if let Some((entity, id)) = target {
            self.map_shape_section(ui, id);
            self.map_size_section(ui, id);
            self.map_materials_section(ui, entity, id);
        }
        self.map_keys_section(ui);
        ui.add_space(12.0);
    }

    // ---- DRAW ---------------------------------------------------------------

    fn map_draw_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "DRAW");
        // A segmented strip rather than a row of fixed-width buttons: the shape
        // chips carry their shortcut in the label, so they are the widest thing
        // in the tab and the first to leave a narrow panel. `strip` shrinks them
        // together, then wraps them together, and drops each one to its bare
        // shape name when the shortcut no longer fits beside it.
        let hover = format!(
            "Drag out the footprint on the ground (or on any map surface you \
             aim at), release, then move to set the height and click.\n\
             {} / {} turn it 90°, {} turns it around, {} / {} change its \
             resolution, Esc cancels.",
            self.map_keys.label(MapCmd::TurnLeft),
            self.map_keys.label(MapCmd::TurnRight),
            self.map_keys.label(MapCmd::TurnAround),
            self.map_keys.label(MapCmd::ResolutionDown),
            self.map_keys.label(MapCmd::ResolutionUp),
        );
        let labels: Vec<(String, String, String)> = MapShape::ALL
            .iter()
            .map(|&shape| {
                let short = shape.label().trim_start_matches("Model ").to_string();
                let key = self.map_keys.label(shape.cmd());
                (format!("{short}  {key}"), short, format!("{hover}\nShortcut: {key}"))
            })
            .collect();
        let chips: Vec<Chip<'_>> = MapShape::ALL
            .iter()
            .zip(&labels)
            .map(|(&shape, (long, short, hover))| {
                Chip::mode(long, hover, self.map_arm == Some(shape)).short(short)
            })
            .collect();
        if let Some(i) = strip(ui, &chips) {
            let shape = MapShape::ALL[i];
            let armed = self.map_arm == Some(shape);
            self.cmd.set_map_arm = Some(if armed { None } else { Some(shape) });
        }
        ui.add_space(2.0);
        match self.map_arm {
            Some(shape) => {
                ui.horizontal_wrapped(|ui| {
                    crate::responsive::para(
                        ui,
                        RichText::new(format!(
                            "✏  drag out a {} — base first, then height",
                            shape.label().trim_start_matches("Model ").to_lowercase()
                        ))
                        .color(DRAW_ACCENT),
                    );
                    if ui.small_button("stop (Esc)").clicked() {
                        self.cmd.set_map_arm = Some(None);
                    }
                });
            }
            None => {
                ui.small("pick a shape, then drag in the viewport: footprint first, then height");
            }
        }

        egui::CollapsingHeader::new(RichText::new("Defaults for NEW shapes").small())
            .id_salt("map_shape_opts")
            .default_open(crate::responsive::start_open(false))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "what the next shape is built with — to change the shape you have \
                         SELECTED, use the SHAPE section further down",
                    )
                    .weak()
                    .small(),
                );
                row(ui, "round", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.sides)
                            .range(3..=128)
                            .prefix("sides "),
                    )
                    .on_hover_text("cylinder / sphere segments");
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.rings)
                            .range(2..=64)
                            .prefix("rings "),
                    )
                    .on_hover_text("sphere latitude bands");
                });
                row(ui, "stairs", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.steps)
                            .range(1..=64)
                            .prefix("steps "),
                    );
                    ui.small("[ and ] while drawing");
                });
                row(ui, "arch", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.arch_segments)
                            .range(2..=32)
                            .prefix("segments "),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.arch_width)
                            .speed(0.01)
                            .range(0.05..=0.98)
                            .prefix("opening w "),
                    )
                    .on_hover_text("opening width as a fraction of the arch's width");
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.arch_height)
                            .speed(0.01)
                            .range(0.05..=0.98)
                            .prefix("h "),
                    )
                    .on_hover_text(
                        "opening height (jamb + arc) as a fraction of the arch's height",
                    );
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("spawn at camera:").weak());
                    for shape in MapShape::ALL {
                        if ui
                            .small_button(shape.label().trim_start_matches("Model "))
                            .on_hover_text("drop one at a default size in front of the camera")
                            .clicked()
                        {
                            self.cmd.add_map_shape = Some(shape);
                        }
                    }
                });
            });
    }

    // ---- SELECT -------------------------------------------------------------

    fn map_select_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "SELECT");
        // Each chip carries its OWN key, not just "Tab cycles" — the direct
        // binds existed all along and nothing said so. Under pressure the chip
        // falls back to the glyph, which is the one part of the label that is
        // still unambiguous at 30 pixels.
        let modes: Vec<(String, String, String)> = MapSubMode::ALL
            .iter()
            .map(|&mode| {
                (
                    format!("{}  {}", mode.glyph(), mode.label()),
                    mode.glyph().to_string(),
                    format!(
                        "select {} — {} (or {} to cycle).\nYour selection CONVERTS, it isn't \
                         dropped: pick a face, switch to vertex, and you're holding its corners.",
                        mode.plural(),
                        self.map_keys.label(mode.cmd()),
                        self.map_keys.label(MapCmd::ModeCycle),
                    ),
                )
            })
            .collect();
        let picked = row(ui, "mode", |ui| {
            let chips: Vec<Chip<'_>> = MapSubMode::ALL
                .iter()
                .zip(&modes)
                .map(|(&mode, (long, glyph, hover))| {
                    Chip::mode(long, hover, self.map_mode == mode).short(glyph)
                })
                .collect();
            strip(ui, &chips)
        });
        if let Some(i) = picked {
            self.cmd.set_map_mode = Some(MapSubMode::ALL[i]);
        }

        let counts = self.map_selection_counts();
        let (faces, edges, verts) = counts.unwrap_or((0, 0, 0));
        let total = faces + edges + verts;
        let mesh_size = self.map_target_mesh().map(|m| (m.faces.len(), m.verts.len()));
        row(ui, "", |ui| {
            match counts {
                None => {
                    ui.label(RichText::new("no model node selected").weak());
                }
                Some(_) if total == 0 => {
                    ui.label(RichText::new("nothing selected").weak());
                }
                Some(_) => {
                    let plural =
                        |n: usize, s: &str| format!("{n} {s}{}", if n == 1 { "" } else { "s" });
                    let mut parts = Vec::new();
                    if faces > 0 {
                        parts.push(plural(faces, "face"));
                    }
                    if edges > 0 {
                        parts.push(plural(edges, "edge"));
                    }
                    if verts > 0 {
                        parts.push(plural(verts, "vert"));
                    }
                    ui.colored_label(ACCENT, format!("{} selected", parts.join(" · ")));
                }
            }
            if let Some((f, v)) = mesh_size {
                ui.label(RichText::new(format!("  (mesh: {f} faces, {v} verts)")).weak().small());
            }
        });
        row(ui, "", |ui| {
            if action(
                ui,
                true,
                &format!("All {}", self.map_mode.plural()),
                &format!(
                    "select every {} in this mesh  (Ctrl+A, or {})",
                    self.map_mode.label(),
                    self.map_keys.label(MapCmd::SelectAll)
                ),
            ) {
                self.cmd.map_op = Some(MapOp::SelectAll);
            }
            if action(
                ui,
                total > 0,
                "None",
                &format!(
                    "clear the sub-object selection  ({})",
                    self.map_keys.label(MapCmd::SelectNone)
                ),
            ) {
                self.cmd.map_op = Some(MapOp::SelectNone);
            }
            if action(
                ui,
                true,
                "Invert",
                &format!(
                    "swap selected for unselected  ({})",
                    self.map_keys.label(MapCmd::SelectInvert)
                ),
            ) {
                self.cmd.map_op = Some(MapOp::SelectInvert);
            }
            if action(ui, total > 0, "Grow", "add the neighbouring ring") {
                self.cmd.map_op = Some(MapOp::Grow);
            }
            if action(ui, total > 0, "Connected", "everything joined to the selection") {
                self.cmd.map_op = Some(MapOp::SelectConnected);
            }
            if action(
                ui,
                faces > 0,
                "Coplanar",
                "spread across the flat region this face sits in",
            ) {
                self.cmd.map_op = Some(MapOp::SelectCoplanar);
            }
            if action(ui, edges > 0, "Edge loop", "run the selection along its quad loops") {
                self.cmd.map_op = Some(MapOp::SelectLoop);
            }
        });
        row(ui, "", |ui| {
            ui.checkbox(&mut *self.map_select_hidden, "select through the surface").on_hover_text(
                "off (default): only sub-objects you can actually see are clickable or \
                 box-selectable — no more grabbing the vertex on the far side of a wall",
            );
        });
        row(ui, "", |ui| {
            ui.label(
                RichText::new(
                    "drag anywhere to box-select  ·  Shift adds  ·  Ctrl removes",
                )
                .weak()
                .small(),
            )
            .on_hover_text(
                "the box starts wherever you press — including on the mesh — so a whole row \
                 of faces is one drag. A press that doesn't move is a plain click.",
            );
        });
    }

    // ---- TRANSFORM ----------------------------------------------------------

    fn map_transform_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "TRANSFORM");
        const XFORMS: [MapXform; 3] = [MapXform::Move, MapXform::Rotate, MapXform::Scale];
        let picked = row(ui, "gizmo", |ui| {
            let chips: Vec<Chip<'_>> = XFORMS
                .iter()
                .map(|&x| Chip::mode(x.label(), "what the gizmo does — X cycles", *self.map_xform == x))
                .collect();
            strip(ui, &chips)
        });
        if let Some(i) = picked {
            *self.map_xform = XFORMS[i];
        }

        const ORIENTS: [MapOrient; 3] = [MapOrient::Normal, MapOrient::Local, MapOrient::Global];
        let picked = row(ui, "handles", |ui| {
            let chips: Vec<Chip<'_>> = ORIENTS
                .iter()
                .map(|&o| {
                    let hover = match o {
                        MapOrient::Normal => {
                            "along the selection itself — a diagonal face pushes straight out of \
                             its own surface in one drag (V cycles)"
                        }
                        MapOrient::Local => "the node's own axes (V cycles)",
                        MapOrient::Global => "world axes (V cycles)",
                    };
                    Chip::mode(o.label(), hover, *self.map_orient == o)
                })
                .collect();
            strip(ui, &chips)
        });
        if let Some(i) = picked {
            *self.map_orient = ORIENTS[i];
        }
    }

    // ---- MODIFY -------------------------------------------------------------

    fn map_modify_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "MODIFY");
        let (faces, edges, verts) = self.map_selection_counts().unwrap_or((0, 0, 0));
        row(ui, "faces", |ui| {
            if action(
                ui,
                faces > 0,
                "⬆ Extrude",
                "push the selected faces out along their own normal  (E)",
            ) {
                self.cmd.map_op = Some(MapOp::Extrude);
            }
            if action(
                ui,
                faces > 0,
                "⊡ Inset",
                "shrink a copy of each face inside its own border — inset then extrude \
                 carves a recess  (I)",
            ) {
                self.cmd.map_op = Some(MapOp::Inset);
            }
            if action(ui, faces > 0, "⊞ Subdivide", "split each selected face into quads") {
                self.cmd.map_op = Some(MapOp::Subdivide);
            }
            if action(
                ui,
                faces == 2,
                "⇌ Bridge",
                "join two selected faces with a tube of walls (they need the same corner count)",
            ) {
                self.cmd.map_op = Some(MapOp::Bridge);
            }
        });
        // Edge tools. A loop cut is the one thing every modeling tool has and
        // this one did not: it gives a shape somewhere to bend without changing
        // how it looks at all.
        row(ui, "", |ui| {
            if action(
                ui,
                edges > 0,
                "▤ Loop cut",
                &format!(
                    "insert a new edge loop running at right angles to the selected edge, \
                     halfway along  ({})",
                    self.map_keys.label(MapCmd::LoopCut)
                ),
            ) {
                self.cmd.map_op = Some(MapOp::LoopCut(0.5));
            }
            if action(
                ui,
                edges > 0,
                "◠ Bevel",
                &format!(
                    "take the sharpness off the selected edges, so they catch the light  ({})",
                    self.map_keys.label(MapCmd::Bevel)
                ),
            ) {
                self.cmd.map_op = Some(MapOp::Bevel(self.map_bevel.0));
            }
            let mut w = self.map_bevel.0;
            if ui
                .add(egui::DragValue::new(&mut w).speed(0.005).range(0.005..=2.0).prefix("width "))
                .on_hover_text("how wide the bevel takes the corner off, in the mesh's own units")
                .changed()
            {
                self.map_bevel.0 = w;
            }
            if action(
                ui,
                edges > 0,
                "⇉ Ring",
                &format!(
                    "extend the selection across the strip of quads — the edges a loop cut \
                     would run through  ({})",
                    self.map_keys.label(MapCmd::SelectRing)
                ),
            ) {
                self.cmd.map_op = Some(MapOp::SelectRing);
            }
        });
        row(ui, "", |ui| {
            if action(ui, faces > 0, "🗑 Delete", "remove the selected faces  (Del)") {
                self.cmd.map_op = Some(MapOp::DeleteFaces);
            }
            if action(
                ui,
                faces > 0,
                "✂ Split off",
                "move the selected faces into their own model node",
            ) {
                self.cmd.map_detach = true;
            }
            if action(ui, faces > 0, "⇄ Flip", "reverse the selected faces' winding") {
                self.cmd.map_op = Some(MapOp::FlipFaces);
            }
            if action(
                ui,
                true,
                "⇄ Flip all",
                "turn the whole mesh inside out (fixes a shape rendering inside-out)",
            ) {
                self.cmd.map_op = Some(MapOp::FlipAll);
            }
        });
        row(ui, "cut", |ui| {
            let on = self.map_knife_on;
            if ui
                .add_sized(
                    [CHIP_W + 14.0, BTN_H],
                    egui::Button::selectable(
                        on,
                        format!("✂ Knife  {}", self.map_keys.label(MapCmd::Knife)),
                    ),
                )
                .on_hover_text(
                    "click one edge or corner of a face, then another, and the face splits \
                     along that line. The cut carries into the faces sharing those edges, so \
                     the seam stays welded. Keeps cutting from the corner it just made — Esc \
                     ends the cut, Esc again puts the knife away.",
                )
                .clicked()
            {
                self.cmd.set_map_knife = Some(!on);
            }
            if on {
                ui.colored_label(
                    DRAW_ACCENT,
                    RichText::new("✂ cutting — click a border point").small(),
                );
            }
        });
        row(ui, "points", |ui| {
            if action(
                ui,
                faces + edges + verts > 0,
                "⊙ Weld",
                "merge selected verts that are within the weld radius of each other",
            ) {
                self.cmd.map_op = Some(MapOp::WeldSelected);
            }
            if action(
                ui,
                true,
                "⊞ Snap to grid",
                "round the selection (or the whole mesh) onto the grid",
            ) {
                self.cmd.map_op = Some(MapOp::SnapToGrid);
            }
        });
        egui::CollapsingHeader::new(RichText::new("Amounts").small()).id_salt("map_amounts").default_open(crate::responsive::start_open(false)).show(
            ui,
            |ui| {
                row(ui, "extrude", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.extrude)
                            .speed(0.05)
                            .range(0.01..=500.0),
                    )
                    .on_hover_text("how far E pushes — grid snap overrides this while it is on");
                });
                row(ui, "inset", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.inset)
                            .speed(0.01)
                            .range(0.001..=100.0),
                    );
                });
                row(ui, "weld", |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.map_opts.weld)
                            .speed(0.005)
                            .range(0.0001..=10.0),
                    )
                    .on_hover_text("verts closer than this to each other merge");
                });
            },
        );
    }

    // ---- SHAPE --------------------------------------------------------------

    /// Shape parameters for a node that is still the primitive it was drawn as
    /// — step count, sides, arch opening — plus the facing controls. Editing
    /// the geometry retires the parameters (the mesh is no longer that shape,
    /// and silently re-generating would throw the edit away), but turning the
    /// node is just a rotation, so that always works.
    fn map_shape_section(&mut self, ui: &mut egui::Ui, id: u32) {
        use floptle_map::ShapeKind;
        let Some(mesh) = self.maps.meshes.get(&id) else { return };
        if mesh.bounds().is_none() {
            return;
        }
        let spec = mesh.spec;
        let title = match spec {
            Some(s) => format!(
                "SHAPE — {}",
                MapShape::of_kind(s.kind).label().trim_start_matches("Model ").to_uppercase()
            ),
            None => "SHAPE".to_string(),
        };
        section(ui, &title);
        row(ui, "", |ui| {
            ui.label(RichText::new("the SELECTED node's own settings").weak().small());
        });
        row(ui, "facing", |ui| {
            if action(ui, true, "⟲ 90°", "turn left a quarter turn about the node's up axis  (,)")
            {
                self.cmd.map_turn = Some(-1);
            }
            if action(ui, true, "⟳ 90°", "turn right a quarter turn  (.)") {
                self.cmd.map_turn = Some(1);
            }
            if action(
                ui,
                true,
                "⇄ 180°",
                "turn it right around  (Z) — for a stair or ramp this is \"climb the other \
                 way\"; the green arrow in the viewport shows which way it goes",
            ) {
                self.cmd.map_turn = Some(2);
            }
        });
        let Some(spec) = spec else {
            row(ui, "", |ui| {
                ui.label(
                    RichText::new(
                        "edited geometry — no shape parameters to adjust (undo past the edit, \
                         or draw a fresh one)",
                    )
                    .weak()
                    .small(),
                );
            });
            return;
        };
        // Buffered like the size fields: apply on release, so a drag is one
        // undo step rather than one per frame.
        let mut next = self.map_spec_buf.unwrap_or(spec);
        if next.kind != spec.kind {
            next = spec;
        }
        let mut editing = false;
        let mut done = false;
        {
            let (e, d) = (&mut editing, &mut done);
            let mut knob = |ui: &mut egui::Ui,
                            v: &mut u32,
                            range: std::ops::RangeInclusive<u32>,
                            prefix: &str| {
                let r = ui.add(egui::DragValue::new(v).range(range).prefix(prefix));
                *e |= r.changed() || r.dragged();
                *d |= r.drag_stopped() || r.lost_focus();
            };
            match spec.kind {
                ShapeKind::Stairs => {
                    row(ui, "steps", |ui| {
                        knob(ui, &mut next.steps, 1..=64, "");
                        ui.small("[ and ] step it from the viewport");
                    });
                }
                ShapeKind::Cylinder => {
                    row(ui, "sides", |ui| knob(ui, &mut next.sides, 3..=128, ""));
                }
                ShapeKind::Sphere => row(ui, "detail", |ui| {
                    knob(ui, &mut next.sides, 3..=128, "segments ");
                    knob(ui, &mut next.rings, 2..=64, "rings ");
                }),
                ShapeKind::Arch => {
                    row(ui, "arc", |ui| knob(ui, &mut next.arch_segments, 2..=32, "segments "));
                }
                ShapeKind::Box | ShapeKind::Plane | ShapeKind::Wedge => row(ui, "", |ui| {
                    ui.label(
                        RichText::new("no resolution to adjust — see SIZE below").weak().small(),
                    );
                }),
            }
        }
        if spec.kind == ShapeKind::Arch {
            row(ui, "opening", |ui| {
                let w = ui.add(
                    egui::DragValue::new(&mut next.arch_width)
                        .speed(0.01)
                        .range(0.05..=0.98)
                        .prefix("width "),
                );
                let h = ui.add(
                    egui::DragValue::new(&mut next.arch_height)
                        .speed(0.01)
                        .range(0.05..=0.98)
                        .prefix("height "),
                );
                editing |= w.changed() || w.dragged() || h.changed() || h.dragged();
                done |= w.drag_stopped() || w.lost_focus() || h.drag_stopped() || h.lost_focus();
                ui.small("of the shape");
            });
        }
        if editing || self.map_spec_buf.is_some() {
            *self.map_spec_buf = Some(next);
        }
        if done {
            self.cmd.map_op = Some(MapOp::Reshape(next));
            *self.map_spec_buf = None;
        }
    }

    // ---- SIZE ---------------------------------------------------------------

    /// The numeric half of a modeling tool. Editing geometry is how a map mesh
    /// gets sized — scaling the NODE would stretch the box-projected UVs and
    /// detune every texture on it.
    fn map_size_section(&mut self, ui: &mut egui::Ui, id: u32) {
        let Some(mesh) = self.maps.meshes.get(&id) else { return };
        let Some((lo, hi)) = mesh.bounds() else { return };
        section(ui, "SIZE");
        let live = self.map_size_buf.is_some();
        let mut size = self.map_size_buf.unwrap_or(hi - lo);
        row(ui, "extents", |ui| {
            let mut editing = false;
            let mut done = false;
            for (i, axis) in ["x ", "y ", "z "].iter().enumerate() {
                let r = ui.add(
                    egui::DragValue::new(&mut size[i])
                        .speed(0.05)
                        .range(0.01..=100000.0)
                        .prefix(*axis),
                );
                editing |= r.changed() || r.dragged();
                done |= r.drag_stopped() || r.lost_focus();
            }
            if editing || live {
                *self.map_size_buf = Some(size);
            }
            if done {
                self.cmd.map_op = Some(MapOp::Resize(size));
                *self.map_size_buf = None;
            }
            ui.label(
                RichText::new("geometry, not node scale — textures keep their real size")
                    .weak()
                    .small(),
            );
        });
        row(ui, "pivot", |ui| {
            if action(
                ui,
                true,
                "⌖ Center",
                "move the node's origin to the middle of the mesh (nothing moves on screen)",
            ) {
                self.cmd.map_op = Some(MapOp::CenterPivot);
            }
            if action(
                ui,
                true,
                "⌖ To selection",
                "put the origin on the selected sub-objects — the point rotation and scale \
                 work around",
            ) {
                self.cmd.map_op = Some(MapOp::PivotToSelection);
            }
        });
    }

    // ---- FACE MATERIALS -----------------------------------------------------

    fn map_materials_section(&mut self, ui: &mut egui::Ui, entity: floptle_core::Entity, id: u32) {
        section(ui, "FACE MATERIALS");
        let faces = self.map_selection_counts().map_or(0, |(f, _, _)| f);
        // The headline action. Previously this took "add slot" + "assign" +
        // "override" and read like it did nothing.
        ui.horizontal_wrapped(|ui| {
            let r = ui.add_enabled(
                faces > 0,
                egui::Button::new("◑  New material for selected faces")
                    .truncate()
                    .min_size(Vec2::new(0.0, BTN_H + 2.0)),
            );
            let hover = if faces > 0 {
                "makes a slot from the selection and gives it its own material — this is how \
                 one face gets a different look from the rest"
            } else {
                "select some faces first (▦ Model tool, face mode)"
            };
            if r.on_hover_text(hover).on_disabled_hover_text(hover).clicked() {
                let name = {
                    let n = self.map_slot_name.trim();
                    if n.is_empty() {
                        format!("Material {}", self.slot_count(id) + 1)
                    } else {
                        n.to_string()
                    }
                };
                self.map_slot_name.clear();
                self.cmd.map_op = Some(MapOp::MaterialFromSelection(name));
            }
            ui.add(
                egui::TextEdit::singleline(self.map_slot_name)
                    .hint_text("name (optional)")
                    .desired_width(crate::responsive::fit_here(ui, 110.0)),
            );
        });
        ui.add_space(4.0);

        let Some(mesh) = self.maps.meshes.get(&id) else { return };
        let slot_names: Vec<String> = mesh.slots.clone();
        let counts: Vec<usize> = (0..slot_names.len())
            .map(|i| mesh.faces.iter().filter(|f| f.slot as usize == i).count())
            .collect();
        for (i, name) in slot_names.iter().enumerate() {
            let has = self
                .world
                .get::<floptle_core::ObjectMaterials>(entity)
                .is_some_and(|om| om.0.contains_key(name));
            crate::responsive::group(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(name).strong());
                        ui.label(
                            RichText::new(format!("{} of this mesh's faces", counts[i]))
                                .weak()
                                .small(),
                        )
                        .on_hover_text("how many faces draw with this slot — not your selection");
                        // Plain flow, not `right_to_left`. A right-aligned run
                        // pins itself to the REGION's right edge and grows
                        // leftwards from there, so in a narrow panel it walks off
                        // the LEFT side instead of the right — same bug, harder
                        // to recognise. These two buttons read fine in order.
                        {
                            if ui
                                .small_button("Select")
                                .on_hover_text("select every face drawing with this slot")
                                .clicked()
                            {
                                self.cmd.map_op = Some(MapOp::SelectSlot(i as u16));
                            }
                            if ui
                                .add_enabled(
                                    faces > 0,
                                    egui::Button::new("Assign selection").small(),
                                )
                                .on_hover_text("move the selected faces onto this slot")
                                .clicked()
                            {
                                self.cmd.map_op = Some(MapOp::AssignSlot(i as u16));
                            }
                        }
                    });
                    // Per-node material override for this slot (the same
                    // ObjectMaterials machinery imported models use).
                    if has {
                        egui::CollapsingHeader::new(RichText::new("material").small())
                            .id_salt(("map_slot_mat", id, i))
                            .default_open(true)
                            .show(ui, |ui| {
                                let (materials, asset_tree, project_root, flsl, sdf, tex_set) = (
                                    &*self.materials,
                                    &*self.asset_tree,
                                    &*self.project_root,
                                    &*self.flsl_cache,
                                    &*self.sdf_cache,
                                    &*self.texture_settings,
                                );
                                if let Some(om) =
                                    self.world.get_mut::<floptle_core::ObjectMaterials>(entity)
                                    && let Some(mat) = om.0.get_mut(name)
                                {
                                    let res = inspector::material_props_ui(
                                        ui,
                                        mat,
                                        materials,
                                        asset_tree,
                                        project_root,
                                        self.mat_name_buf,
                                        flsl,
                                        sdf,
                                        tex_set,
                                    );
                                    self.cmd.inspector_changed |= res.changed;
                                    self.cmd.open_shader_graph =
                                        res.open_shader.or(self.cmd.open_shader_graph.take());
                                }
                                if ui.small_button("✖ clear override").clicked()
                                    && let Some(om) =
                                        self.world.get_mut::<floptle_core::ObjectMaterials>(entity)
                                {
                                    om.0.remove(name);
                                    self.cmd.inspector_changed = true;
                                }
                            });
                    } else if ui
                        .small_button("✚ give this slot its own material")
                        .on_hover_text("colour / texture / shader for every face on this slot")
                        .clicked()
                    {
                        if self.world.get::<floptle_core::ObjectMaterials>(entity).is_none() {
                            self.world.insert(entity, floptle_core::ObjectMaterials::default());
                        }
                        if let Some(om) =
                            self.world.get_mut::<floptle_core::ObjectMaterials>(entity)
                        {
                            om.0.insert(name.clone(), floptle_core::Material::default());
                            self.cmd.inspector_changed = true;
                        }
                    }
                });
        }
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            if ui
                .small_button("✚ empty slot")
                .on_hover_text("add a slot without assigning anything to it")
                .clicked()
            {
                let name = {
                    let n = self.map_slot_name.trim();
                    if n.is_empty() {
                        format!("Slot {}", slot_names.len() + 1)
                    } else {
                        n.to_string()
                    }
                };
                self.map_slot_name.clear();
                self.cmd.map_op = Some(MapOp::AddSlot(name));
            }
            if ui
                .small_button("♻ Clean unused geometry")
                .on_hover_text(
                    "drop stored geometry no node uses any more (duplicates and deleted nodes \
                     leave copies behind so undo can bring them back — anything undo could \
                     still restore is kept)",
                )
                .clicked()
            {
                self.cmd.map_prune = true;
            }
        });
    }

    // ---- KEYS ---------------------------------------------------------------

    /// Every control's hotkey, listed and rebindable. Click a chord, press the
    /// new one; anything the editor already answers in this context — or that
    /// another map command holds — is refused with the reason, so a broken
    /// binding can't come into being.
    fn map_keys_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "KEYS");
        // Wrapped, and the button is simply next in the flow rather than pinned
        // right: a `right_to_left` layout aligns to the REGION's right edge, and
        // a region is exactly the thing that grows — so pinning right is how a
        // button ends up outside a panel it was meant to sit inside.
        ui.horizontal_wrapped(|ui| {
            crate::responsive::para(
                ui,
                RichText::new(
                    "model keys only fire while the ▦ Model tool is active and you're not typing",
                )
                .weak()
                .small(),
            );
            {
                if ui
                    .small_button("Reset all")
                    .on_hover_text("back to the shipped bindings")
                    .clicked()
                {
                    *self.map_keys = crate::map_keys::MapKeys::default();
                    *self.map_rebind = None;
                    *self.map_rebind_err = None;
                    save_map_keys(self.map_keys);
                }
            }
        });
        if let Some(err) = self.map_rebind_err.clone() {
            ui.colored_label(Color32::from_rgb(235, 120, 120), RichText::new(err).small());
        }
        let listening = *self.map_rebind;
        for group in ["Draw", "Select", "Transform", "Modify"] {
            egui::CollapsingHeader::new(RichText::new(group).small())
                .id_salt(("map_keys", group))
                .default_open(crate::responsive::start_open(false))
                .show(ui, |ui| {
                    // Through `responsive::grid`, which bounds the column widths
                    // and falls through to a wrapped flow when the dock is too
                    // thin for two columns. A bare `egui::Grid` sizes itself from
                    // its content and grows past the panel — and the panel then
                    // wraps everything AFTER it against an edge off screen.
                    crate::responsive::grid(ui, ("map_keys_grid", group), |ui| {
                        for cmd in MapCmd::ALL.into_iter().filter(|c| c.group() == group) {
                            // `para`, not `label`: below two columns this grid
                            // lays out as a wrapped flow, and a caption there is
                            // a line of prose that has to wrap like one.
                            crate::responsive::para(
                                ui,
                                RichText::new(cmd.label()).small(),
                            );
                            let waiting = listening == Some(cmd);
                            let text = if waiting {
                                "press a key…".to_string()
                            } else {
                                self.map_keys.label(cmd)
                            };
                            // Elided to the button: a `Button`'s label extends
                            // rather than truncating, so "Shift+/" in a thin dock
                            // ran past a box that was itself inside the panel.
                            let bw = crate::responsive::fit_here_wrapping(ui, 110.0);
                            let pad = ui.spacing().button_padding.x * 2.0 + 4.0;
                            let shown = crate::responsive::elide(ui, &text, (bw - pad).max(8.0));
                            if ui
                                .add_sized(
                                    [bw, BTN_H],
                                    egui::Button::selectable(waiting, shown),
                                )
                                .on_hover_text(format!(
                                    "{text}\n\nclick, then press the key (Shift is part of \
                                     the chord; Ctrl belongs to the application). Esc cancels."
                                ))
                                .clicked()
                            {
                                *self.map_rebind = if waiting { None } else { Some(cmd) };
                                *self.map_rebind_err = None;
                            }
                            ui.end_row();
                        }
                    });
                });
        }
        egui::CollapsingHeader::new(RichText::new("Keys the editor keeps").small())
            .id_salt("map_keys_reserved")
            .default_open(crate::responsive::start_open(false))
            .show(ui, |ui| {
                crate::responsive::para(
                    ui,
                    RichText::new(
                        "these answer whatever modifiers are held, so the Model tool won't \
                         take them:",
                    )
                    .weak()
                    .small(),
                );
                let mut listed: Vec<(&str, Vec<String>)> = Vec::new();
                for key in crate::map_keys::known_keys() {
                    if let Some(what) = reserved(key) {
                        let label = crate::map_keys::key_label(key);
                        match listed.iter_mut().find(|(w, _)| *w == what) {
                            Some((_, keys)) => keys.push(label),
                            None => listed.push((what, vec![label])),
                        }
                    }
                }
                for (what, keys) in listed {
                    crate::responsive::para(
                        ui,
                        RichText::new(format!("{}  —  {what}", keys.join(" "))).small().weak(),
                    );
                }
            });
    }

    // ---- shared lookups -----------------------------------------------------

    /// `(faces, edges, verts)` selected on the targeted map node, or `None`
    /// when no map node is selected at all — the two states read differently
    /// in the UI and must not be conflated.
    fn map_selection_counts(&self) -> Option<(usize, usize, usize)> {
        let e = *self.selection.last()?;
        matches!(self.world.get::<Matter>(e), Some(Matter::MapMesh { .. })).then_some(())?;
        let sel = self.map_sel.as_ref().filter(|s| s.entity == e);
        Some(sel.map_or((0, 0, 0), |s| (s.faces.len(), s.edges.len(), s.verts.len())))
    }

    fn map_target_mesh(&self) -> Option<&floptle_map::MapMesh> {
        let e = *self.selection.last()?;
        match self.world.get::<Matter>(e) {
            Some(Matter::MapMesh { id }) => self.maps.meshes.get(id),
            _ => None,
        }
    }

    fn slot_count(&self, id: u32) -> usize {
        self.maps.meshes.get(&id).map_or(0, |m| m.slots.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_map::ShapeKind;

    /// A scene with one model node selected, holding an ARCH — the shape with
    /// the most parameters, so SHAPE, SIZE and FACE MATERIALS are all on screen
    /// at once — with faces selected so every MODIFY button is live rather than
    /// greyed, and a per-slot material override so the material inspector draws
    /// too. The point is to render the panel at its WIDEST, since a section that
    /// is not on screen cannot overflow.
    fn arch_scene() -> (World, Entity, map_edit::MapStore, Option<map_edit::MapSel>) {
        const ID: u32 = 1;
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, floptle_core::Name("Arch".into()));
        world.insert(e, Matter::MapMesh { id: ID });

        let spec = floptle_map::ShapeSpec::new(ShapeKind::Arch, Vec3::new(2.0, 3.0, 1.0));
        let mut mesh = spec.build();
        mesh.slots.push("Trim".into());

        let mut om = floptle_core::ObjectMaterials::default();
        om.0.insert("Trim".into(), floptle_core::Material::default());
        world.insert(e, om);

        let faces: std::collections::BTreeSet<u32> = (0..mesh.faces.len().min(3) as u32).collect();
        let sel = map_edit::MapSel {
            entity: e,
            id: ID,
            verts: Default::default(),
            edges: [(0u32, 1u32)].into_iter().collect(),
            faces,
            anchor: None,
        };

        let mut maps = map_edit::MapStore::default();
        maps.meshes.insert(ID, mesh);
        (world, e, maps, Some(sel))
    }

    /// **The panel must survive being dragged thin.** The ▦ Model tab is the
    /// widest form panel in the editor — seven titled sections of chips and
    /// action buttons — and it is the one this guard was written for.
    ///
    /// It can be driven at all because the tab takes [`MapCtx`] rather than
    /// rendering off `EditorTabViewer`'s hundred-field borrow. That is the whole
    /// argument for the context struct: a panel that cannot be constructed
    /// cannot be asserted on.
    #[test]
    fn the_panel_fits_however_thin_the_dock_gets() {
        let (mut world, e, maps, map_sel) = arch_scene();
        let selection = vec![e];
        let mut slot_name = String::new();
        let mut opts = map_edit::MapOpts::default();
        let mut size_buf = None;
        let mut spec_buf = None;
        let mut orient = map_edit::MapOrient::default();
        let mut xform = map_edit::MapXform::default();
        let mut select_hidden = false;
        let mut bevel = map_edit::BevelWidth::default();
        let mut keys = map_keys::MapKeys::default();
        let mut rebind = None;
        let mut rebind_err = None;
        let mut mat_name = String::new();
        let flsl = crate::shaders::FlslCache::default();
        let sdf = crate::shaders::SdfCache::default();
        let tex = std::collections::HashMap::new();
        let root = std::path::PathBuf::from(".");
        let mut cmd = crate::EditorCmd::default();

        crate::responsive::tests::assert_fits("the ▦ Model tab", |ui| {
            MapCtx {
                world: &mut world,
                selection: &selection,
                maps: &maps,
                map_sel: &map_sel,
                map_mode: map_edit::MapSubMode::default(),
                map_slot_name: &mut slot_name,
                map_opts: &mut opts,
                map_size_buf: &mut size_buf,
                map_spec_buf: &mut spec_buf,
                // Armed, so the DRAW strip renders in its selected state and the
                // "stop (Esc)" row is on screen too.
                map_arm: Some(MapShape::Arch),
                map_knife_on: true,
                map_orient: &mut orient,
                map_xform: &mut xform,
                map_select_hidden: &mut select_hidden,
                map_bevel: &mut bevel,
                map_tool_on: false, // draws the "turn the tool on" banner as well
                map_playing: false,
                map_keys: &mut keys,
                map_rebind: &mut rebind,
                map_rebind_err: &mut rebind_err,
                materials: &[],
                mat_name_buf: &mut mat_name,
                flsl_cache: &flsl,
                sdf_cache: &sdf,
                asset_tree: &[],
                texture_settings: &tex,
                project_root: &root,
                cmd: &mut cmd,
            }
            .ui(ui);
        });
    }
}
