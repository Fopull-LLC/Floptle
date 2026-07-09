//! The 🎧 Mixer tab: DAW-style channel strips over the project's mixer graph.
//!
//! Left-to-right: Master, then every user track — each a strict top-down strip
//! like a hardware console channel: name, insert-effect rack, pan, fader +
//! meter (with mute/solo), and output routing at the bottom. Clicking an
//! insert opens its parameter editor in the right-hand panel (the parametric
//! EQ gets a draggable response-curve editor); right-clicking one gets
//! bypass / reorder / copy / paste / remove.
//!
//! Edits mutate the project's [`MixerDesc`] directly and set
//! `cmd.mixer_changed`, which live-applies the graph to the engine (and to the
//! running play session) and marks the project dirty for the next save.

use floptle_audio::{EffectDesc, EffectSlot, EqBand, EqBandKind, TrackDesc, MASTER};

/// Mixer tab state that survives across frames.
#[derive(Default)]
pub(crate) struct MixerUiState {
    /// Selected effect: (track slot, effect index). Slot 0 = Master, slot
    /// `i+1` = `tracks[i]` — matching the strip order on screen.
    pub selected: Option<(usize, usize)>,
    /// Copy/paste buffer for effect settings (right-click a chain row).
    pub fx_clipboard: Option<EffectDesc>,
    /// Smoothed meter level per track name (raw peaks flicker too hard).
    meters: std::collections::HashMap<String, f32>,
}

const STRIP_W: f32 = 140.0;
const FADER_H: f32 = 140.0;
/// Height of the insert-effect rack box (same on every strip so faders line up).
const FX_H: f32 = 150.0;
const DB_MIN: f32 = -60.0;
const DB_MAX: f32 = 12.0;

fn meter_color(level: f32) -> egui::Color32 {
    if level > 1.0 {
        egui::Color32::from_rgb(235, 80, 70) // clipping
    } else if level > 0.7 {
        egui::Color32::from_rgb(235, 190, 90)
    } else {
        egui::Color32::from_rgb(110, 210, 130)
    }
}

