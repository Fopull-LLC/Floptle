//! The ⚙ Settings dock tab — everything project-wide, one section at a time.
//!
//! This was a fixed-size modal window with four topics stacked into a 430 px
//! column, which meant scrolling past Rendering to reach Layers and squinting
//! at binding chips. It's now a real tab (drag it anywhere, split it beside the
//! viewport, close it) laid out as **nav on the left, one section on the
//! right** — the shape every settings screen uses, because it keeps each topic
//! short enough to read.
//!
//! The search box spans every section: type "gravity" or "jump" and you get the
//! matching rows wherever they live, with their section named, instead of
//! having to know which topic owns a setting.

use crate::icons;

/// One topic in the left-hand nav.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SettingsSection {
    #[default]
    Game,
    Rendering,
    Layers,
    Input,
}

impl SettingsSection {
    pub(crate) const ALL: &'static [SettingsSection] = &[
        SettingsSection::Game,
        SettingsSection::Rendering,
        SettingsSection::Layers,
        SettingsSection::Input,
    ];

    fn title(self) -> &'static str {
        match self {
            SettingsSection::Game => "Game",
            SettingsSection::Rendering => "Rendering",
            SettingsSection::Layers => "Layers",
            SettingsSection::Input => "Input",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsSection::Game => icons::PLAY,
            SettingsSection::Rendering => icons::SHADERS,
            SettingsSection::Layers => icons::MAP,
            SettingsSection::Input => icons::KEYBOARD,
        }
    }

    /// One line under the section heading — what this topic is *for*.
    fn blurb(self) -> &'static str {
        match self {
            SettingsSection::Game => "What a build ships as.",
            SettingsSection::Rendering => "Applies to every scene in the project.",
            SettingsSection::Layers => "Collision and query groups.",
            SettingsSection::Input => {
                "Named actions your scripts read, and the keys, mouse buttons \
                 and gamepad controls that trigger them."
            }
        }
    }

    /// Extra words the search should match beyond the visible row labels —
    /// what someone would actually *type* looking for this topic.
    fn keywords(self) -> &'static str {
        match self {
            SettingsSection::Game => "title name entry scene boot build export ships",
            SettingsSection::Rendering => "retro pixel resolution matter sdf post bloom vignette",
            SettingsSection::Layers => "collision matrix physics raycast group mask",
            SettingsSection::Input => {
                "action axis binding key keyboard mouse gamepad pad controller \
                 jump move look bind rebind deadzone socd motion buffer player"
            }
        }
    }
}

/// Case-insensitive substring match; an empty query matches everything.
pub(crate) fn matches(query: &str, haystack: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    q.is_empty() || haystack.to_ascii_lowercase().contains(&q)
}

/// A section heading with its explanatory line.
fn heading(ui: &mut egui::Ui, section: SettingsSection) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(section.icon()).size(16.0));
        ui.label(egui::RichText::new(section.title()).size(16.0).strong());
    });
    ui.label(egui::RichText::new(section.blurb()).weak());
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(6.0);
}

/// A labelled settings row: a fixed-width label on the left, the control on the
/// right. Consistent widths are what stop a settings page reading as a jumble.
pub(crate) fn row<R>(
    ui: &mut egui::Ui,
    label: &str,
    help: Option<&str>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut out = None;
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        let l = ui.add_sized([120.0, 20.0], egui::Label::new(label).selectable(false));
        if let Some(h) = help {
            l.on_hover_text(h);
        }
        out = Some(add(ui));
    });
    out.expect("row body runs")
}

