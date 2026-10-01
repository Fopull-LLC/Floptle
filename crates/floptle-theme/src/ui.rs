//! The theme settings: a picker, the effects controls, and a theme editor.
//!
//! One UI for both programs. It never touches the disk or opens a dialog
//! itself: it returns [`Action`]s and the host carries them out, because the
//! editor has native file pickers and the Hub does not, and only the host
//! knows where its packages are.

use std::sync::Arc;

use egui::{Color32, CornerRadius, RichText, Sense, Stroke, Ui, vec2};

use crate::color::Rgba;
use crate::model::{self, LayerFile, Origin, SurfaceFile, Theme, ThemeFile};
use crate::source::{self, Effects, Entry, Library, Prefs};

/// What the user asked for. The host does it.
#[derive(Clone, Debug)]
pub enum Action {
    /// Use this theme, and save the choice.
    Choose(String),
    /// The effects settings changed; save them.
    PrefsChanged,
    /// Look again at the themes folder (and packages).
    Rescan,
    /// Pick a `.floptletheme` (or a folder) to add.
    Import,
    /// Show the themes folder in the file manager.
    OpenFolder,
    /// Save the theme with this id as a `.floptletheme`, somewhere the user picks.
    Export(String),
    /// Remove a theme of yours.
    Delete(String),
    /// Show this theme while it is being edited, without choosing it.
    Preview(Arc<Theme>),
    /// Editing ended without saving: go back to the chosen theme.
    EndPreview,
    /// Save the edited theme to the themes folder and choose it.
    Save(Box<ThemeFile>, model::Assets),
    /// Pick an image for a region's backdrop in the theme editor. The host
    /// answers with [`Settings::add_image`].
    PickImage(String),
}

/// What the settings remember between frames.
#[derive(Default)]
pub struct Settings {
    editing: Option<Edit>,
    /// A line to show under the picker: an import's result, an error.
    pub notice: Option<(String, bool)>,
}

struct Edit {
    file: ThemeFile,
    assets: model::Assets,
    error: Option<String>,
    dirty: bool,
    region: String,
}

impl Settings {
    /// Is the theme editor open? (The host keeps previewing while it is.)
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// The image the host picked for `region`, after [`Action::PickImage`].
    pub fn add_image(&mut self, region: &str, file_name: &str, bytes: Vec<u8>) -> Option<Action> {
        let ed = self.editing.as_mut()?;
        let safe: String = file_name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let path = format!("images/{safe}");
        ed.assets.files.insert(path.clone(), Arc::from(bytes.into_boxed_slice()));
        let s = ed.file.surfaces.entry(region.to_string()).or_default();
        let mut layers = s.layers.clone().unwrap_or_default();
        layers.retain(|l| !matches!(l, LayerFile::Image { .. }));
        layers.push(LayerFile::Image {
            path,
            fit: model::Fit::Cover,
            align: model::Align::Center,
            scale: 1.0,
            offset: (0.0, 0.0),
            opacity: 0.35,
            tint: None,
            blend: model::Blend::Normal,
            space: model::Space::Region,
            pixelated: false,
        });
        s.layers = Some(layers);
        // A picture under a solid panel is a picture nobody sees: give the
        // veil some transparency the first time.
        if s.fill.is_none() {
            s.fill = Some("$ground/80%".into());
        }
        ed.dirty = true;
        rebuild(ed)
    }
}

/// Effects controls and what they cost right now.
pub struct Runtime {
    pub draws_per_second: u32,
    pub shader_errors: Vec<(String, String)>,
    /// Show "Hold still while the game plays" (the editor; not the Hub).
    pub show_pause_while_playing: bool,
}

