//! The 🖌 Paint dock tab: vertex-brush settings.
//!
//! A tab rather than an Inspector section on purpose — the Inspector is per-entity and
//! rebuilds on every selection change, but brush state has to outlive selection (you
//! pick a color once and paint ten props with it).

use crate::EditorTabViewer;

/// What surface a brush paints INTO. Vertex = per-vertex color (resolution follows the
/// mesh's tessellation, the classic retro look); Texture = a per-node paint texture sampled
/// through the mesh UVs (resolution-independent — paint fine detail on a flat low-poly wall).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaintTarget {
    Vertex,
    Texture,
}

/// What a dab does to the vertices under it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaintMode {
    /// Blend toward the brush color.
    Paint,
    /// Average each vertex toward its neighbours-in-radius — softens hard edges.
    Smooth,
    /// Undo paint under the brush: texture paint returns to the node's ORIGINAL look
    /// (the seeded canvas), vertex paint returns to neutral. Strength/falloff apply,
    /// so a soft low-strength eraser fades paint out gradually.
    Erase,
    /// Adopt the color under the cursor as the brush color (eyedropper).
    Sample,
}

/// How a dab's color combines with what's already on the vertex.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlendMode {
    /// Lerp toward the brush color — the normal paint.
    Mix,
    /// Darken by the brush color. The natural way to paint baked shadow/AO into
    /// vertices, which is the whole retro lighting trick.
    Multiply,
    /// Add light. Paint bounce/rim highlights.
    Add,
    /// Subtract — carve darkness out.
    Subtract,
    /// Keep whichever is brighter per channel.
    Lighten,
    /// Keep whichever is darker per channel.
    Darken,
}

impl BlendMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            BlendMode::Mix => "Mix",
            BlendMode::Multiply => "Multiply",
            BlendMode::Add => "Add",
            BlendMode::Subtract => "Subtract",
            BlendMode::Lighten => "Lighten",
            BlendMode::Darken => "Darken",
        }
    }
    pub(crate) const ALL: [BlendMode; 6] = [
        BlendMode::Mix,
        BlendMode::Multiply,
        BlendMode::Add,
        BlendMode::Subtract,
        BlendMode::Lighten,
        BlendMode::Darken,
    ];

    /// Combine `cur` with the brush color `src` (both 0..255 per channel). The result
    /// is the FULL-strength target; the brush weight lerps toward it afterwards, so
    /// every mode responds to strength/falloff the same way.
    pub(crate) fn apply(self, cur: f32, src: f32) -> f32 {
        match self {
            BlendMode::Mix => src,
            BlendMode::Multiply => cur * src / 255.0,
            BlendMode::Add => cur + src,
            BlendMode::Subtract => cur - src,
            BlendMode::Lighten => cur.max(src),
            BlendMode::Darken => cur.min(src),
        }
        .clamp(0.0, 255.0)
    }
}

/// Vertex-paint brush settings.
#[derive(Clone, Copy)]
pub(crate) struct VertexBrush {
    /// Paint into per-vertex color or a per-node texture.
    pub(crate) target: PaintTarget,
    pub(crate) mode: PaintMode,
    pub(crate) radius: f32,
    pub(crate) strength: f32,
    pub(crate) color: [f32; 3],
    pub(crate) alpha: f32,
    /// The weight ramp from core to rim — shared with the terrain brush.
    pub(crate) profile: floptle_field::BrushProfile,
    /// Dab spacing as a fraction of the radius (was hardcoded at 0.34).
    pub(crate) spacing: f32,
    pub(crate) blend: BlendMode,
    /// Per-channel write mask. Off-channels are left untouched — which is what makes
    /// paint usable as SHADER DATA (paint a mask into red without disturbing the
    /// color you already laid down in green/blue).
    pub(crate) channels: [bool; 4],
    /// Paint vertices whose normal faces away from the camera too. Off by default:
    /// otherwise a dab on a thin wall silently paints its far side as well.
    pub(crate) backfaces: bool,
}

impl Default for VertexBrush {
    fn default() -> Self {
        Self {
            target: PaintTarget::Vertex,
            mode: PaintMode::Paint,
            radius: 0.5,
            strength: 0.6,
            color: [0.9, 0.25, 0.2],
            alpha: 1.0,
            profile: floptle_field::BrushProfile::default(),
            spacing: 0.34,
            blend: BlendMode::Mix,
            channels: [true; 4],
            backfaces: false,
        }
    }
}