/// Everything the Settings tab reads or writes, as borrows.
///
/// The tab renders from inside `EditorTabViewer`, which holds disjoint field
/// borrows rather than `&mut Editor` (the GPU and egui state are borrowed
/// elsewhere for the frame). So the tab takes exactly what it needs and reports
/// changes back through [`SettingsOut`], applied after the frame — the same
/// deferral every other panel here uses.
pub(crate) struct SettingsCtx<'a> {
    pub(crate) scene_files: &'a [String],
    pub(crate) layer_new: &'a mut String,
    pub(crate) section: &'a mut SettingsSection,
    pub(crate) search: &'a mut String,
    pub(crate) input_map: &'a floptle_input::InputMap,
    pub(crate) input_pending: Option<&'a floptle_input::PendingRebind>,
    pub(crate) input_scan: &'a crate::input_scan::InputScan,
    pub(crate) input_test: &'a floptle_input::ActionState,
    pub(crate) pad_names: &'a [Option<String>],
    pub(crate) input_new_action: &'a mut String,
}

/// What the tab changed, applied after the frame.
#[derive(Default)]
pub(crate) struct SettingsOut {
    pub(crate) save_project: bool,
    pub(crate) rename_layer: Option<(String, String)>,
    pub(crate) input: crate::input_ui::InputEdits,
}