/// The whole settings section. `current` is the theme being drawn.
pub fn settings(
    ui: &mut Ui,
    st: &mut Settings,
    lib: &Library,
    current: &Theme,
    prefs: &mut Prefs,
    rt: &Runtime,
) -> Vec<Action> {
    let mut out = Vec::new();
    if st.editing.is_some() {
        editor(ui, st, &mut out);
        return out;
    }

    ui.label(crate::look::label(ui, "Theme"));
    ui.add_space(4.0);
    let card_w = 176.0;
    let cols = ((ui.available_width() + 8.0) / (card_w + 8.0)).floor().max(1.0) as usize;
    egui::Grid::new("floptle-theme-cards").spacing(vec2(8.0, 8.0)).show(ui, |ui| {
        for (i, e) in lib.entries.iter().enumerate() {
            if card(ui, e, e.id == prefs.theme, card_w).clicked() && e.id != prefs.theme {
                out.push(Action::Choose(e.id.clone()));
            }
            if (i + 1) % cols == 0 {
                ui.end_row();
            }
        }
    });
    for r in &lib.refused {
        ui.label(RichText::new(format!("Not loaded: {}", r.error)).color(crate::signal::WARN).small());
    }
    if let Some(e) = lib.find(&prefs.theme) {
        for w in &e.warnings {
            ui.label(RichText::new(w).color(crate::signal::WARN).small());
        }
    }
    for (s, e) in &rt.shader_errors {
        ui.label(RichText::new(format!("{s} did not compile, so its panels show their colour:\n{e}"))
            .color(crate::signal::WARN).small().monospace());
    }

    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        if ui.button("Customize…").on_hover_text("Start a theme of your own from this one").clicked() {
            let mut file = current.file.clone();
            if matches!(current.origin, Origin::Builtin) {
                file.id = format!("{}-custom", current.id);
                file.name = format!("{} (custom)", current.name);
                file.author = None;
            }
            file.extends = None;
            st.editing = Some(Edit {
                file,
                assets: current.assets.clone(),
                error: None,
                dirty: false,
                region: "ground".into(),
            });
        }
        if ui.button("Import…").on_hover_text("Add a .floptletheme file").clicked() {
            out.push(Action::Import);
        }
        if ui.button("Export…").on_hover_text("Save this theme as a .floptletheme to share").clicked() {
            out.push(Action::Export(prefs.theme.clone()));
        }
        if ui.button("Open themes folder").clicked() {
            out.push(Action::OpenFolder);
        }
        if ui.button("Rescan").on_hover_text("Look for themes added to the folder").clicked() {
            out.push(Action::Rescan);
        }
        if let Some(e) = lib.find(&prefs.theme)
            && matches!(e.origin, Origin::User(_))
            && ui.button("Remove").on_hover_text("Delete this theme from your themes folder").clicked()
        {
            out.push(Action::Delete(e.id.clone()));
        }
    });
    if let Some((msg, bad)) = &st.notice {
        let c = if *bad { crate::signal::BAD } else { crate::signal::GOOD };
        ui.label(RichText::new(msg).color(c).small());
    }

    ui.add_space(10.0);
    ui.label(crate::look::label(ui, "Effects"));
    ui.add_space(2.0);
    let before = prefs.clone();
    ui.horizontal(|ui| {
        ui.selectable_value(&mut prefs.effects, Effects::Full, "Moving")
            .on_hover_text("Images, and shader backdrops animate");
        ui.selectable_value(&mut prefs.effects, Effects::Still, "Still")
            .on_hover_text("Images, and each backdrop drawn once and held: no cost per frame");
        ui.selectable_value(&mut prefs.effects, Effects::Off, "Off")
            .on_hover_text("Colours only: no images or backdrops");
    });
    ui.add_enabled_ui(prefs.effects != Effects::Off, |ui| {
        ui.horizontal(|ui| {
            ui.label("Backdrop resolution");
            let mut pct = (prefs.backdrop_scale * 100.0).round();
            if ui.add(egui::Slider::new(&mut pct, 25.0..=100.0).step_by(25.0).suffix("%")).changed() {
                prefs.backdrop_scale = pct / 100.0;
            }
        });
        ui.add_enabled_ui(prefs.effects == Effects::Full, |ui| {
            ui.horizontal(|ui| {
                ui.label("Backdrop rate");
                for fps in [15.0, 30.0, 60.0] {
                    ui.selectable_value(&mut prefs.backdrop_fps, fps, format!("{fps:.0} fps"));
                }
            });
        });
        if rt.show_pause_while_playing {
            ui.checkbox(&mut prefs.pause_while_playing, "Hold backdrops still while the game plays");
        }
    });
    let cost = if prefs.effects == Effects::Off || !current.has_effects() {
        "This theme draws no imagery.".to_string()
    } else if rt.draws_per_second == 0 {
        "Backdrops are holding still: no cost per frame.".to_string()
    } else {
        format!("{} backdrop draws a second, at {:.0}% resolution.", rt.draws_per_second, prefs.backdrop_scale * 100.0)
    };
    ui.label(crate::look::fine(ui, cost));
    if *prefs != before {
        out.push(Action::PrefsChanged);
    }
    out
}