impl EditorTabViewer<'_> {
    pub(crate) fn paint_ui(&mut self, ui: &mut egui::Ui) {
        let brush = &mut *self.vertex_brush;
        let cmd = &mut *self.cmd;

        if self.playing {
            // Paint edits asset-adjacent data that Stop does NOT revert, and
            // `push_history` is a hard no-op while playing — so a Play-time stroke
            // would be both unrecorded and un-undoable. Refuse rather than lose work.
            ui.label("⏸ Paint is disabled during Play.");
            ui.small("Stop to paint — strokes made while playing couldn't be undone.");
            return;
        }

        ui.label("Paint tool (key 7) — LMB-drag to paint onto a mesh or primitive.");
        ui.label("Ctrl+Z/Y undo whole strokes.");
        ui.separator();

        // Vertex vs Texture: the resolution question. Vertex follows the mesh's polygons
        // (retro, free); Texture paints into a per-node image via the UVs (fine detail on
        // a flat wall, resolution-independent).
        ui.horizontal(|ui| {
            ui.label("into");
            ui.selectable_value(&mut brush.target, PaintTarget::Vertex, "▲ Vertices");
            ui.selectable_value(&mut brush.target, PaintTarget::Texture, "▦ Texture");
        });
        match brush.target {
            PaintTarget::Vertex => {
                ui.small("per-vertex color — resolution follows the mesh. Turn the material's");
                ui.small("⬛ unlit on for the classic no-lighting look; it's also the fastest path.");
            }
            PaintTarget::Texture => {
                ui.small("a transparent paint OVERLAY — the node underneath never changes.");
                ui.small("The brush is a sphere and paints EVERY surface inside it, so a stroke");
                ui.small("along a wall-floor seam shades both sides at once (painted AO).");
            }
        }
        ui.separator();

        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut brush.mode, PaintMode::Paint, "🖌 Paint");
            if brush.target == PaintTarget::Vertex {
                ui.selectable_value(&mut brush.mode, PaintMode::Smooth, "≈ Smooth");
            }
            ui.selectable_value(&mut brush.mode, PaintMode::Erase, "⌫ Erase")
                .on_hover_text(match brush.target {
                    PaintTarget::Texture => "brush paint back to the original texture",
                    PaintTarget::Vertex => "brush vertex paint back to neutral",
                });
            ui.selectable_value(&mut brush.mode, PaintMode::Sample, "⛶ Sample");
        });

        if !matches!(brush.mode, PaintMode::Smooth | PaintMode::Erase) {
            ui.horizontal(|ui| {
                ui.label("color");
                ui.color_edit_button_rgb(&mut brush.color);
                if ui.button("⬜").on_hover_text("white — the identity (erases paint)").clicked() {
                    brush.color = [1.0, 1.0, 1.0];
                }
            });
        }
        // Logarithmic + a far higher cap: 4.0 couldn't cover a large prop, but a linear
        // slider to 100 would make small brushes unpickable.
        ui.add(
            egui::Slider::new(&mut brush.radius, 0.005..=100.0)
                .logarithmic(true)
                .text("radius"),
        );
        ui.add(egui::Slider::new(&mut brush.strength, 0.01..=1.0).text("strength"));

        if brush.mode == PaintMode::Paint {
            ui.horizontal(|ui| {
                ui.label("blend");
                egui::ComboBox::from_id_salt("paint_blend")
                    .selected_text(brush.blend.label())
                    .show_ui(ui, |ui| {
                        for b in BlendMode::ALL {
                            ui.selectable_value(&mut brush.blend, b, b.label());
                        }
                    });
            });
        }
        crate::terrain_ui::brush_profile_ui(ui, &mut brush.profile, &mut brush.spacing);

        ui.collapsing("Advanced", |ui| {
            ui.horizontal(|ui| {
                ui.label("write");
                ui.checkbox(&mut brush.channels[0], "R");
                ui.checkbox(&mut brush.channels[1], "G");
                ui.checkbox(&mut brush.channels[2], "B");
                ui.checkbox(&mut brush.channels[3], "A");
            });
            ui.add(egui::Slider::new(&mut brush.alpha, 0.0..=1.0).text("alpha"));
            ui.checkbox(&mut brush.backfaces, "paint back faces")
                .on_hover_text("also paint vertices facing away from the camera");
        });

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Fill selected")
                .on_hover_text(match (brush.target, brush.mode == PaintMode::Erase) {
                    (PaintTarget::Texture, true) => {
                        "⌫ Erase is active: fade ALL paint by the brush strength (full strength removes it)"
                    }
                    (PaintTarget::Texture, false) => {
                        "flood the node at the brush STRENGTH — 1.0 is solid, lower lays a translucent wash"
                    }
                    (PaintTarget::Vertex, true) => {
                        "⌫ Erase is active: flood the selected node back to neutral (unpainted)"
                    }
                    (PaintTarget::Vertex, false) => "flood the whole selected node with the brush color",
                })
                .clicked()
            {
                cmd.paint_fill = true;
            }
            if ui
                .button("Clear selected")
                .on_hover_text(match brush.target {
                    PaintTarget::Texture => "remove the painted texture (back to the material's own texture)",
                    PaintTarget::Vertex => "remove all vertex paint from the selected node",
                })
                .clicked()
            {
                cmd.paint_clear = true;
            }
        });
        match brush.target {
            PaintTarget::Vertex => {
                ui.small("Vertex paint lives in <project>/paint/<scene>.vpaint.");
            }
            PaintTarget::Texture => {
                ui.small("Paint overlays the node's own look (which stays pixel-exact). Ctrl+Z");
                ui.small("undoes strokes; saved to <project>/paint/<scene>.tpaint on Save.");
            }
        }
    }
}