impl<'a> SettingsCtx<'a> {
    /// The ⚙ Settings tab.
    pub(crate) fn ui(
        &mut self,
        ui: &mut egui::Ui,
        project: &mut floptle_scene::ProjectConfigDoc,
    ) -> SettingsOut {
        let mut out = SettingsOut::default();
        ui.add_space(4.0);

        // --- search across every section --------------------------------
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let resp = ui.add_sized(
                [ui.available_width().min(320.0), 22.0],
                egui::TextEdit::singleline(self.search)
                    .hint_text("search all settings…"),
            );
            if !self.search.is_empty() {
                if ui.small_button("✖").on_hover_text("clear the search").clicked() {
                    self.search.clear();
                }
                // Escape clears too, but only while the box has focus — it must
                // not steal Escape from a rebind prompt or the viewport.
                if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.search.clear();
                }
            }
        });
        ui.add_space(6.0);

        let query = self.search.trim().to_string();
        let searching = !query.is_empty();

        // Which sections have anything to show for this query.
        let hits: Vec<SettingsSection> = SettingsSection::ALL
            .iter()
            .copied()
            .filter(|s| {
                !searching
                    || matches(&query, s.title())
                    || matches(&query, s.blurb())
                    || matches(&query, s.keywords())
            })
            .collect();

        // Searching to nothing is worth SAYING — an empty pane with no
        // explanation reads as a broken panel.
        if searching && hits.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label(egui::RichText::new(format!("no settings match “{query}”")).weak());
            });
            return out;
        }

        // Keep the selection sensible while searching: if the current section
        // has no hits, follow the search to one that does.
        if searching && !hits.contains(&*self.section) {
            *self.section = hits[0];
        }

        let selected = *self.section;
        let mut want_section = selected;

        // A manual two-column split rather than nested panels: this tab docks
        // anywhere, and a panel inside a dock leaf fights the leaf's own layout
        // when the tab is narrow.
        ui.horizontal_top(|ui| {
            let col_h = ui.available_height();
            ui.allocate_ui_with_layout(
                egui::vec2(146.0, col_h),
                egui::Layout::top_down_justified(egui::Align::Min),
                |ui| {
                    ui.add_space(2.0);
                    for sec in SettingsSection::ALL.iter().copied() {
                        let is_hit = hits.contains(&sec);
                        let label = format!("{}  {}", sec.icon(), sec.title());
                        let text = if is_hit {
                            egui::RichText::new(label)
                        } else {
                            egui::RichText::new(label).weak()
                        };
                        let resp = ui.selectable_label(sec == selected, text);
                        if resp.clicked() {
                            want_section = sec;
                        }
                        resp.on_hover_text(sec.blurb());
                        ui.add_space(2.0);
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("project.ron").weak().small());
                    if selected == SettingsSection::Input {
                        ui.label(egui::RichText::new("input.ron").weak().small());
                    }
                },
            );
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("settings_body")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    // Breathing room down the left of the content column.
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            heading(ui, selected);
                            match selected {
                                SettingsSection::Game => self.settings_game(ui, project, &mut out),
                                SettingsSection::Rendering => {
                                    self.settings_rendering(ui, project, &mut out)
                                }
                                SettingsSection::Layers => self.settings_layers(ui, project, &mut out),
                                SettingsSection::Input => self.settings_input(ui, &query, &mut out),
                            }
                            ui.add_space(16.0);
                        });
                    });
                });
        });

        *self.section = want_section;
        out
    }

    // --- Game -----------------------------------------------------------
    fn settings_game(&mut self, ui: &mut egui::Ui, project: &mut floptle_scene::ProjectConfigDoc, out: &mut SettingsOut) {

        row(ui, "Title", Some("names the exported binary and its window"), |ui| {
            let mut t = project.title.clone().unwrap_or_default();
            if ui
                .add_sized([220.0, 20.0], egui::TextEdit::singleline(&mut t).hint_text("My Game"))
                .changed()
            {
                project.title = (!t.trim().is_empty()).then_some(t);
                out.save_project = true;
            }
        });

        let stem = |s: &str| {
            std::path::Path::new(s)
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| s.to_string())
        };
        row(ui, "Entry scene", Some("the scene a build boots into"), |ui| {
            let current =
                project.entry_scene.clone().unwrap_or_else(|| "scenes/first.ron".into());
            egui::ComboBox::from_id_salt("entry_scene_pick")
                .width(220.0)
                .selected_text(stem(&current))
                .show_ui(ui, |ui| {
                    for s in self.scene_files {
                        if ui.selectable_label(current == *s, stem(s)).on_hover_text(s).clicked() {
                            project.entry_scene = Some(s.clone());
                            out.save_project = true;
                        }
                    }
                });
        });
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(
                "The editor opens this scene on project load too, so what you see is what ships.",
            )
            .weak()
            .small(),
        );

    }

    // --- Rendering ------------------------------------------------------
    fn settings_rendering(&mut self, ui: &mut egui::Ui, project: &mut floptle_scene::ProjectConfigDoc, out: &mut SettingsOut) {

        row(ui, "Retro", Some("render at a low resolution and upscale"), |ui| {
            out.save_project |= ui.checkbox(&mut project.retro, "pixelization").changed();
        });
        ui.add_enabled_ui(project.retro, |ui| {
            row(ui, "Pixel rows", Some("vertical resolution before the upscale"), |ui| {
                out.save_project |= ui
                    .add_sized(
                        [220.0, 20.0],
                        egui::Slider::new(&mut project.retro_height, 80u32..=1080),
                    )
                    .changed();
            });
        });
        row(ui, "Matter", Some("the SDF raymarched geometry pass"), |ui| {
            out.save_project |= ui.checkbox(&mut project.matter, "SDF matter").changed();
        });

        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(
                "Post-processing (bloom, vignette, ambient occlusion) is per-scene: select the \
                 Post Processing node in the Hierarchy.",
            )
            .weak()
            .small(),
        );

    }

    // --- Layers ---------------------------------------------------------
    fn settings_layers(&mut self, ui: &mut egui::Ui, project: &mut floptle_scene::ProjectConfigDoc, out: &mut SettingsOut) {

        ui.label(
            egui::RichText::new(
                "Nodes pick a layer in the Inspector; scripts read node.layer and filter \
                 raycasts by name. \"Default\" always exists.",
            )
            .weak()
            .small(),
        );
        ui.add_space(8.0);

        let mut remove_idx: Option<usize> = None;
        for i in 0..project.layers.len() {
            ui.horizontal(|ui| {
                ui.set_min_height(24.0);
                let before = project.layers[i].clone();
                let resp = ui.add_sized(
                    [200.0, 20.0],
                    egui::TextEdit::singleline(&mut project.layers[i]),
                );
                if resp.changed() {
                    let after = project.layers[i].clone();
                    // The rename follows through: exception pairs here, the open
                    // scene's nodes below (per keystroke, so they never detach
                    // mid-edit). Other scene FILES keep the old name and warn at Play.
                    for (a, b) in project.no_collide.iter_mut() {
                        if *a == before {
                            *a = after.clone();
                        }
                        if *b == before {
                            *b = after.clone();
                        }
                    }
                    out.rename_layer = Some((before, after));
                    out.save_project = true;
                }
                // Removal is destructive and NOT undoable, so it's two clicks.
                let arm_id = egui::Id::new("layer-delete-armed");
                let armed: Option<usize> = ui.ctx().data(|d| d.get_temp(arm_id)).flatten();
                if armed == Some(i) {
                    ui.label(egui::RichText::new("delete?").weak());
                    if ui
                        .small_button("✔")
                        .on_hover_text(
                            "yes, remove it — nodes still naming it act as Default (and warn at Play)",
                        )
                        .clicked()
                    {
                        remove_idx = Some(i);
                        ui.ctx().data_mut(|d| d.insert_temp(arm_id, None::<usize>));
                    }
                    if ui.small_button("✖").clicked() {
                        ui.ctx().data_mut(|d| d.insert_temp(arm_id, None::<usize>));
                    }
                } else if ui
                    .small_button(icons::REMOVE)
                    .on_hover_text("remove this layer (asks to confirm)")
                    .clicked()
                {
                    ui.ctx().data_mut(|d| d.insert_temp(arm_id, Some(i)));
                }
            });
        }
        if let Some(i) = remove_idx {
            let name = project.layers.remove(i);
            project.no_collide.retain(|(a, b)| *a != name && *b != name);
            out.save_project = true;
        }

        let resolved = project.build_layers();
        let full = resolved.names.len() >= floptle_core::layers::MAX_LAYERS;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let resp = ui.add_sized(
                [200.0, 20.0],
                egui::TextEdit::singleline(&mut *self.layer_new).hint_text("new layer…"),
            );
            let commit = (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || ui.small_button(icons::ADD).clicked();
            if commit && !self.layer_new.trim().is_empty() {
                let name = self.layer_new.trim().to_string();
                if !full && resolved.index_of(&name).is_none() {
                    project.layers.push(name);
                    out.save_project = true;
                }
                self.layer_new.clear();
            }
            if full {
                ui.label(egui::RichText::new("32-layer max").weak());
            }
        });

        // The collision matrix.
        let resolved = project.build_layers();
        if resolved.names.len() > 1 {
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Collision matrix").strong());
            ui.label(
                egui::RichText::new(
                    "An unchecked pair passes through each other. Unfiltered rays still hit \
                     everything — rays only filter when a script asks.",
                )
                .weak()
                .small(),
            );
            ui.add_space(6.0);
            egui::Grid::new("layer_matrix").spacing([6.0, 4.0]).show(ui, |ui| {
                ui.label("");
                for (j, name) in resolved.names.iter().enumerate() {
                    ui.label(egui::RichText::new(format!("{j}")).weak()).on_hover_text(name);
                }
                ui.end_row();
                for (i, a) in resolved.names.iter().enumerate() {
                    ui.label(format!("{i}  {a}"));
                    for (j, b) in resolved.names.iter().enumerate() {
                        if j < i {
                            ui.label("");
                            continue;
                        }
                        let mut on = resolved.collides(i as u8, j as u8);
                        if ui.checkbox(&mut on, "").on_hover_text(format!("{a} × {b}")).changed() {
                            if on {
                                project.no_collide.retain(|(x, y)| {
                                    !((x == a && y == b) || (x == b && y == a))
                                });
                            } else {
                                project.no_collide.push((a.clone(), b.clone()));
                            }
                            out.save_project = true;
                        }
                    }
                    ui.end_row();
                }
            });
        }

    }

    // --- Input ----------------------------------------------------------
    fn settings_input(&mut self, ui: &mut egui::Ui, query: &str, out: &mut SettingsOut) {
        out.input = crate::input_ui::input_section(
            ui,
            self.input_map,
            self.input_pending,
            self.input_scan,
            self.input_test,
            self.pad_names,
            self.input_new_action,
            query,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_case_insensitive_and_empty_matches_all() {
        assert!(matches("", "anything"));
        assert!(matches("GAMEPAD", "keyboard gamepad pad"));
        assert!(matches("pad", "GAMEPAD"));
        assert!(!matches("zzz", "keyboard gamepad"));
    }

    /// Searching for the words people actually type must land on a section —
    /// a search box that finds nothing is worse than no search box.
    #[test]
    fn common_searches_find_their_section() {
        let find = |q: &str| {
            SettingsSection::ALL.iter().copied().find(|s| {
                matches(q, s.title()) || matches(q, s.blurb()) || matches(q, s.keywords())
            })
        };
        for (q, want) in [
            ("gamepad", SettingsSection::Input),
            ("controller", SettingsSection::Input),
            ("rebind", SettingsSection::Input),
            ("deadzone", SettingsSection::Input),
            ("jump", SettingsSection::Input),
            ("retro", SettingsSection::Rendering),
            ("pixel", SettingsSection::Rendering),
            ("collision", SettingsSection::Layers),
            ("raycast", SettingsSection::Layers),
            ("title", SettingsSection::Game),
            ("entry", SettingsSection::Game),
        ] {
            assert_eq!(find(q), Some(want), "searching {q:?}");
        }
    }

    /// Render every section headlessly.
    ///
    /// UI code has failure modes the type system won't catch — a duplicate
    /// widget id, an out-of-range index, an `unwrap` on an empty list — and
    /// they only show up when the panel is actually drawn. This draws all of
    /// them, twice (egui needs a second frame for anything that reads last
    /// frame's state), against a map that exercises the awkward cases:
    /// unbound entries, a script referencing something undefined, and a live
    /// tester with no pad connected.
    #[test]
    fn every_section_renders_without_panicking() {
        let ctx = crate::icons::test_context();
        let mut project = floptle_scene::ProjectConfigDoc::ps1();
        project.layers = vec!["Ground".into(), "Ghosts".into()];
        project.no_collide = vec![("Ground".into(), "Ghosts".into())];

        let mut map = floptle_input::InputMap::starter();
        // An action with nothing bound — the case the UI must call out rather
        // than silently draw as normal.
        map.actions.push(floptle_input::Action::new("Unbound"));

        let scan = crate::input_scan::InputScan::default();
        let test_state = floptle_input::ActionState::default();
        let pad_names: Vec<Option<String>> = vec![None; 4];
        let scene_files = vec!["scenes/first.ron".to_string()];

        for section in SettingsSection::ALL.iter().copied() {
            for search in ["", "jump", "zzz-no-match"] {
                let mut layer_new = String::new();
                let mut new_action = String::new();
                let mut sec = section;
                let mut query = search.to_string();
                // Two frames: the first lays out, the second reads back state.
                for _ in 0..2 {
                    let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
                        let mut cx = SettingsCtx {
                            scene_files: &scene_files,
                            layer_new: &mut layer_new,
                            section: &mut sec,
                            search: &mut query,
                            input_map: &map,
                            input_pending: None,
                            input_scan: &scan,
                            input_test: &test_state,
                            pad_names: &pad_names,
                            input_new_action: &mut new_action,
                        };
                        let _ = cx.ui(ui, &mut project);
                    });
                }
            }
        }
    }

    /// Collect every string egui painted this frame.
    ///
    /// egui reports a duplicate widget id by PAINTING an error over the
    /// offending widget rather than logging it, so reading the paint list is
    /// how a test can see one.
    fn painted_text(output: &egui::FullOutput) -> String {
        fn walk(shape: &egui::epaint::Shape, out: &mut String) {
            match shape {
                egui::epaint::Shape::Text(t) => {
                    out.push_str(t.galley.text());
                    out.push('\n');
                }
                egui::epaint::Shape::Vec(v) => {
                    for s in v {
                        walk(s, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = String::new();
        for cs in &output.shapes {
            walk(&cs.shape, &mut out);
        }
        out
    }

    /// The Input section actually draws its rows.
    ///
    /// NOTE on what this does *not* cover: the popup bug (binding menus and the
    /// SOCD dropdown snapping shut on click) was an egui **widget-id
    /// collision** — every action row built its picker from the same label, so
    /// all the rows' menus shared one id and fought over which was open. The
    /// fix is the `push_id` namespacing in `input_ui.rs`. egui reports a clash
    /// by painting over the offending widget, and that painting does not happen
    /// in a headless pass, so there is no automated guard for it here — an
    /// attempt at one passed just as happily with the collision reintroduced,
    /// which is worse than no test. Verify popups by hand after touching row
    /// layout.
    #[test]
    fn the_input_section_draws_its_rows() {
        let ctx = crate::icons::test_context();
        let mut project = floptle_scene::ProjectConfigDoc::ps1();
        let mut map = floptle_input::InputMap::starter();
        map.actions.push(floptle_input::Action::new("Punch"));
        map.actions.push(floptle_input::Action::new("Kick"));

        let scan = crate::input_scan::InputScan::default();
        let test_state = floptle_input::ActionState::default();
        let pad_names: Vec<Option<String>> = vec![None];
        let (mut layer_new, mut new_action) = (String::new(), String::new());
        let mut sec = SettingsSection::Input;
        let mut query = String::new();

        let mut painted = String::new();
        for _ in 0..3 {
            let out = ctx.run_ui(crate::icons::test_input(), |ui| {
                let mut cx = SettingsCtx {
                    scene_files: &[],
                    layer_new: &mut layer_new,
                    section: &mut sec,
                    search: &mut query,
                    input_map: &map,
                    input_pending: None,
                    input_scan: &scan,
                    input_test: &test_state,
                    pad_names: &pad_names,
                    input_new_action: &mut new_action,
                };
                let _ = cx.ui(ui, &mut project);
            });
            painted = painted_text(&out);
        }
        // Only things above the fold: the pass paints what's VISIBLE, and the
        // live tester sits below the scroll in an 800 px viewport.
        for want in ["Jump", "Punch", "Kick", "Move", "Actions"] {
            assert!(painted.contains(want), "the Input section never drew {want:?}:\n{painted}");
        }
    }

    /// The same, with a press-to-bind armed — that path draws a different
    /// banner and is easy to break in isolation.
    #[test]
    fn the_rebind_prompt_renders() {
        let ctx = crate::icons::test_context();
        let mut project = floptle_scene::ProjectConfigDoc::ps1();
        let map = floptle_input::InputMap::starter();
        let scan = crate::input_scan::InputScan::default();
        let test_state = floptle_input::ActionState::default();
        let pad_names: Vec<Option<String>> = vec![Some("Test Pad".into())];
        let pending = floptle_input::PendingRebind {
            action: "Jump".into(),
            slot: 0,
            filter: floptle_input::BindFilter::AnyButton,
            captured: None,
        };
        let (mut layer_new, mut new_action) = (String::new(), String::new());
        let mut sec = SettingsSection::Input;
        let mut query = String::new();
        for _ in 0..2 {
            let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
                let mut cx = SettingsCtx {
                    scene_files: &[],
                    layer_new: &mut layer_new,
                    section: &mut sec,
                    search: &mut query,
                    input_map: &map,
                    input_pending: Some(&pending),
                    input_scan: &scan,
                    input_test: &test_state,
                    pad_names: &pad_names,
                    input_new_action: &mut new_action,
                };
                let _ = cx.ui(ui, &mut project);
            });
        }
    }

    #[test]
    fn every_section_has_a_blurb_and_keywords() {
        for s in SettingsSection::ALL {
            assert!(!s.blurb().is_empty(), "{s:?}");
            assert!(!s.keywords().is_empty(), "{s:?}");
            assert!(!s.title().is_empty(), "{s:?}");
        }
    }
}