/// One theme's card: a small picture of it, drawn in its own colours.
fn card(ui: &mut Ui, e: &Entry, selected: bool, w: f32) -> egui::Response {
    let h = 116.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let host = crate::paint::theme(ui.ctx());
    let p = ui.painter_at(rect.expand(2.0));
    let [ground, surface, text, accent, accent_hi] = e.swatch.map(Rgba::to_egui);
    let r = CornerRadius::same(host.shape.radius.round() as u8);
    let edge = if selected {
        Stroke::new(2.0, host.tokens.accent.to_egui())
    } else if resp.hovered() {
        Stroke::new(1.0, host.tokens.accent_edge.to_egui())
    } else {
        Stroke::new(1.0, host.tokens.hairline.to_egui())
    };
    // The picture: ground, a panel in its surface, lines of its text, its
    // primary button, at its own corner radius.
    let pic = egui::Rect::from_min_size(rect.min, vec2(w, 70.0));
    let pr = CornerRadius { nw: r.nw, ne: r.ne, sw: 0, se: 0 };
    p.rect_filled(pic, pr, ground);
    if e.effects {
        // A backdrop theme: its accents washing in from one corner, the way
        // its picture glows behind the panels.
        let mut m = egui::Mesh::default();
        let clear = Color32::TRANSPARENT;
        m.colored_vertex(pic.left_top(), clear);
        m.colored_vertex(pic.right_top(), accent_hi.gamma_multiply(0.30));
        m.colored_vertex(pic.right_bottom(), accent.gamma_multiply(0.45));
        m.colored_vertex(pic.left_bottom(), clear);
        m.add_triangle(0, 1, 2);
        m.add_triangle(0, 2, 3);
        p.add(egui::Shape::mesh(m));
    }
    // A theme that ships its own picture shows that instead of the sketch.
    if let Some(tex) = e.preview.as_ref().and_then(|b| preview_texture(ui.ctx(), &e.id, b)) {
        let size = tex.size_vec2();
        let s = (pic.width() / size.x).max(pic.height() / size.y);
        let shown = size * s;
        let uv = egui::Rect::from_center_size(egui::pos2(0.5, 0.5), egui::vec2(pic.width() / shown.x, pic.height() / shown.y));
        let mut m = egui::Mesh::with_texture(tex.id());
        m.add_rect_with_uv(pic, uv, Color32::WHITE);
        p.add(egui::Shape::mesh(m));
        p.rect_stroke(rect, r, edge, egui::StrokeKind::Inside);
        p.line_segment([pic.left_bottom(), pic.right_bottom()], Stroke::new(1.0, host.tokens.hairline.to_egui()));
        return card_label(ui, &p, e, rect, pic, resp, &host);
    }
    let tr = CornerRadius::same(e.radius.min(6.0).round() as u8);
    let panel = egui::Rect::from_min_size(pic.min + vec2(10.0, 10.0), vec2(w * 0.58, 50.0));
    p.rect(panel, tr, surface, Stroke::new(1.0, text.gamma_multiply(0.12)), egui::StrokeKind::Inside);
    for (i, f) in [0.75f32, 0.55, 0.65].iter().enumerate() {
        let y = panel.top() + 10.0 + i as f32 * 9.0;
        p.line_segment(
            [egui::pos2(panel.left() + 8.0, y), egui::pos2(panel.left() + 8.0 + (panel.width() - 16.0) * f, y)],
            Stroke::new(2.5, text.gamma_multiply(if i == 0 { 0.9 } else { 0.45 })),
        );
    }
    let btn = egui::Rect::from_min_size(egui::pos2(panel.right() + 8.0, panel.bottom() - 16.0), vec2(w - panel.width() - 28.0, 16.0));
    p.rect_filled(btn, tr, accent);
    p.rect_stroke(rect, r, edge, egui::StrokeKind::Inside);
    p.line_segment([pic.left_bottom(), pic.right_bottom()], Stroke::new(1.0, host.tokens.hairline.to_egui()));

    card_label(ui, &p, e, rect, pic, resp, &host)
}