impl crate::EditorTabViewer<'_> {
    pub(crate) fn mixer_ui(&mut self, ui: &mut egui::Ui) {
        // Pull fresh meter peaks and fold them into the smoothed display level.
        let raw: std::collections::HashMap<String, f32> =
            self.audio.meters().into_iter().collect();
        for (name, lvl) in &raw {
            let s = self.mixer_ui.meters.entry(name.clone()).or_insert(0.0);
            *s = if *lvl > *s { *lvl } else { *s * 0.85 }; // instant attack, slow decay
        }
        ui.ctx().request_repaint(); // meters animate even when idle

        let mut changed = false;
        egui::Panel::right("mixer_fx_panel")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                changed |= self.effect_panel_ui(ui);
            });

        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                // Master strip first (slot 0), then user tracks.
                changed |= self.track_strip_ui(ui, 0);
                let n = self.mixer.tracks.len();
                for i in 0..n {
                    changed |= self.track_strip_ui(ui, i + 1);
                }
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    if ui.button("✚ Track").on_hover_text("Add a mixer track").clicked() {
                        let name = self.mixer.fresh_name("Track");
                        self.mixer.tracks.push(TrackDesc::new(name));
                        changed = true;
                    }
                });
            });
        });

        if changed {
            self.cmd.mixer_changed = true;
        }
    }

    /// One channel strip. `slot` 0 = Master, else `tracks[slot-1]`.
    fn track_strip_ui(&mut self, ui: &mut egui::Ui, slot: usize) -> bool {
        let mut changed = false;
        let is_master = slot == 0;
        let mut delete = false;
        let mut rename: Option<(String, String)> = None;

        let frame = egui::Frame::group(ui.style()).inner_margin(6.0);
        frame.show(ui, |ui| {
            ui.set_width(STRIP_W);
            // The strips sit in a horizontal row; the strip itself is strictly
            // top-down (the frame would otherwise inherit the row's layout).
            ui.vertical(|ui| {
                // ---- header: name (+ delete) ---------------------------------
                ui.horizontal(|ui| {
                    if is_master {
                        ui.add_sized(
                            [STRIP_W - 26.0, 18.0],
                            egui::Label::new(egui::RichText::new(MASTER).strong()),
                        );
                    } else {
                        let track = &mut self.mixer.tracks[slot - 1];
                        let mut name = track.name.clone();
                        let resp = ui.add_sized(
                            [STRIP_W - 26.0, 18.0],
                            egui::TextEdit::singleline(&mut name),
                        );
                        if resp.changed() && !name.is_empty() && name != MASTER {
                            rename = Some((track.name.clone(), name));
                        }
                        if ui.small_button("🗑").on_hover_text("Delete track").clicked() {
                            delete = true;
                        }
                    }
                });
                ui.separator();

                // ---- insert effects ------------------------------------------
                changed |= self.effect_chain_ui(ui, slot);

                // ---- pan -----------------------------------------------------
                ui.add_space(6.0);
                {
                    let track = if is_master {
                        &mut self.mixer.master
                    } else {
                        &mut self.mixer.tracks[slot - 1]
                    };
                    ui.horizontal(|ui| {
                        ui.weak("Pan");
                        ui.spacing_mut().slider_width = STRIP_W - 46.0;
                        let pan = ui
                            .add(egui::Slider::new(&mut track.pan, -1.0..=1.0).show_value(false));
                        changed |= pan.changed();
                        if pan.double_clicked() {
                            track.pan = 0.0;
                            changed = true;
                        }
                        pan.on_hover_text(format!(
                            "pan {:+.2} (double-click to center)",
                            track.pan
                        ));
                    });

                    // ---- fader + meter + readout / mute / solo ---------------
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        let fader = ui.add_sized(
                            [28.0, FADER_H],
                            egui::Slider::new(&mut track.gain_db, DB_MIN..=DB_MAX)
                                .vertical()
                                .show_value(false),
                        );
                        changed |= fader.changed();
                        if fader.double_clicked() {
                            track.gain_db = 0.0; // double-click a fader = unity
                            changed = true;
                        }

                        // Meter: post-fader peak on a dB scale, 0 dB at the top.
                        let (rect, _) = ui
                            .allocate_exact_size(egui::vec2(12.0, FADER_H), egui::Sense::hover());
                        let name = if is_master { MASTER } else { track.name.as_str() };
                        let level = self.mixer_ui.meters.get(name).copied().unwrap_or(0.0);
                        let db = 20.0 * level.max(1e-6).log10();
                        let t = ((db - DB_MIN) / (0.0 - DB_MIN)).clamp(0.0, 1.0);
                        let p = ui.painter();
                        p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
                        let fill = egui::Rect::from_min_max(
                            egui::pos2(rect.left(), rect.bottom() - rect.height() * t),
                            rect.max,
                        );
                        p.rect_filled(fill, 2.0, meter_color(level));
                        let tick = ui.visuals().weak_text_color().linear_multiply(0.4);
                        for tdb in [0.0f32, -12.0, -24.0, -48.0] {
                            let ty = rect.bottom()
                                - rect.height() * ((tdb - DB_MIN) / (0.0 - DB_MIN));
                            p.line_segment(
                                [egui::pos2(rect.left(), ty), egui::pos2(rect.right(), ty)],
                                (1.0, tick),
                            );
                        }

                        ui.vertical(|ui| {
                            let dv = ui.add(
                                egui::DragValue::new(&mut track.gain_db)
                                    .speed(0.1)
                                    .range(DB_MIN..=DB_MAX)
                                    .max_decimals(1)
                                    .suffix(" dB"),
                            );
                            changed |= dv.changed();
                            if !is_master {
                                ui.add_space(4.0);
                                let m =
                                    ui.selectable_label(track.muted, " M ").on_hover_text("Mute");
                                if m.clicked() {
                                    track.muted = !track.muted;
                                    changed = true;
                                }
                                let s = ui
                                    .selectable_label(track.soloed, " S ")
                                    .on_hover_text("Solo");
                                if s.clicked() {
                                    track.soloed = !track.soloed;
                                    changed = true;
                                }
                            }
                        });
                    });
                }

                // ---- output routing ------------------------------------------
                ui.add_space(4.0);
                if is_master {
                    ui.weak("→ output device");
                } else {
                    let current = self.mixer.tracks[slot - 1]
                        .output
                        .clone()
                        .unwrap_or_else(|| MASTER.to_string());
                    let others: Vec<String> = std::iter::once(MASTER.to_string())
                        .chain(
                            self.mixer
                                .tracks
                                .iter()
                                .enumerate()
                                .filter(|(j, _)| *j != slot - 1)
                                .map(|(_, t)| t.name.clone()),
                        )
                        .collect();
                    let mut pick: Option<String> = None;
                    egui::ComboBox::from_id_salt(("mixer_out", slot))
                        .selected_text(format!("→ {current}"))
                        .width(STRIP_W)
                        .show_ui(ui, |ui| {
                            for name in &others {
                                if ui.selectable_label(*name == current, name).clicked() {
                                    pick = Some(name.clone());
                                }
                            }
                        });
                    if let Some(p) = pick {
                        self.mixer.tracks[slot - 1].output =
                            if p == MASTER { None } else { Some(p) };
                        changed = true;
                    }
                }
            });
        });

        if let Some((old, new)) = rename {
            // Keep routing + selection coherent through the rename.
            for t in &mut self.mixer.tracks {
                if t.output.as_deref() == Some(old.as_str()) {
                    t.output = Some(new.clone());
                }
            }
            self.mixer.tracks[slot - 1].name = new;
            changed = true;
        }
        if delete {
            let dead = self.mixer.tracks.remove(slot - 1).name;
            for t in &mut self.mixer.tracks {
                if t.output.as_deref() == Some(dead.as_str()) {
                    t.output = None;
                }
            }
            self.mixer_ui.selected = None;
            changed = true;
        }
        changed
    }

    /// The strip's insert rack: a fixed-height vertical list of effect slots.
    /// Click selects (opens the editor panel); right-click gets bypass /
    /// reorder / copy / paste / remove; `✚ Effect` below adds or pastes one.
    fn effect_chain_ui(&mut self, ui: &mut egui::Ui, slot: usize) -> bool {
        let mut changed = false;
        let is_master = slot == 0;

        // Deferred intents so the row loop can't invalidate its own iteration.
        let mut remove: Option<usize> = None;
        let mut swap: Option<(usize, usize)> = None;
        let mut copy: Option<EffectDesc> = None;
        let mut paste_into: Option<usize> = None;
        let mut push_fx: Option<EffectDesc> = None;

        let clipboard = self.mixer_ui.fx_clipboard.clone();
        let inset = egui::Frame::NONE
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(4.0)
            .inner_margin(3.0);
        inset.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(FX_H);
            egui::ScrollArea::vertical()
                .id_salt(("mixer_fx", slot))
                .max_height(FX_H)
                .show(ui, |ui| {
                    let track = if is_master {
                        &mut self.mixer.master
                    } else {
                        &mut self.mixer.tracks[slot - 1]
                    };
                    let n = track.effects.len();
                    if n == 0 {
                        ui.add_space(FX_H * 0.42);
                        ui.vertical_centered(|ui| ui.weak("no effects"));
                    }
                    for (i, fx) in track.effects.iter_mut().enumerate() {
                        let selected = self.mixer_ui.selected == Some((slot, i));
                        let mut text = egui::RichText::new(format!(
                            "{} {}",
                            if fx.bypass { "◌" } else { "●" },
                            fx.effect.name()
                        ));
                        if fx.bypass {
                            text = text.weak();
                        }
                        let resp = ui.add_sized(
                            [ui.available_width(), 18.0],
                            egui::Button::selectable(selected, text),
                        );
                        if resp.clicked() || resp.secondary_clicked() {
                            self.mixer_ui.selected = Some((slot, i));
                        }
                        resp.context_menu(|ui| {
                            if ui
                                .button(if fx.bypass { "●  Enable" } else { "◌  Bypass" })
                                .clicked()
                            {
                                fx.bypass = !fx.bypass;
                                changed = true;
                                ui.close();
                            }
                            ui.separator();
                            if ui.add_enabled(i > 0, egui::Button::new("⏶  Move up")).clicked() {
                                swap = Some((i, i - 1));
                                ui.close();
                            }
                            if ui
                                .add_enabled(i + 1 < n, egui::Button::new("⏷  Move down"))
                                .clicked()
                            {
                                swap = Some((i, i + 1));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("⎘  Copy settings").clicked() {
                                copy = Some(fx.effect.clone());
                                ui.close();
                            }
                            let can_paste =
                                clipboard.as_ref().is_some_and(|c| c.same_kind(&fx.effect));
                            let paste = ui
                                .add_enabled(can_paste, egui::Button::new("📋  Paste settings"))
                                .on_disabled_hover_text(
                                    "copy settings from an effect of the same type first",
                                );
                            if paste.clicked() {
                                paste_into = Some(i);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("🗑  Remove").clicked() {
                                remove = Some(i);
                                ui.close();
                            }
                        });
                    }
                });
        });

        // ✚ Effect below the rack — add a fresh effect or paste the copied one.
        ui.with_layout(egui::Layout::top_down_justified(egui::Align::Center), |ui| {
            ui.menu_button("✚  Effect", |ui| {
                for fx in EffectDesc::all_defaults() {
                    if ui.button(fx.name()).clicked() {
                        push_fx = Some(fx);
                        ui.close();
                    }
                }
                if let Some(clip) = &clipboard {
                    ui.separator();
                    if ui.button(format!("📋  Paste {}", clip.name())).clicked() {
                        push_fx = Some(clip.clone());
                        ui.close();
                    }
                }
            });
        });

        // Apply the deferred intents.
        if let Some(fx) = copy {
            self.mixer_ui.fx_clipboard = Some(fx);
        }
        let track =
            if is_master { &mut self.mixer.master } else { &mut self.mixer.tracks[slot - 1] };
        if let Some(i) = paste_into
            && let Some(clip) = &self.mixer_ui.fx_clipboard
        {
            track.effects[i].effect = clip.clone();
            changed = true;
        }
        if let Some(fx) = push_fx {
            track.effects.push(EffectSlot { effect: fx, bypass: false });
            self.mixer_ui.selected = Some((slot, track.effects.len() - 1));
            changed = true;
        }
        if let Some(i) = remove {
            track.effects.remove(i);
            match self.mixer_ui.selected {
                Some((s, j)) if s == slot && j == i => self.mixer_ui.selected = None,
                Some((s, j)) if s == slot && j > i => {
                    self.mixer_ui.selected = Some((s, j - 1));
                }
                _ => {}
            }
            changed = true;
        }
        if let Some((a, b)) = swap {
            track.effects.swap(a, b);
            if self.mixer_ui.selected == Some((slot, a)) {
                self.mixer_ui.selected = Some((slot, b));
            } else if self.mixer_ui.selected == Some((slot, b)) {
                self.mixer_ui.selected = Some((slot, a));
            }
            changed = true;
        }
        changed
    }

    /// The right panel: the selected effect's parameters.
    fn effect_panel_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let Some((slot, idx)) = self.mixer_ui.selected else {
            ui.add_space(8.0);
            ui.weak("Select an effect on a strip to edit it.");
            return false;
        };
        let track = if slot == 0 {
            &mut self.mixer.master
        } else {
            match self.mixer.tracks.get_mut(slot - 1) {
                Some(t) => t,
                None => {
                    self.mixer_ui.selected = None;
                    return false;
                }
            }
        };
        let track_name = if slot == 0 { MASTER.to_string() } else { track.name.clone() };
        let Some(fx) = track.effects.get_mut(idx) else {
            self.mixer_ui.selected = None;
            return false;
        };

        let mut changed = false;
        let mut remove = false;
        ui.horizontal(|ui| {
            ui.strong(format!("{} — {}", track_name, fx.effect.name()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("🗑").on_hover_text("Remove effect").clicked() {
                    remove = true;
                }
                let b = ui
                    .selectable_label(fx.bypass, "⊘")
                    .on_hover_text("Bypass (keep in chain, skip processing)");
                if b.clicked() {
                    fx.bypass = !fx.bypass;
                    changed = true;
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            changed |= effect_params_ui(ui, &mut fx.effect);
        });
        if remove {
            track.effects.remove(idx);
            self.mixer_ui.selected = None;
            changed = true;
        }
        changed
    }
}

/// Slider row helper: label + drag value with a range.
fn param(ui: &mut egui::Ui, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, speed: f64) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            changed = ui
                .add(egui::DragValue::new(v).speed(speed).range(range))
                .changed();
        });
    });
    changed
}