/// A theme's preview picture as a texture, decoded once per theme.
fn preview_texture(ctx: &egui::Context, id: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let key = egui::Id::new(("floptle-theme-preview", id, bytes.len()));
    if let Some(t) = ctx.data(|d| d.get_temp::<Option<egui::TextureHandle>>(key)) {
        return t;
    }
    let tex = image::load_from_memory(bytes).ok().map(|i| {
        let i = i.thumbnail(512, 512).to_rgba8();
        let img = egui::ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw());
        ctx.load_texture(format!("theme-preview:{id}"), img, egui::TextureOptions::LINEAR)
    });
    ctx.data_mut(|d| d.insert_temp(key, tex.clone()));
    tex
}

fn card_label(
    _ui: &Ui,
    p: &egui::Painter,
    e: &Entry,
    rect: egui::Rect,
    pic: egui::Rect,
    resp: egui::Response,
    host: &Theme,
) -> egui::Response {
    // Its name, and where it is from.
    let text_col = host.tokens.text.to_egui();
    let dim = host.tokens.dim.to_egui();
    p.text(
        egui::pos2(rect.left() + 10.0, pic.bottom() + 8.0),
        egui::Align2::LEFT_TOP,
        &e.name,
        egui::FontId::proportional(13.0),
        text_col,
    );
    let from = match &e.origin {
        Origin::Builtin => "built in".to_string(),
        Origin::User(_) => "yours".to_string(),
        Origin::Package { package, .. } => package.clone(),
    };
    let mut tags = vec![from];
    if e.animated {
        tags.push("moving".into());
    } else if e.effects {
        tags.push("images".into());
    }
    if !e.warnings.is_empty() {
        tags.push("low contrast".into());
    }
    p.text(
        egui::pos2(rect.left() + 10.0, pic.bottom() + 27.0),
        egui::Align2::LEFT_TOP,
        tags.join(" · "),
        egui::FontId::monospace(10.5),
        dim,
    );
    let hover = e.description.clone().unwrap_or_else(|| e.name.clone());
    resp.on_hover_text(hover).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Resolve the edited file for a preview, or say why it cannot be.
fn rebuild(ed: &mut Edit) -> Option<Action> {
    let parent = source::load(&source::Source::Builtin(source::DEFAULT_ID), Origin::Builtin, false)
        .map(|t| t.file)
        .ok()?;
    let merged = model::merge(&ed.file, &parent);
    match model::resolve(&merged, "theme editor", ed.assets.clone(), Origin::Builtin) {
        Ok(t) => {
            ed.error = None;
            Some(Action::Preview(Arc::new(t)))
        }
        Err(e) => {
            ed.error = Some(e.message);
            None
        }
    }
}

const TOKEN_HELP: &[(&str, &str)] = &[
    ("ground", "the window behind everything"),
    ("surface", "a panel, a card, a window"),
    ("surface_2", "a header row, a chip, a resting button"),
    ("well", "text fields and code: a step below the ground"),
    ("hairline", "the 1px line between surfaces"),
    ("hairline_quiet", "a rule inside a panel"),
    ("text", "the words"),
    ("dim", "secondary text"),
    ("faint", "labels and fine print"),
    ("accent", "the one primary action, on, focus, selection"),
    ("accent_hi", "links, hover on the accent"),
    ("accent_wash", "a selected row's background"),
    ("accent_edge", "the border of something hovered or on"),
    ("on_accent", "text on a filled accent button"),
    ("backdrop", "what is under a translucent panel"),
    ("selection", "selected text"),
];

fn editor(ui: &mut Ui, st: &mut Settings, out: &mut Vec<Action>) {
    let Some(ed) = st.editing.as_mut() else { return };
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(crate::look::section(ui, "Theme editor"));
        ui.label(crate::look::fine(ui, "changes show as you make them"));
    });
    ui.add_space(4.0);
    egui::Grid::new("theme-ed-meta").num_columns(2).spacing(vec2(10.0, 4.0)).show(ui, |ui| {
        ui.label("Name");
        changed |= ui.text_edit_singleline(&mut ed.file.name).changed();
        ui.end_row();
        ui.label("Id");
        let r = ui.add(egui::TextEdit::singleline(&mut ed.file.id).font(egui::TextStyle::Monospace));
        if r.changed() {
            ed.file.id = ed.file.id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        }
        ui.end_row();
        ui.label("Author");
        let mut a = ed.file.author.clone().unwrap_or_default();
        if ui.text_edit_singleline(&mut a).changed() {
            ed.file.author = (!a.trim().is_empty()).then_some(a);
        }
        ui.end_row();
        ui.label("Dark");
        let mut d = ed.file.dark.unwrap_or(true);
        if ui.checkbox(&mut d, "a dark theme").changed() {
            ed.file.dark = Some(d);
            changed = true;
        }
        ui.end_row();
    });

    egui::CollapsingHeader::new("Colours").default_open(true).show(ui, |ui| {
        egui::Grid::new("theme-ed-colors").num_columns(3).spacing(vec2(8.0, 3.0)).show(ui, |ui| {
            for (tok, help) in TOKEN_HELP {
                ui.label(RichText::new(*tok).monospace());
                let cur = ed.file.colors.get(tok).cloned().unwrap_or_default();
                let mut c = resolve_preview(&ed.file, &cur).to_egui();
                if ui.color_edit_button_srgba(&mut c).changed() {
                    ed.file.colors.set(tok, Some(Rgba::from_egui(c).to_hex()));
                    changed = true;
                }
                ui.label(crate::look::fine(ui, *help));
                ui.end_row();
            }
        });
    });

    egui::CollapsingHeader::new("Type").show(ui, |ui| {
        egui::Grid::new("theme-ed-type").num_columns(2).spacing(vec2(10.0, 4.0)).show(ui, |ui| {
            for (key, label) in [("ui", "Interface"), ("mono", "Code and data"), ("display", "Titles")] {
                ui.label(label);
                let slot = match key {
                    "ui" => &mut ed.file.fonts.ui,
                    "mono" => &mut ed.file.fonts.mono,
                    _ => &mut ed.file.fonts.display,
                };
                let cur = slot.clone().unwrap_or_default();
                egui::ComboBox::from_id_salt(("theme-ed-font", key)).selected_text(&cur).show_ui(ui, |ui| {
                    for f in crate::fonts::BUILTIN_FACES {
                        if ui.selectable_label(cur == *f, *f).clicked() {
                            *slot = Some((*f).to_string());
                            changed = true;
                        }
                    }
                });
                ui.end_row();
            }
            ui.label("Text size");
            let mut size = ed.file.fonts.size.unwrap_or(13.0);
            if ui.add(egui::Slider::new(&mut size, 10.0..=20.0).step_by(0.5).suffix(" pt")).changed() {
                ed.file.fonts.size = Some(size);
                changed = true;
            }
            ui.end_row();
            ui.label("Text weight");
            let mut w = ed.file.fonts.ui_weight.unwrap_or(400.0);
            if ui.add(egui::Slider::new(&mut w, 300.0..=600.0).step_by(10.0)).changed() {
                ed.file.fonts.ui_weight = Some(w);
                changed = true;
            }
            ui.end_row();
            ui.label("Title weight");
            let mut w = ed.file.fonts.display_weight.unwrap_or(640.0);
            if ui.add(egui::Slider::new(&mut w, 300.0..=800.0).step_by(10.0)).changed() {
                ed.file.fonts.display_weight = Some(w);
                changed = true;
            }
            ui.end_row();
        });
    });

    egui::CollapsingHeader::new("Shape").show(ui, |ui| {
        egui::Grid::new("theme-ed-shape").num_columns(2).spacing(vec2(10.0, 4.0)).show(ui, |ui| {
            let mut slider = |ui: &mut Ui, label: &str, v: &mut Option<f32>, d: f32, r: std::ops::RangeInclusive<f32>, step: f64| {
                ui.label(label);
                let mut x = v.unwrap_or(d);
                if ui.add(egui::Slider::new(&mut x, r).step_by(step)).changed() {
                    *v = Some(x);
                    changed = true;
                }
                ui.end_row();
            };
            slider(ui, "Corner radius", &mut ed.file.shape.radius, 6.0, 0.0..=16.0, 1.0);
            slider(ui, "Small radius", &mut ed.file.shape.radius_small, 4.0, 0.0..=12.0, 1.0);
            slider(ui, "Field radius", &mut ed.file.shape.widget_radius, 4.0, 0.0..=12.0, 1.0);
            slider(ui, "Line width", &mut ed.file.shape.stroke, 1.0, 0.0..=3.0, 0.5);
            slider(ui, "Spacing", &mut ed.file.shape.density, 1.0, 0.7..=1.4, 0.05);
            slider(ui, "Animation (ms)", &mut ed.file.motion.animation_ms, 120.0, 0.0..=400.0, 10.0);
            ui.label("Window shadow");
            let mut on = ed.file.shape.shadow.is_some();
            if ui.checkbox(&mut on, "soft shadow under windows").changed() {
                ed.file.shape.shadow = on.then(model::ShadowFile::default);
                changed = true;
            }
            ui.end_row();
        });
    });

    egui::CollapsingHeader::new("Backdrops").show(ui, |ui| {
        ui.label(crate::look::fine(ui, "Each region shows its own backdrop, or its parent's when it has none: a tab shows the panel's, which shows the ground's."));
        ui.horizontal(|ui| {
            ui.label("Region");
            egui::ComboBox::from_id_salt("theme-ed-region").selected_text(&ed.region).show_ui(ui, |ui| {
                for (r, _, help) in model::REGIONS {
                    ui.selectable_value(&mut ed.region, (*r).to_string(), *r).on_hover_text(*help);
                }
                for tab in ["tab.inspector", "tab.hierarchy", "tab.assets", "tab.console", "tab.scripting", "tab.packages"] {
                    ui.selectable_value(&mut ed.region, tab.to_string(), tab);
                }
            });
        });
        let region = ed.region.clone();
        let s = ed.file.surfaces.entry(region.clone()).or_insert_with(SurfaceFile::default);
        ui.horizontal(|ui| {
            ui.label("Shader");
            let cur = s.layers.as_ref().and_then(|l| {
                l.iter().find_map(|l| match l {
                    LayerFile::Shader { shader, .. } => Some(shader.clone()),
                    _ => None,
                })
            });
            let label = match (&s.layers, &cur) {
                (None, _) => "from parent".to_string(),
                (Some(_), None) => "none".to_string(),
                (Some(_), Some(c)) => c.clone(),
            };
            egui::ComboBox::from_id_salt("theme-ed-shader").selected_text(label).show_ui(ui, |ui| {
                if ui.selectable_label(s.layers.is_none(), "from parent").clicked() {
                    s.layers = None;
                    changed = true;
                }
                if ui.selectable_label(s.layers.is_some() && cur.is_none(), "none").clicked() {
                    let mut l = s.layers.clone().unwrap_or_default();
                    l.retain(|l| !matches!(l, LayerFile::Shader { .. }));
                    s.layers = Some(l);
                    changed = true;
                }
                for (name, _) in crate::backdrop::BUILTIN_SHADERS {
                    let id = format!("builtin:{name}");
                    if ui.selectable_label(cur.as_deref() == Some(&id), &id).clicked() {
                        let mut l = s.layers.clone().unwrap_or_default();
                        l.retain(|l| !matches!(l, LayerFile::Shader { .. }));
                        l.insert(0, LayerFile::Shader {
                            shader: id,
                            opacity: 1.0,
                            speed: 1.0,
                            scale: 1.0,
                            colors: vec![],
                            params: vec![],
                            image: None,
                            blend: model::Blend::Normal,
                        });
                        s.layers = Some(l);
                        if s.fill.is_none() {
                            s.fill = Some("$ground/70%".into());
                        }
                        changed = true;
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label("Image");
            if ui.button("Choose…").clicked() {
                out.push(Action::PickImage(region.clone()));
            }
            let has = s.layers.as_ref().is_some_and(|l| l.iter().any(|l| matches!(l, LayerFile::Image { .. })));
            if has && ui.button("Remove").clicked() {
                if let Some(l) = s.layers.as_mut() {
                    l.retain(|l| !matches!(l, LayerFile::Image { .. }));
                }
                changed = true;
            }
        });
        if let Some(layers) = s.layers.as_mut() {
            for l in layers.iter_mut() {
                match l {
                    LayerFile::Shader { shader, opacity, speed, scale, .. } => {
                        ui.label(RichText::new(shader.as_str()).monospace());
                        changed |= ui.add(egui::Slider::new(opacity, 0.0..=1.0).text("opacity")).changed();
                        changed |= ui.add(egui::Slider::new(speed, 0.0..=4.0).text("speed")).changed();
                        changed |= ui.add(egui::Slider::new(scale, 0.25..=4.0).text("scale")).changed();
                    }
                    LayerFile::Image { path, fit, align, opacity, scale, .. } => {
                        ui.label(RichText::new(path.as_str()).monospace());
                        ui.horizontal(|ui| {
                            for (f, n) in [(model::Fit::Cover, "Cover"), (model::Fit::Contain, "Contain"), (model::Fit::Tile, "Tile"), (model::Fit::Natural, "Natural")] {
                                changed |= ui.selectable_value(fit, f, n).changed();
                            }
                        });
                        ui.horizontal(|ui| {
                            for (a, n) in [(model::Align::TopLeft, "↖"), (model::Align::Center, "·"), (model::Align::BottomRight, "↘"), (model::Align::Right, "→"), (model::Align::Bottom, "↓")] {
                                changed |= ui.selectable_value(align, a, n).changed();
                            }
                        });
                        changed |= ui.add(egui::Slider::new(opacity, 0.0..=1.0).text("opacity")).changed();
                        changed |= ui.add(egui::Slider::new(scale, 0.1..=3.0).text("scale")).changed();
                    }
                    _ => {}
                }
            }
        }
        let theme_fill = s.fill.clone().unwrap_or_else(|| format!("${}", model::default_fill_token(&region)));
        ui.horizontal(|ui| {
            ui.label("Panel veil");
            let mut c = resolve_preview(&ed.file, &theme_fill).to_egui();
            if ui.color_edit_button_srgba(&mut c).changed() {
                let s = ed.file.surfaces.entry(region.clone()).or_default();
                s.fill = Some(Rgba::from_egui(c).to_hex());
                changed = true;
            }
            ui.label(crate::look::fine(ui, "lower its alpha to let the backdrop through"));
        });
    });
    // An entry the editor opened and left empty is not a setting.
    ed.file.surfaces.retain(|_, s| s.fill.is_some() || s.layers.is_some());

    if changed {
        ed.dirty = true;
        if let Some(a) = rebuild(ed) {
            out.push(a);
        }
    }
    if let Some(e) = &ed.error {
        ui.label(RichText::new(e).color(crate::signal::BAD).small());
    }
    ui.add_space(8.0);
    let mut close = false;
    ui.horizontal(|ui| {
        let ok = ed.error.is_none() && !ed.file.id.is_empty() && source::BUILTINS.iter().all(|b| b.0 != ed.file.id);
        if ui.add_enabled(ok, crate::look::primary(ui, "Save theme")).clicked() {
            out.push(Action::Save(Box::new(ed.file.clone()), ed.assets.clone()));
            close = true;
        }
        if ui.button("Cancel").clicked() {
            out.push(Action::EndPreview);
            close = true;
        }
        if source::BUILTINS.iter().any(|b| b.0 == ed.file.id) {
            ui.label(crate::look::fine(ui, "give it an id of its own to save it"));
        }
    });
    if close {
        st.editing = None;
    }
}

/// A colour string as the editor would show it: references followed against
/// the file being edited, alpha kept.
fn resolve_preview(file: &ThemeFile, s: &str) -> Rgba {
    let mut cur = s.to_string();
    let mut alpha = 1.0f32;
    for _ in 0..8 {
        match crate::color::parse(&cur) {
            Ok(crate::color::ColorExpr::Lit(c)) => return c.with_alpha_mul(alpha),
            Ok(crate::color::ColorExpr::Ref { token, alpha: a }) => {
                alpha *= a;
                let parent = crate::default_theme();
                cur = match file.colors.get(&token) {
                    Some(v) => v.clone(),
                    None => parent.tokens.get(&token).map(|c| c.to_hex()).unwrap_or_default(),
                };
            }
            Err(_) => break,
        }
    }
    Rgba(Color32::GRAY.to_srgba_unmultiplied())
}