/// Per-effect parameter editors. Returns true when anything changed.
fn effect_params_ui(ui: &mut egui::Ui, fx: &mut EffectDesc) -> bool {
    let mut c = false;
    match fx {
        EffectDesc::ParametricEq { bands } => c |= eq_editor_ui(ui, bands),
        EffectDesc::Delay { time_ms, feedback, mix, ping_pong, damping } => {
            c |= param(ui, "Time (ms)", time_ms, 1.0..=4000.0, 1.0);
            c |= param(ui, "Feedback", feedback, 0.0..=0.98, 0.005);
            c |= param(ui, "Damping", damping, 0.0..=0.99, 0.005);
            c |= param(ui, "Mix", mix, 0.0..=1.0, 0.005);
            c |= ui.checkbox(ping_pong, "Ping-pong (bounce L/R)").changed();
        }
        EffectDesc::Reverb { room_size, damping, width, mix, pre_delay_ms } => {
            c |= param(ui, "Room size", room_size, 0.0..=1.0, 0.005);
            c |= param(ui, "Damping", damping, 0.0..=1.0, 0.005);
            c |= param(ui, "Width", width, 0.0..=1.0, 0.005);
            c |= param(ui, "Pre-delay (ms)", pre_delay_ms, 0.0..=250.0, 0.5);
            c |= param(ui, "Mix", mix, 0.0..=1.0, 0.005);
        }
        EffectDesc::Chorus { rate_hz, depth_ms, mix } => {
            c |= param(ui, "Rate (Hz)", rate_hz, 0.01..=8.0, 0.01);
            c |= param(ui, "Depth (ms)", depth_ms, 0.1..=15.0, 0.05);
            c |= param(ui, "Mix", mix, 0.0..=1.0, 0.005);
        }
        EffectDesc::Flanger { rate_hz, depth_ms, feedback, mix } => {
            c |= param(ui, "Rate (Hz)", rate_hz, 0.01..=8.0, 0.01);
            c |= param(ui, "Depth (ms)", depth_ms, 0.1..=10.0, 0.05);
            c |= param(ui, "Feedback", feedback, -0.95..=0.95, 0.005);
            c |= param(ui, "Mix", mix, 0.0..=1.0, 0.005);
        }
        EffectDesc::Phaser { rate_hz, stages, center_hz, depth, feedback, mix } => {
            c |= param(ui, "Rate (Hz)", rate_hz, 0.01..=6.0, 0.01);
            let mut st = *stages as f32;
            if param(ui, "Stages", &mut st, 2.0..=12.0, 0.1) {
                *stages = (st.round() as u32).clamp(2, 12) & !1; // keep even
                c = true;
            }
            c |= param(ui, "Center (Hz)", center_hz, 80.0..=8000.0, 5.0);
            c |= param(ui, "Depth", depth, 0.0..=1.0, 0.005);
            c |= param(ui, "Feedback", feedback, -0.95..=0.95, 0.005);
            c |= param(ui, "Mix", mix, 0.0..=1.0, 0.005);
        }
        EffectDesc::PitchShift { semitones, window_ms, mix } => {
            c |= param(ui, "Semitones", semitones, -24.0..=24.0, 0.05);
            c |= param(ui, "Window (ms)", window_ms, 10.0..=250.0, 0.5);
            c |= param(ui, "Mix", mix, 0.0..=1.0, 0.005);
        }
        EffectDesc::Compressor { threshold_db, ratio, attack_ms, release_ms, makeup_db } => {
            c |= param(ui, "Threshold (dB)", threshold_db, -60.0..=0.0, 0.1);
            c |= param(ui, "Ratio", ratio, 1.0..=20.0, 0.05);
            c |= param(ui, "Attack (ms)", attack_ms, 0.1..=200.0, 0.5);
            c |= param(ui, "Release (ms)", release_ms, 5.0..=1000.0, 1.0);
            c |= param(ui, "Makeup (dB)", makeup_db, -12.0..=24.0, 0.1);
        }
        EffectDesc::Limiter { ceiling_db, release_ms } => {
            c |= param(ui, "Ceiling (dB)", ceiling_db, -24.0..=0.0, 0.1);
            c |= param(ui, "Release (ms)", release_ms, 5.0..=500.0, 1.0);
        }
        EffectDesc::Distortion { drive, tone, mix } => {
            c |= param(ui, "Drive", drive, 0.0..=1.0, 0.005);
            c |= param(ui, "Tone", tone, 0.0..=1.0, 0.005);
            c |= param(ui, "Mix", mix, 0.0..=1.0, 0.005);
        }
        EffectDesc::Utility { gain_db, width } => {
            c |= param(ui, "Gain (dB)", gain_db, -60.0..=24.0, 0.1);
            c |= param(ui, "Width", width, 0.0..=2.0, 0.005);
        }
    }
    c
}

// ---- the parametric EQ graph editor --------------------------------------

const EQ_FMIN: f32 = 20.0;
const EQ_FMAX: f32 = 20_000.0;
const EQ_DB: f32 = 18.0;
/// The response math needs a sample rate; the editor curve uses a nominal one
/// (band responses below Nyquist/2 barely differ between 44.1 and 48 kHz).
const EQ_SR: f32 = 48_000.0;

fn freq_to_x(f: f32, rect: egui::Rect) -> f32 {
    let t = (f / EQ_FMIN).ln() / (EQ_FMAX / EQ_FMIN).ln();
    rect.left() + t.clamp(0.0, 1.0) * rect.width()
}

fn x_to_freq(x: f32, rect: egui::Rect) -> f32 {
    let t = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
    EQ_FMIN * (EQ_FMAX / EQ_FMIN).powf(t)
}

fn db_to_y(db: f32, rect: egui::Rect) -> f32 {
    rect.center().y - (db / EQ_DB).clamp(-1.0, 1.0) * rect.height() * 0.5
}

fn y_to_db(y: f32, rect: egui::Rect) -> f32 {
    ((rect.center().y - y) / (rect.height() * 0.5)).clamp(-1.0, 1.0) * EQ_DB
}

fn band_color(i: usize) -> egui::Color32 {
    const COLORS: [egui::Color32; 6] = [
        egui::Color32::from_rgb(120, 200, 250),
        egui::Color32::from_rgb(250, 180, 100),
        egui::Color32::from_rgb(150, 240, 140),
        egui::Color32::from_rgb(240, 140, 200),
        egui::Color32::from_rgb(200, 160, 250),
        egui::Color32::from_rgb(250, 230, 120),
    ];
    COLORS[i % COLORS.len()]
}

/// The draggable EQ response curve + per-band rows. The drawn curve is the
/// exact filter math the audio thread runs (same RBJ coefficients).
fn eq_editor_ui(ui: &mut egui::Ui, bands: &mut Vec<EqBand>) -> bool {
    let mut changed = false;
    let width = ui.available_width().max(220.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(width, 170.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);

    // Grid: octave-ish frequency lines + 6 dB gain lines.
    let grid = ui.visuals().weak_text_color().linear_multiply(0.25);
    for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0] {
        let x = freq_to_x(f, rect);
        p.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], (1.0, grid));
    }
    for db in [-12.0f32, -6.0, 0.0, 6.0, 12.0] {
        let y = db_to_y(db, rect);
        let w = if db == 0.0 { 1.5 } else { 1.0 };
        p.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], (w, grid));
    }

    // Combined response curve.
    let steps = 160;
    let mut pts = Vec::with_capacity(steps);
    for i in 0..steps {
        let t = i as f32 / (steps - 1) as f32;
        let f = EQ_FMIN * (EQ_FMAX / EQ_FMIN).powf(t);
        let db: f32 = bands.iter().map(|b| b.response_db(f, EQ_SR)).sum();
        pts.push(egui::pos2(freq_to_x(f, rect), db_to_y(db, rect)));
    }
    p.add(egui::Shape::line(pts, egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 200, 250))));

    // Band handles: drag = freq + gain (cut filters: freq only).
    for (i, b) in bands.iter_mut().enumerate() {
        let has_gain = matches!(b.kind, EqBandKind::LowShelf | EqBandKind::Peak | EqBandKind::HighShelf);
        let pos = egui::pos2(
            freq_to_x(b.freq_hz, rect),
            if has_gain { db_to_y(b.gain_db, rect) } else { db_to_y(0.0, rect) },
        );
        let hr = egui::Rect::from_center_size(pos, egui::vec2(16.0, 16.0));
        let resp = ui.interact(hr, ui.id().with(("eq_band", i)), egui::Sense::drag());
        let color = if b.enabled { band_color(i) } else { ui.visuals().weak_text_color() };
        p.circle_filled(pos, if resp.hovered() || resp.dragged() { 7.0 } else { 5.0 }, color);
        p.circle_stroke(pos, 8.0, (1.0, color.linear_multiply(0.4)));
        if resp.dragged()
            && let Some(mp) = resp.interact_pointer_pos()
        {
            b.freq_hz = x_to_freq(mp.x, rect).clamp(EQ_FMIN, EQ_FMAX);
            if has_gain {
                b.gain_db = y_to_db(mp.y, rect);
            }
            changed = true;
        }
        // Scroll over a handle adjusts Q.
        if resp.hovered() {
            let scroll = ui.input(|inp| inp.smooth_scroll_delta.y);
            if scroll != 0.0 {
                b.q = (b.q * (1.0 + scroll.signum() * 0.12)).clamp(0.1, 18.0);
                changed = true;
            }
            resp.on_hover_text(format!(
                "{:.0} Hz  {:+.1} dB  Q {:.2}\n(drag to move, scroll for Q)",
                b.freq_hz, b.gain_db, b.q
            ));
        }
    }

    // Band rows.
    let mut remove: Option<usize> = None;
    for (i, b) in bands.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let dot = band_color(i);
            let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, dot);
            changed |= ui.checkbox(&mut b.enabled, "").changed();
            egui::ComboBox::from_id_salt(("eq_kind", i))
                .selected_text(kind_name(b.kind))
                .width(78.0)
                .show_ui(ui, |ui| {
                    for k in [
                        EqBandKind::LowShelf,
                        EqBandKind::Peak,
                        EqBandKind::HighShelf,
                        EqBandKind::LowPass,
                        EqBandKind::HighPass,
                        EqBandKind::Notch,
                    ] {
                        if ui.selectable_label(b.kind == k, kind_name(k)).clicked() {
                            b.kind = k;
                            changed = true;
                        }
                    }
                });
            changed |= ui
                .add(egui::DragValue::new(&mut b.freq_hz).speed(5.0).range(EQ_FMIN..=EQ_FMAX).suffix(" Hz"))
                .changed();
            changed |= ui
                .add(egui::DragValue::new(&mut b.gain_db).speed(0.1).range(-EQ_DB..=EQ_DB).suffix(" dB"))
                .changed();
            changed |= ui
                .add(egui::DragValue::new(&mut b.q).speed(0.02).range(0.1..=18.0).prefix("Q "))
                .changed();
            if ui.small_button("✖").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        bands.remove(i);
        changed = true;
    }
    if ui.button("✚ Band").clicked() {
        bands.push(EqBand { kind: EqBandKind::Peak, freq_hz: 1000.0, gain_db: 0.0, q: 1.0, enabled: true });
        changed = true;
    }
    changed
}

fn kind_name(k: EqBandKind) -> &'static str {
    match k {
        EqBandKind::LowShelf => "Low shelf",
        EqBandKind::Peak => "Peak",
        EqBandKind::HighShelf => "High shelf",
        EqBandKind::LowPass => "Low-pass",
        EqBandKind::HighPass => "High-pass",
        EqBandKind::Notch => "Notch",
    }
}
