//! The frame's UI: egui runs over the World here, between the gather and the
//! draw, and everything it decided is handed on as a `FrameUi`.

use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::Name;
use std::collections::HashMap;
use std::path::Path;
use crate::assets::{AssetPayload, collect_texture_paths};
#[cfg(feature = "editor-ui")]
use crate::dock::{EditorTab, default_dock};
#[cfg(feature = "editor-ui")]
use crate::gizmo::Tool;
#[cfg(feature = "editor-ui")]
use crate::hierarchy::{node_new_menu};
use crate::prefs::{DEFAULT_PLAY_TINT, GridConfig};
#[cfg(feature = "editor-ui")]
use crate::theme::{CODE_THEMES, ENGINE_THEMES};
#[cfg(feature = "editor-ui")]
use crate::export::EXPORT_TARGETS;
use crate::{Editor, ProjectAction};
#[cfg(feature = "editor-ui")]
use crate::{EditorCmd, EditorTabViewer, scene_hit};
use crate::perf_readout::{PerfSnapshot, Pacing};
#[cfg(feature = "editor-ui")]
use crate::gather::FrameGather;
#[cfg(feature = "editor-ui")]
use crate::perf_readout::perf_readout;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use crate::packages_ui::PackagesAction;
use std::path::PathBuf;
use crate::map_edit::MapSubMode;
use floptle_render::ViewLock;

/// What building the frame's UI hands to the draw: the picture to paint, and
/// the decisions to act on once it is painted.
pub(crate) struct FrameUi {
    pub(crate) ctx: egui::Context,
    /// What egui produced, minus the platform output the UI phase already handled.
    pub(crate) shapes: Vec<egui::epaint::ClippedShape>,
    pub(crate) textures_delta: egui::TexturesDelta,
    pub(crate) egui_ppp: f32,
    pub(crate) glass: bool,
    pub(crate) ppp: f32,
    pub(crate) ssr_on: bool,
    pub(crate) after: FrameAfter,
}

/// What the frame's UI asked for, applied after the draw: saves, the quit,
/// the packages' menu and shortcut clicks, the profiler toggle, and the
/// editor commands the panels queued.
pub(crate) struct FrameAfter {
    pub(crate) cmd: EditorCmd,
    pub(crate) ext_menu_click: Option<usize>,
    pub(crate) ext_shortcut_click: Option<usize>,
    pub(crate) frame_pointer_down: bool,
    pub(crate) perf_toggle: Option<bool>,
    pub(crate) pkg_action: crate::packages_ui::PackagesAction,
    pub(crate) want_exit: bool,
    pub(crate) want_save: bool,
    pub(crate) want_save_all: bool,
    pub(crate) want_save_project: bool,
}

/// What the frame's UI decided, filled in by the panels and dialogs as they run.
pub(crate) struct UiOut {
    pub(crate) ext_menu_click: Option<usize>,
    pub(crate) ext_shortcut_click: Option<usize>,
    pub(crate) pkg_action: PackagesAction,
    pub(crate) view_lock: ViewLock,
    pub(crate) view_ortho: Option<f32>,
    pub(crate) voice_test_pick: bool,
    pub(crate) voice_test_stop: bool,
    pub(crate) perf_snapshot: PerfSnapshot,
    pub(crate) perf_toggle: Option<bool>,
    pub(crate) cmd: EditorCmd,
    pub(crate) want_save: bool,
    pub(crate) want_save_project: bool,
    pub(crate) want_save_all: bool,
    pub(crate) want_exit: bool,
    pub(crate) frame_pointer_down: bool,
}

/// The networking state the multiplayer window and the net-stats overlay both
/// read, taken once per frame.
#[derive(Clone)]
pub(crate) struct NetSnapshot {
    pub(crate) net_hosting: bool,
    pub(crate) net_peer_count: usize,
    pub(crate) net_as_player: bool,
    pub(crate) net_rtt: f32,
    pub(crate) net_pred_stats: Option<(u64, u64, f64)>,
    pub(crate) net_late_inputs: u64,
    pub(crate) net_is_real: bool,
    pub(crate) replays: Vec<(String, PathBuf)>,
}

impl Editor {
    fn net_snapshot(&self) -> NetSnapshot {
        let net_hosting = self.net_server.is_some();
        let net_peer_count = self.net_server.as_ref().map(|s| s.peers().len()).unwrap_or(0);
        let net_as_player = self.net_play_client.is_some();
        // The measured round trip, falling back to the transport's own number
        // until the first probe comes back. Through a relay the transport can
        // only see its own leg, so it reports host↔relay and calls it the
        // player's ping — off by a whole hop, and always in the flattering
        // direction.
        let net_rtt = self
            .net_play_client
            .as_ref()
            .map(|c| {
                c.peer_rtt_ms(floptle_net::SERVER)
                    .unwrap_or_else(|| c.stats(floptle_net::SERVER).rtt_ms)
            })
            .unwrap_or(0.0);
        let net_pred_stats = self
            .net_predictor
            .as_ref()
            .map(|(_, p)| (p.corrections, p.confirmations, p.last_error));
        let net_late_inputs = self
            .net_hidden
            .as_ref()
            .map(|h| h.session.late_inputs())
            .or_else(|| self.net_server.as_ref().map(|s| s.late_inputs()))
            .unwrap_or(0);
        let replays = crate::shadow::list_replays(&self.project_root);
        // A real session (quic) has no hub: the link is the actual network, so
        // the simulated latency/loss sliders and ghost worlds don't apply.
        let net_is_real = (self.net_server.is_some() || self.net_play_client.is_some())
            && self.net_hub.is_none();
        NetSnapshot { net_hosting, net_peer_count, net_as_player, net_rtt, net_pred_stats, net_late_inputs, net_is_real, replays }
    }

    /// Build this frame's UI. `None` only if the renderer is not ready, which
    /// `render()` has already ruled out.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_frame_ui(
        &mut self,
        gather: FrameGather,
        _elapsed: f32,
        game_focused: bool,
        game_offscreen: bool,
        preview_view: Option<crate::PreviewView>,
        profile: &Rc<RefCell<floptle_core::profile::FrameProfile>>,
        window: &Arc<winit::window::Window>,
    ) -> Option<(FrameGather, FrameUi)> {
        let (Some(_gpu), Some(_raster), Some(_raymarch), Some(_retro), Some(_post), Some(egui)) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.retro.as_mut(),
            self.post.as_mut(),
            self.egui.as_mut(),
        ) else {
            return None;
        };
        let _scene_history = &mut self.scene_history;
        let gpu_timer = self.gpu_timer.as_mut();
        let _aspect = gather.aspect;
        let _clear = gather.clear;
        let gizmo_tool = gather.gizmo_tool;
        let instances = &gather.instances;
        let light_node = gather.light_node;
        // ---- build the egui UI (mutating the World) ----
        let mut raw_input = egui.state.take_egui_input(window);
        // A focused game owns the keyboard. egui hands Tab to
        // widget focus traversal before anything else sees it, which put every
        // press on the dock's tab bar and left `input.pressed("tab")` returning
        // false — the same as not being pressed, so a game bound to the most
        // conventional inventory key there is had no way to tell. Gated on a text
        // field not wanting input, so typing into the Console or the Inspector
        // during play still works; a click is how you go back to the editor.
        // `text_edit_focused`, not `egui_wants_keyboard_input` — the latter is
        // "any widget has focus", so clicking a Play-mode HUD button used to
        // hand Tab back to the dock for the rest of the session. See the same
        // fix at the `typing` gate in `main.rs`.
        if self.playing && game_focused && !egui.ctx.text_edit_focused() {
            crate::game_keys::claim_keys_for_game(&mut raw_input, &egui.ctx);
        }
        let ctx = egui.ctx.clone();
        // A package that shipped a typeface gets it registered here — after the
        // load pass, before anything draws with it. `set_fonts` rebuilds egui's
        // glyph atlas, so it is gated on the flag and not run per frame; a
        // project whose packages ship no fonts never reaches it at all.
        if self.ext.fonts_dirty {
            self.ext.fonts_dirty = false;
            ctx.set_fonts(crate::fonts::definitions(&self.ext.fonts));
        }
        // Apply the selected engine (chrome) theme, then a play-mode tint on top so you
        // never mistake play mode for edit mode (and lose edits on Stop). Reapplied each
        // frame so switching the theme in Preferences takes effect immediately.
        {
            let theme = ENGINE_THEMES[self.engine_theme.min(ENGINE_THEMES.len() - 1)];
            let mut vis = theme.visuals();
            if self.playing && self.play_tint_enabled {
                let [tr, tg, tb] = self.play_tint;
                let tint = |c: egui::Color32| {
                    egui::Color32::from_rgb(
                        (c.r() as u16 + tr as u16).min(255) as u8,
                        (c.g() as u16 + tg as u16).min(255) as u8,
                        (c.b() as u16 + tb as u16).min(255) as u8,
                    )
                };
                vis.panel_fill = tint(vis.panel_fill);
                vis.window_fill = tint(vis.window_fill);
                vis.extreme_bg_color = tint(vis.extreme_bg_color);
            }
            ctx.all_styles_mut(|s| {
                s.visuals = vis.clone();
                // **Leave the scrollbar its own gutter.**
                //
                // egui's scroll bars float by default: they are drawn over the
                // contents and allocate no width. So the last few pixels of
                // every scrolling panel are behind a bar — a slider's label
                // ellipsised down to its first letter, a `…` menu half over the
                // edge — and the panel looks a little bit cut off everywhere,
                // which is exactly what it is. The controls are laid out to the
                // panel's edge correctly; the edge is simply not where the
                // visible area ends.
                //
                // Allocating the bar's width moves that edge in to where things
                // can actually be seen, and every widget follows it — egui's own
                // truncation as much as `responsive::fit_here`. The bar still
                // floats and still looks the same.
                s.spacing.scroll.floating_allocated_width = s.spacing.scroll.bar_width;
            });
        }
        let ppp = ctx.pixels_per_point();
        // Prefill the export title from the project's title (Project Settings
        // ⏵ Game); the folder name is a poor fallback (the conventional root is
        // just `assets`, which also collides with the shipped assets folder).
        if self.export_title.is_empty() {
            self.export_title = self.project.title.clone().unwrap_or_else(|| {
                self.project_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|n| n != "assets")
                    .unwrap_or_default()
            });
        }
        // The entry-scene picker's options (only scanned while the window is up).
        let map_mode = self.map_mode;
        // ⏱ The frame-timing panel's open flag and the last frame it collected,
        // both taken out here for the same reason everything else on this line is
        // — the UI below runs while `self` is split apart.
        // Read from the borrowed timer rather than back out of `self`: the frame
        // took it mutably at the destructure. `poll` has already run this frame,
        // so these are the newest results that have actually landed.
        let gpu_spans: Vec<floptle_render::Span> =
            gpu_timer.as_deref().map(|t| t.spans().to_vec()).unwrap_or_default();
        let gpu_total = gpu_timer.as_deref().map(|t| t.total_ms()).unwrap_or(0.0);
        self.gpu_timing_frames = self.gpu_timing_frames.wrapping_add(1);
        let gpu_timing_supported = gpu_timer.is_some();
        if !gpu_spans.is_empty()
            && self.gpu_timing_open
            && std::env::var("FLOPTLE_GPU_TIMING").is_ok()
            && self.gpu_timing_frames.is_multiple_of(120)
        {
            floptle_say::say!("--- GPU frame {gpu_total:.2} ms");
            for sp in &gpu_spans {
                floptle_say::say!("  {:>7.3} ms  {}", sp.ms, sp.label);
            }
        }
        // Dirty tilesets ride the scene's flag here. They are not scene state —
        // they are their own files — but every gate that asks "is there unsaved
        // work" wants one answer, and a tileset's collision shapes and autotile
        // groups are hours of work.
        let scene_dirty_now = self.scene_dirty || !self.tiles.dirty.is_empty();
        // Current theme selections (changes are routed through `cmd`, then saved + applied).
        let engine_theme = self.engine_theme;
        let code_theme = self.code_theme;
        let project_root = self.project_root.clone();
        let playing = self.playing;
        let net = self.net_snapshot();
        let net_hosting = net.net_hosting;
        let net_as_player = net.net_as_player;
        let mut out = UiOut {
            ext_menu_click: None,
            ext_shortcut_click: None,
            pkg_action: crate::packages_ui::PackagesAction::default(),
            view_lock: self.camera.lock,
            view_ortho: self.camera.ortho,
            voice_test_pick: false,
            voice_test_stop: false,
            perf_snapshot: PerfSnapshot::take(&profile.borrow()),
            perf_toggle: None,
            cmd: EditorCmd::default(),
            want_save: false,
            want_save_project: false,
            want_save_all: false,
            want_exit: false,
            frame_pointer_down: false,
        };
        let scene_name = self.scene_name.clone();
        let tool = self.tool;
        // Multiplayer harness panel state: read-only status snapshot + live knobs.
        if self.net_host_port.is_empty() {
            self.net_host_port = "7777".into();
        }
        if self.net_join_addr.is_empty() {
            self.net_join_addr = "quic://127.0.0.1:7777".into();
        }
        if self.net_relay_addr.is_empty() {
            // Floptle Cloud: a managed code carries its region in its first
            // letter, so `cloud` is the whole address. Self-hosters type
            // their own `host:port`.
            self.net_relay_addr = "cloud".into();
        }
        out.perf_snapshot.pacing = Pacing {
            mean_ms: self.frame_ms,
            p99_ms: self.frame_low_ms,
            // `refresh_period` is in seconds (it is compared against `dt`).
            refresh_ms: self.refresh_period * 1000.0,
            snap_rate: self.dt_snap_rate,
            present_wait_ms: self.present_wait_ms,
            cost_ms: (self.frame_ms - self.present_wait_ms).max(0.0),
        };
        // Player mode (an exported build / --play): no editor chrome at all —
        // the Game view is the window. F1 (handled at the winit layer) toggles
        // the multiplayer window, which still works for LAN/relay sessions.
        let player_mode = self.player_mode;
        // Relative export folders resolve against the project's parent (shown
        // live in the dialog) — never the process CWD, which depends on how
        // the editor was launched.
        let export_base =
            self.project_root.parent().unwrap_or(&self.project_root).to_path_buf();
        if self.export_dir.trim().is_empty() {
            self.export_dir = "builds".into();
        }
        let full_output = ctx.run_ui(raw_input, |ui| {
            let pointer_down = ui.input(|i| i.pointer.any_down());
            out.frame_pointer_down = pointer_down;
            self.ui_top_menu_bar(ui, &mut out, net_hosting, player_mode, playing, scene_dirty_now);
            self.ui_frame_cost_window(ui, &mut out);
            self.ui_multiplayer_harness(ui, &mut out, player_mode, playing, &net);
            self.ui_net_stats_overlay(ui, playing, &net);
            self.ui_player_mode_hint(ui, &mut out, net_as_player, net_hosting, player_mode);
            self.ui_dock(ui, &mut out, code_theme, game_offscreen, gizmo_tool, map_mode, player_mode, playing, pointer_down, ppp, &preview_view, &project_root, &scene_name, tool);
            self.ui_package_panels(ui);
            self.ui_package_messages(ui);
            self.ui_package_shortcuts(ui, &mut out);
            self.ui_export_game(ui, &mut out, &export_base);
            self.ui_crash_prompt(ui, &mut out);
            self.ui_autosave_prompt(ui, &mut out);
            self.ui_preferences_window(ui, &mut out, code_theme, engine_theme);
            self.ui_frame_timing_window(ui, &gpu_spans, gpu_timing_supported, gpu_total);
            self.ui_grid_settings_window(ui, &mut out);
            self.ui_viewport_context_menu(ui, &mut out, map_mode, tool);
            self.ui_project_window(ui, &mut out);
            self.ui_rename_prompt(ui, &mut out);
            self.ui_new_scene_prompt(ui, &mut out);
            self.ui_new_asset_prompt(ui, &mut out);
            self.ui_quit_prompt(ui, &mut out, scene_dirty_now);
            self.ui_close_image_prompt(ui, &mut out);
            self.ui_transient_toast(ui);
            self.ui_delete_asset_prompt(ui, &mut out);
            self.ui_collision_layer_prompt(ui, &mut out);
            self.ui_new_terrain_prompt(ui, &mut out);
            self.ui_open_scene_prompt(ui, &mut out);
            self.ui_who_has_the_pointer(ui, player_mode, playing);
        });
        let (Some(gpu), Some(raster), Some(raymarch), Some(retro), Some(post), Some(egui)) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.retro.as_mut(),
            self.post.as_mut(),
            self.egui.as_mut(),
        ) else {
            return None;
        };
        let scene_history = &mut self.scene_history;
        let UiOut {
            ext_menu_click,
            ext_shortcut_click,
            pkg_action,
            view_lock,
            view_ortho,
            perf_toggle,
            cmd,
            want_save,
            want_save_project,
            want_save_all,
            want_exit,
            frame_pointer_down,
            ..
        } = out;
        if view_lock != self.camera.lock {
            self.camera.set_lock(view_lock);
        }
        if view_ortho != self.camera.ortho {
            self.camera.set_ortho(view_ortho);
        }
        let egui::FullOutput { platform_output, textures_delta, shapes, pixels_per_point: egui_ppp, .. } =
            full_output;
        egui.state.handle_platform_output(window, platform_output);
        // egui-winit's cursor-icon handling calls set_cursor_visible(true) whenever
        // the hover icon changes — un-hiding a cursor the game grabbed. Re-assert
        // the hide while any lock is held so the pointer can't flicker back.
        // A script lock only hides the cursor while it's actually over the game
        // view: where the grab is only a Confine (X11), the pointer can reach
        // the Inspector mid-play — it must be visible there to tweak values.
        // Where the grab is a real Lock it cannot travel at all, which is what
        // Escape (`cursor_freed`) is for.
        // (cursor_over_game and game_holds_cursor are inlined as plain field
        // reads — this scope holds a mutable gpu borrow, so a `&self` method
        // here would borrow the whole editor)
        let over_game = scene_hit(&egui.ctx, self.cursor, self.game_rect);
        let game_has_it = self.script_mouse_lock && !self.cursor_freed;
        if self.game_trap || (game_has_it && over_game) {
            window.set_cursor_visible(false);
        } else if self.script_mouse_lock {
            // Off the game view with the lock still wanted — or held back by
            // Escape — force the show. egui only un-hides on an icon change,
            // which may never fire, and a cursor you freed but cannot see is
            // the same bug as one you never freed.
            window.set_cursor_visible(true);
        }
        // **Against the dims last applied, not against what they were at the top
        // of this frame.** The old check captured the value before the UI pass
        // and compared after it, which caught exactly one source of change:
        // Project Settings. A script setting `app.setRetroHeight` runs before
        // that capture, so its new value was already there to be captured as the
        // "old" one and the target was never resized — the setting appeared to
        // do nothing, for ever. Comparing against what the
        // target actually is has no such blind spot, whoever moved the number.
        let want_retro =
            self.project.retro_size(gpu.config.width as f32 / gpu.config.height.max(1) as f32);
        if want_retro != self.retro_applied {
            retro.resize_to(gpu, want_retro.0, want_retro.1);
            self.retro_applied = want_retro;
        }

        // Post-processing (SSAO/bloom/vignette, from the scene's PostProcess node —
        // gathered above) runs at the resolution the scene was composited at: the
        // retro internal res in retro mode (before the nearest-neighbor upscale, so
        // AO/bloom/vignette land on the same chunky pixel grid as the scene), else
        // full frame res. The stack lazily re-sizes when retro toggles/resizes.
        let post_size =
            if self.project.retro { retro.resolution() } else { (gpu.config.width, gpu.config.height) };
        post.configure(gpu, post_size.0, post_size.1, self.project.retro);

        // Screen-space reflections need somewhere to keep last frame's picture.
        // Allocated the first frame a scene asks for them and dropped again when
        // it stops: this is a full-frame mip chain, and much the largest thing
        // the renderer holds, so a project that never turns reflections on must
        // not carry one. It follows the composited size, which in retro mode is
        // the internal resolution — reflecting a full-res picture into a 320×240
        // scene would be sharper than anything else in the frame.
        let ssr_on = light_node.reflections;
        // Glass needs the same stored picture, for the opposite reason: not to
        // reflect the scene but to see through it. So the texture is allocated
        // when either asks, and a scene with a single window in it gets one
        // without having to switch reflections on as well.
        let glass = raster.any_transmissive(instances);
        {
            let fmt = gpu.scene_format();
            let rebuilt = if ssr_on || glass {
                match scene_history.as_mut() {
                    Some(h) => h.resize_to(&gpu.device, post_size.0, post_size.1, fmt),
                    None => {
                        *scene_history = Some(floptle_render::SceneHistory::new(
                            &gpu.device,
                            post_size.0,
                            post_size.1,
                            fmt,
                        ));
                        true
                    }
                }
            } else {
                scene_history.take().is_some()
            };
            // A bind group is immutable, so it is rebuilt only when the texture
            // behind it actually changed — not every frame, which would allocate
            // a bind group per frame for as long as the editor was open.
            if rebuilt {
                let bind = scene_history.as_ref().map(|h| (h.view(), h.sampler()));
                raymarch.set_scene_history(gpu, bind);
            }
        }

        Some((
            gather,
            FrameUi {
                ctx,
                shapes,
                textures_delta,
                egui_ppp,
                glass,
                ppp,
                ssr_on,
                after: FrameAfter {
                    cmd,
                    ext_menu_click,
                    ext_shortcut_click,
                    frame_pointer_down,
                    perf_toggle,
                    pkg_action,
                    want_exit,
                    want_save,
                    want_save_all,
                    want_save_project,
                },
            },
        ))
    }

    /// top menu bar (never in a build)
    #[allow(clippy::too_many_arguments)]
    fn ui_top_menu_bar(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        net_hosting: bool,
        player_mode: bool,
        playing: bool,
        scene_dirty_now: bool,
    ) {
        let has_selection = !self.selection.is_empty();
        // What the save-status chip names on hover: the real file being edited.
        let save_status_file = if self.scene_rel.is_empty() {
            format!("scenes/{}.ron", self.scene_name)
        } else {
            self.scene_rel.clone()
        };
        let paused = self.paused;
        let game_tick_no = self.game_tick_no;
        // Built before the closure: `ext_menu_tree` reads the whole editor, and
        // inside the UI pass only disjoint field borrows exist.
        let ext_menus = crate::ext_wire::menu_tree(&self.ext);
        let project_trust = self.project_trust.clone();
        // ---- top menu bar (never in a build) ----
        if !player_mode {
        // Above the menu bar, so it is the first thing seen: this
        // project's packages are running with no permissions until the
        // user says otherwise (`ext::trust`).
        if let (true, Some(answer)) = crate::ext::trust::banner(ui, &project_trust) {
            out.cmd.project_trust = Some(answer);
        }
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New / Open Project…").clicked() {
                        self.show_project_mgr = true;
                        ui.close();
                    }
                    if ui.button("Close Project").clicked() {
                        out.cmd.project_action = Some(ProjectAction::Close);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Save Scene").clicked() {
                        out.want_save = true;
                        ui.close();
                    }
                    if ui.button("Save Project").clicked() {
                        out.want_save_project = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .button("Open Project Folder")
                        .on_hover_text("show the project (assets, scenes, scripts) in your file manager")
                        .clicked()
                    {
                        out.cmd.open_folder = Some(std::path::PathBuf::new()); // empty = project root
                        ui.close();
                    }
                    if ui
                        .button("Export Game…")
                        .on_hover_text(
                            "stamp out a runnable build: the engine + your project, for \
                             any platform — Windows, Linux or macOS, from whichever \
                             one you're on",
                        )
                        .clicked()
                    {
                        self.show_export = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui.button("Undo  (Ctrl+Z)").clicked() { out.cmd.undo = true; ui.close(); }
                    if ui.button("Redo  (Ctrl+Y)").clicked() { out.cmd.redo = true; ui.close(); }
                    ui.separator();
                    // Selection-dependent items grey out with nothing selected
                    // (Paste stays — it depends on the clipboard, not selection).
                    if ui.add_enabled(has_selection, egui::Button::new("Copy  (Ctrl+C)")).clicked() { out.cmd.copy = true; ui.close(); }
                    if ui.button("Paste  (Ctrl+V)").clicked() { out.cmd.paste = true; ui.close(); }
                    if ui.add_enabled(has_selection, egui::Button::new("Duplicate  (Ctrl+D)")).clicked() { out.cmd.duplicate = true; ui.close(); }
                    if ui.add_enabled(has_selection, egui::Button::new("Delete  (Del)")).clicked() { out.cmd.delete = true; ui.close(); }
                    ui.separator();
                    if ui.button("Project Settings").on_hover_text(
                        "Opens the ⚙ Settings tab — drag it wherever you like, or dock it beside the viewport.",
                    ).clicked() {
                        out.cmd.open_settings = true;
                        ui.close();
                    }
                    if ui.button("Preferences…").clicked() {
                        self.show_preferences = true;
                        ui.close();
                    }
                });
                // The same catalog as the Hierarchy's ✚ New menu — one source of truth.
                ui.menu_button("Add", |ui| node_new_menu(ui, &mut out.cmd, None));
                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.grid.show, "Grid");
                    ui.checkbox(&mut self.grid.snap, "Snap to grid");
                    if ui.button("Grid Settings…").clicked() {
                        self.show_grid_settings = true;
                        ui.close();
                    }
                    ui.separator();
                    ui.checkbox(&mut self.show_terrain_collider, "Terrain collider wireframe")
                        .on_hover_text("show the terrain's collision surface (what the player walks on)");
                    ui.checkbox(&mut self.show_mesh_colliders, "Collider wireframes (mesh + shapes)")
                        .on_hover_text("show every static collider — walkable meshes and Collidable Cube/Sphere/Capsule shapes (the selected one always shows)");
                    ui.checkbox(&mut self.show_navmesh, "Navmesh")
                        .on_hover_text(
                            "show where characters can walk as one filled surface, a colour \
                             per connected area, with the joins between elevations drawn \
                             where a character can actually take them (the Nav Mesh node \
                             always shows its own when selected)",
                        );
                    ui.add_enabled_ui(self.show_navmesh, |ui| {
                        ui.checkbox(&mut self.nav_cells, "    ⊞ …and the rectangles it was cut into")
                            .on_hover_text(
                                "the bake's working: every convex rectangle the walkable \
                                 surface was divided into. Useful for judging cell size; \
                                 it is not what the ground looks like",
                            );
                    });
                });
                // Tool windows + panels live under Window (View = viewport display).
                // Every entry opens/focuses its window (close them from the
                // window itself) — one consistent behavior.
                ui.menu_button("Window", |ui| {
                    // Materials are edited in the Inspector's material view;
                    // this opens it on the selected node's material.
                    let with_material = self
                        .selection
                        .last()
                        .copied()
                        .filter(|&e| self.world.get::<floptle_core::Material>(e).is_some());
                    if ui
                        .add_enabled(with_material.is_some(), egui::Button::new("◑ Material"))
                        .on_hover_text("edit the selected node's material in the Inspector")
                        .on_disabled_hover_text("select a node with a Material first")
                        .clicked()
                    {
                        out.cmd.open_material = with_material.map(crate::material_bank::MaterialTarget::Node);
                        ui.close();
                    }
                    if ui.button("◎ Animation Controller").on_hover_text("the state-graph editor: states, transitions, fades, layers").clicked() {
                        out.cmd.focus_anim_graph = true;
                        ui.close();
                    }
                    if ui.button("⏱ Animating").on_hover_text("the animation timeline: preview, keys, events").clicked() {
                        out.cmd.focus_animating = true;
                        ui.close();
                    }
                    if ui
                        .checkbox(&mut self.gpu_timing_open, "⏱ Frame timing")
                        .on_hover_text(
                            "where the frame's time actually goes, measured on the GPU pass \
                             by pass. Nothing is measured while this is shut, so leaving it \
                             off costs nothing",
                        )
                        .changed()
                    {
                        ui.close();
                    }
                    if ui.button("Δ Terrain tools").clicked() {
                        out.cmd.focus_terrain = true;
                        ui.close();
                    }
                    if ui.button("▦ Model tools").clicked() {
                        out.cmd.focus_map = true;
                        ui.close();
                    }
                    if ui
                        .button("🖼 Image editor")
                        .on_hover_text(
                            "draw a texture in the engine — pixels, paint and vectors, with the mesh updating as you paint",
                        )
                        .clicked()
                    {
                        out.cmd.focus_image = true;
                        ui.close();
                    }
                    if ui
                        .button("📦 Packages")
                        .on_hover_text(
                            "install, switch off or write a package — editor tools, \
                             scripts and art anybody can make and share",
                        )
                        .clicked()
                    {
                        out.cmd.focus_packages = true;
                        ui.close();
                    }
                    ui.separator();
                    ui.label(
                        egui::RichText::new("your layout is saved when you close the editor")
                            .small()
                            .weak(),
                    );
                    if ui
                        .button("⟲ Reset layout")
                        .on_hover_text(
                            "put every panel back where it starts: Hierarchy + Map left, \
                             viewports and graph editors centre, Inspector right, \
                             project and timelines below — and forget the saved one, so \
                             it stays reset",
                        )
                        .clicked()
                    {
                        out.cmd.reset_layout = true;
                        ui.close();
                    }
                    if ui
                        .button("⟲ Reset window size")
                        .on_hover_text(
                            "back to 1280×720 where it can be seen, and forget where this \
                             window was — what to press if it opened somewhere awkward",
                        )
                        .clicked()
                    {
                        out.cmd.reset_window = true;
                        ui.close();
                    }
                });
                // Help, and specifically somewhere to report things. The tracker used
                // to appear once, in the Hub's About tab, which is not where anybody
                // is standing when something goes wrong.
                ui.menu_button("Help", |ui| {
                    if ui
                        .button("🎓 Learn — follow-along tutorials")
                        .on_hover_text(
                            "build a platformer, a top-down RPG or Flappy step by \
                             step, with each step ticking itself off as your project \
                             comes to match it",
                        )
                        .clicked()
                    {
                        out.cmd.focus_learn = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .button("🐛 Report a bug")
                        .on_hover_text(crate::ISSUES_URL)
                        .clicked()
                    {
                        crate::open_issue_tracker(None);
                        ui.close();
                    }
                    if ui.button("📖 Scripting docs").clicked() {
                        let _ = floptle_script::open_in_browser(crate::DOCS_URL);
                        ui.close();
                    }
                    if ui.button("🌐 fopull.com").clicked() {
                        let _ = floptle_script::open_in_browser("https://fopull.com/");
                        ui.close();
                    }
                    ui.separator();
                    ui.label(egui::RichText::new(format!("Floptle {}", env!("CARGO_PKG_VERSION"))).small());
                });
                // Whatever the project's packages registered, grouped by
                // the first segment of each path — so two packages both
                // filing under "Tools" build one menu, not two.
                for group in &ext_menus {
                    ui.menu_button(&group.title, |ui| {
                        for (label, idx) in &group.items {
                            if ui.button(label).clicked() {
                                out.ext_menu_click = Some(*idx);
                                ui.close();
                            }
                        }
                    });
                }
                ui.separator();
                let play_label = if playing { "⏹ Stop  (F1)" } else { "⏵ Play  (F1)" };
                if ui.button(play_label).clicked() {
                    out.cmd.toggle_play = true;
                }
                if playing {
                    let pause_label = if paused { "⏵ Resume  (F2)" } else { "⏸ Pause  (F2)" };
                    if ui.button(pause_label).clicked() {
                        out.cmd.toggle_pause = true;
                    }
                    // Frame-step: only meaningful while frozen. One click = exactly
                    // one fixedUpdate tick (scripts, physics, animation), then stop
                    // again — how you find out whether a jab is 4 frames of startup
                    // or 5.
                    ui.add_enabled_ui(paused, |ui| {
                        // Backwards first, so the pair reads left-to-right as a
                        // scrubber rather than as two unrelated buttons.
                        if ui
                            .button("⏮ Back  (Shift+F3)")
                            .on_hover_text(
                                "put the simulation back exactly one gameplay tick.\n\n                                     A simulation isn't invertible, so this reads the \
                                 ROLLBACK state ring rather than re-deriving anything: \
                                 it needs a rollback session running, and reaches back \
                                 as far as the ring keeps (about a fifth of a second).",
                            )
                            .clicked()
                        {
                            out.cmd.step_tick_back = true;
                        }
                        if ui
                            .button("⏭ Step  (F3)")
                            .on_hover_text(
                                "advance exactly one gameplay tick — scripts, \
                                 physics and animation each move one frame",
                            )
                            .clicked()
                        {
                            out.cmd.step_tick = true;
                        }
                    });
                    // The tick counter, so an observed event has a frame number you
                    // can put in a frame-data table.
                    ui.label(
                        egui::RichText::new(format!("tick {game_tick_no}")).monospace().weak(),
                    )
                    .on_hover_text("gameplay ticks since Play started (60 Hz)");
                }
                if ui
                    .button(if net_hosting { "🌐 hosting" } else { "🌐" })
                    .on_hover_text("Multiplayer — host & join locally, latency/loss sliders (docs/multiplayer.md)")
                    .clicked()
                {
                    self.show_net_panel = !self.show_net_panel;
                }
                // ⏱ Frame cost. Opening it turns collection
                // on; closing it turns collection off, so the profiler costs
                // nothing when nobody is looking at it — which is the only
                // way one stays switched on.
                if ui
                    .button(if self.show_perf_panel { "⏱ profiling" } else { "⏱" })
                    .on_hover_text(
                        "Frame cost — where the time goes, per subsystem and per \
                         script. Readable from Lua too (perf.*), so a game can \
                         assert its own budget in a smoke test.",
                    )
                    .clicked()
                {
                    self.show_perf_panel = !self.show_perf_panel;
                    out.perf_toggle = Some(self.show_perf_panel);
                }
                // The view is chosen by the Scene / Game dock tabs (the editor
                // free-fly view vs the active-camera gameplay view), not a toggle here.

                // ---- save status (right end of the bar, always visible) ----
                // Whatever tab you're docked in, this answers "are my changes
                // on disk?": a quiet "✔ saved" at rest, an amber "● unsaved"
                // the moment an edit lands, and a brief green glow when a
                // save completes. Right-aligned so nothing else ever moves.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let dt = ui.input(|i| i.stable_dt).min(0.1);
                    self.save_flash = (self.save_flash - dt).max(0.0);
                    let quiet = ui.visuals().weak_text_color();
                    // The two signal colours, from the one place they live
                    // (`theme::signal`) — an unsaved change is a warn and a
                    // save that landed is a good, the same amber and green
                    // as everywhere else in the editor.
                    let (label, color, hover) = if scene_dirty_now {
                        (
                            "● unsaved",
                            crate::theme::signal::WARN,
                            format!("{save_status_file} has unsaved changes — click here (or Ctrl+S) to save"),
                        )
                    } else {
                        // Glow bright right after a save, settle to quiet
                        // (t = 0 is the resting state — one branch, one wording).
                        let t = (self.save_flash / Editor::SAVE_FLASH_SECS).clamp(0.0, 1.0);
                        (
                            "✔ saved",
                            quiet.lerp_to_gamma(crate::theme::signal::GOOD, t),
                            format!("{save_status_file} is saved"),
                        )
                    };
                    let text = egui::RichText::new(label).color(color);
                    if playing {
                        // Saving is blocked during Play (Play changes aren't
                        // kept) — say so instead of failing quietly.
                        ui.add_enabled(false, egui::Button::new(text).frame(false))
                            .on_disabled_hover_text(
                                "can't save during Play — press Stop first (Play changes aren't kept)",
                            );
                    } else if scene_dirty_now {
                        // Only a button when there is something to save. A
                        // chip that looks pressable and does nothing is the
                        // small dead interaction this is meant to replace.
                        if ui
                            .add(egui::Button::new(text).frame(false))
                            .on_hover_text(hover)
                            .clicked()
                        {
                            out.want_save = true;
                        }
                    } else {
                        ui.label(text).on_hover_text(hover);
                    }
                });
            });
        });
        }

    }

    /// ⏱ frame cost
    fn ui_frame_cost_window(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- ⏱ frame cost ----
        if self.show_perf_panel {
            let mut open = true;
            egui::Window::new("⏱ Frame cost")
                .open(&mut open)
                .default_width(320.0)
                .show(ui, |ui| {
                    perf_readout(ui, &out.perf_snapshot);
                });
            if !open {
                self.show_perf_panel = false;
                out.perf_toggle = Some(false);
            }
        }

    }

    /// 🌐 multiplayer harness (Host & Join locally)
    #[allow(clippy::too_many_arguments)]
    fn ui_multiplayer_harness(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        player_mode: bool,
        playing: bool,
        net: &NetSnapshot,
    ) {
        let net_hosting = net.net_hosting;
        let net_has_client = self.net_client.is_some();
        // Per-player pings, host side — what a relay could never report.
        let net_peer_rtts = self.net_server.as_ref().map(|s| s.peer_rtts()).unwrap_or_default();
        let net_predicted_name = self
            .net_predictor
            .as_ref()
            .and_then(|(e, _)| self.world.get::<Name>(*e).map(|n| n.0.clone()));
        // Rollback health (docs/multiplayer.md §7 P6): a fighting
        // game's connection quality is rollback depth and mispredict rate, not
        // ping — and the stall indicator is the one readout a player needs,
        // because a stalled sim looks like the game running slightly slow and
        // is otherwise indistinguishable from a bad frame rate.
        let net_rollback = self.net_rollback.as_ref().map(|d| {
            crate::rollback_session::RollbackStats::with_session(
                d,
                self.net_server.as_ref().or(self.net_play_client.as_ref()),
            )
        });
        // (referee tick, live tick) — how far behind the authoritative sim is.
        let referee = self
            .net_referee
            .as_ref()
            .map(|r| (r.tick(), self.net_rollback.as_ref().map(|d| d.net.current()).unwrap_or(0)));
        // Interest management is the one feature whose job is to not send
        // things, so with no readout it is indistinguishable from a bug: set
        // the radius too tight and distant objects quietly stop moving, with
        // nothing anywhere saying why. `None` when it's off, which is a
        // different statement from "on, and culling nothing".
        let net_interest = self.net_server.as_ref().and_then(|s| {
            let cfg = s.interest();
            cfg.enabled.then(|| (cfg, s.interest_stats()))
        });
        // Voice chat: `None` when nothing is captured or heard,
        // which is a different statement from "on, and silent". A voice that
        // is quiet because the jitter buffer is starving looks exactly like a
        // player who stopped talking, and these are the numbers that tell them
        // apart.
        let net_voice = self.voice.active().then(|| {
            (self.voice.diagnostics(), self.voice.mic_summary())
        });
        let voice_test_peer = self.voice.test_speaker();
        let net_lobby_code = self.net_lobby_code.clone();
        // ---- 🌐 multiplayer harness (Host & Join locally) ----
        if self.show_net_panel {
            let mut open = true;
            egui::Window::new("🌐 Multiplayer")
                .open(&mut open)
                .default_width(280.0)
                .show(ui, |ui| {
                    if !playing {
                        ui.label("Enter Play mode, then host or join a session here.");
                        ui.small(
                            "Test alone (a hidden ghost client over a simulated link), \
                             or for real: host on a UDP port and a friend with this \
                             project joins over the network.",
                        );
                        return;
                    }
                    net_pings_ui(ui, net_hosting, &net_peer_rtts);
                    net_interest_ui(ui, net_interest.as_ref());
                    net_voice_ui(ui, out, net_voice.as_ref(), voice_test_peer);
                    net_rollback_ui(ui, net_rollback.as_ref(), referee);
                    net_replays_ui(ui, &net.replays, out);
                    net_impair_ui(ui);
                    self.net_session_ui(ui, out, net, player_mode, net_has_client, net_predicted_name, net_lobby_code);
                });
            if !open {
                self.show_net_panel = false;
            }
        }

    }

    /// Host or join — locally, by relay code, or over UDP — and, once in a
    /// session, the readouts and the way out.
    #[allow(clippy::too_many_arguments)]
    fn net_session_ui(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        net: &NetSnapshot,
        player_mode: bool,
        net_has_client: bool,
        net_predicted_name: Option<String>,
        net_lobby_code: Option<String>,
    ) {
        let NetSnapshot { net_hosting, net_peer_count, net_as_player, net_rtt, net_pred_stats, net_late_inputs, net_is_real, .. } = *net;
        if net_as_player {
            ui.label(format!(
                "🎮 you are a REMOTE PLAYER · rtt {net_rtt:.0} ms"
            ));
            match &net_predicted_name {
                Some(n) => ui.small(format!(
                    "predicting \"{n}\" locally — orange ghosts = the hidden server's truth. Raise latency/loss and feel it stay responsive."
                )),
                None => ui.small(
                    "spectating (no Predicted node) — give your character a Networked component with mode 'Predicted (owner)'",
                ),
            };
            if let Some((corr, conf, last)) = net_pred_stats {
                let total = corr + conf;
                let pct = if total > 0 {
                    100.0 * corr as f64 / total as f64
                } else {
                    0.0
                };
                ui.small(format!(
                    "reconciles: {conf} confirmed · {corr} corrected ({pct:.0}%) · last error {:.0} mm · late inputs {net_late_inputs}",
                    last * 1000.0
                ))
                .on_hover_text("healthy prediction: corrections near 0%, late inputs near 0 (a brief burst right after dragging the latency slider is normal — the server pauses to refill the input pipeline). Constant growth = the sims disagree — report it");
            }
        } else {
            match (net_hosting, net_has_client) {
                (false, _) => {
                    // The simulated-link harness is an editor
                    // dev tool — a build's menu is just the
                    // real hosting/joining flows.
                    if !player_mode {
                        ui.label("Test alone (simulated link)");
                        if ui.button("⏵ Host + join a local client").clicked() {
                            out.cmd.net_host_local = true;
                            out.cmd.net_join_local = true;
                        }
                        if ui
                            .button("🎮 Test as remote player (predicted)")
                            .on_hover_text("the play world becomes a CLIENT predicting against a hidden authoritative server — your character stays responsive at any latency, the server keeps the truth")
                            .clicked()
                        {
                            out.cmd.net_play_as_client = true;
                        }
                        ui.separator();
                    }
                    ui.label(if player_mode {
                        "Host — friends join with a lobby code"
                    } else {
                        "Real network — via relay (lobby codes)"
                    });
                    ui.horizontal(|ui| {
                        ui.label("relay");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.net_relay_addr)
                                .desired_width(150.0)
                                .hint_text("cloud, or host:port"),
                        )
                        .on_hover_text("`cloud` = Floptle Cloud (the code says which region). A host:port = your own floptle-relay.");
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .button("⏵ Host — get a lobby code")
                            .on_hover_text("registers a lobby on the relay above and shows a CODE for friends: six characters on Floptle Cloud, five on your own floptle-relay. Nobody port-forwards.")
                            .clicked()
                        {
                            out.cmd.net_host_relay = Some(self.net_relay_addr.clone());
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("code");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.net_join_code)
                                .desired_width(70.0)
                                .hint_text("UABCDE"),
                        );
                        if r.changed() {
                            self.net_join_code = self.net_join_code.to_uppercase();
                        }
                        let ok = !self.net_join_code.trim().is_empty();
                        if ui
                            .add_enabled(ok, egui::Button::new("⏵ Join by code"))
                            .on_hover_text("joins the lobby with this code — a Floptle Cloud code finds its relay by its first letter; with a host:port above, through that relay")
                            .clicked()
                        {
                            out.cmd.net_join_quic = Some(crate::net::lobby_join_target(
                                &self.net_relay_addr,
                                &self.net_join_code,
                            ));
                        }
                    });
                    ui.separator();
                    ui.label("Real network — direct (LAN / self-host)");
                    ui.horizontal(|ui| {
                        ui.label("port");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.net_host_port)
                                .desired_width(60.0),
                        );
                        if ui.button("⏵ Host on LAN").clicked() {
                            out.cmd.net_host_quic =
                                Some(self.net_host_port.trim().parse().unwrap_or(7777));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.net_join_addr)
                                .desired_width(170.0)
                                .hint_text("quic://ip:port"),
                        );
                        if ui.button("⏵ Join").clicked() {
                            out.cmd.net_join_quic = Some(self.net_join_addr.clone());
                        }
                    });
                    ui.small(
                        "both machines run THIS project. Player slots = the \
                         scene's Predicted nodes in order (#1 the host, #2+ \
                         joiners) — or spawn one per joiner (player_spawner.lua). \
                         Scripts: net.host{relay=\"cloud\"} / net.join(\"cloud://CODE\")",
                    );
                }
                (true, false) if !net_is_real => {
                    ui.label("hosting · 0 ghost clients");
                    if ui.button("➕ Join a local ghost client").clicked() {
                        out.cmd.net_join_local = true;
                    }
                }
                _ => {
                    ui.label(format!(
                        "hosting · {net_peer_count} client(s) connected"
                    ));
                    if let Some(code) = &net_lobby_code {
                        ui.horizontal(|ui| {
                            ui.label("lobby code:");
                            ui.add(egui::Label::new(
                                egui::RichText::new(code).strong().monospace(),
                            ).selectable(true));
                            if ui.small_button("copy").clicked() {
                                ui.ctx().copy_text(code.clone());
                            }
                        });
                    }
                    if net_is_real && net_peer_count > 0 {
                        ui.small(format!("late inputs {net_late_inputs} — near zero is healthy"));
                    }
                }
            }
        }
        if net_hosting || net_as_player {
            ui.separator();
            if net_is_real {
                ui.label("real link (QUIC)");
                ui.small("latency and loss are whatever the network gives you — the sliders only shape the simulated harness");
            } else {
                ui.label("simulated link");
                let mut lat = self.net_latency_ticks as i32;
                if ui
                    .add(egui::Slider::new(&mut lat, 0..=30).text("latency (ticks)"))
                    .on_hover_text("one-way, in gameplay ticks — 6 ticks ≈ 100 ms round trip")
                    .changed()
                {
                    self.net_latency_ticks = lat as u64;
                }
                ui.add(
                    egui::Slider::new(&mut self.net_loss, 0.0..=0.9)
                        .text("packet loss")
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                ui.checkbox(&mut self.net_ghosts, "show client ghosts (cyan)")
                    .on_hover_text("where the ghost client believes every networked node is — the gap to the real object is the interp delay");
            }
            ui.separator();
            if ui.button("⏹ End session").clicked() {
                out.cmd.net_stop_session = true;
            }
        }
    }

    /// net-stats overlay: one compact line while a session runs, so
    #[allow(clippy::too_many_arguments)]
    fn ui_net_stats_overlay(
        &mut self,
        ui: &mut egui::Ui,
        playing: bool,
        net: &NetSnapshot,
    ) {
        let NetSnapshot { net_hosting, net_peer_count, net_as_player, net_rtt, net_pred_stats, net_late_inputs, net_is_real, .. } = net.clone();
        // Client-side input timing, from the server's InputAck feedback —
        // the only place a joiner can see whether its inputs run late.
        let net_input_ack = self.net_play_client.as_ref().and_then(|c| c.input_ack());
        // ---- net-stats overlay: one compact line while a session runs, so
        // connection health is visible without the 🌐 panel open ----
        if playing && (net_hosting || net_as_player) {
            egui::Area::new(egui::Id::new("net_stats_overlay"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::RIGHT_TOP, [-10.0, 40.0])
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        let kind = if net_is_real { "net" } else { "sim" };
                        let mut line = if net_as_player {
                            let timing = net_input_ack
                                .map(|(margin, late)| {
                                    format!(" · input margin {margin:+} · late in {late}")
                                })
                                .unwrap_or_default();
                            format!("🌐 client ({kind}) · rtt {net_rtt:.0} ms{timing}")
                        } else {
                            format!(
                                "🌐 host ({kind}) · {net_peer_count} peer(s) · late in {net_late_inputs}"
                            )
                        };
                        if let Some((corr, conf, last)) = net_pred_stats {
                            let total = corr + conf;
                            let clean =
                                if total > 0 { 100.0 * conf as f64 / total as f64 } else { 100.0 };
                            line.push_str(&format!(
                                " · predict {clean:.0}% clean · err {:.0} mm",
                                last * 1000.0
                            ));
                        }
                        ui.small(line);
                    });
                });
        }

    }

    /// player-mode hint: the only chrome a build shows, and only
    fn ui_player_mode_hint(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        net_as_player: bool,
        net_hosting: bool,
        player_mode: bool,
    ) {
        let play_t = self.play_t;
        // ---- player-mode hint: the only chrome a build shows, and only
        // for the first seconds (until the UI system gives games real menus) ----
        if player_mode && play_t < 8.0 && !(net_hosting || net_as_player) {
            egui::Area::new(egui::Id::new("player_hint"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -14.0])
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.small("F1 — multiplayer");
                    });
                });
        }

        // The 🌐 panel's test-voice buttons, acted on out here where the
        // borrow of the panel's destructured fields has ended.
        if out.voice_test_stop {
            self.voice.stop_test_speaker();
        }
        if out.voice_test_pick && self.voice_test_pick.is_none() {
            self.voice_test_pick = Some(crate::native_dialog::pick_files_filtered(
                "Play a WAV as a remote player's microphone",
                Some(("audio", &floptle_audio::AUDIO_EXTENSIONS
                    .iter()
                    .map(|e| (*e).to_string())
                    .collect::<Vec<_>>())),
                false,
            ));
        }

    }

    /// dockable panels: Hierarchy / Inspector / Assets / Scene + Scripting
    #[allow(clippy::too_many_arguments)]
    fn ui_dock(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        code_theme: usize,
        game_offscreen: bool,
        gizmo_tool: Tool,
        map_mode: MapSubMode,
        player_mode: bool,
        playing: bool,
        pointer_down: bool,
        ppp: f32,
        preview_view: &Option<crate::PreviewView>,
        project_root: &Path,
        scene_name: &String,
        tool: Tool,
    ) {
        let dock_state = self.dock_state.get_or_insert_with(default_dock);
        // ⚙ Settings tab inputs. Only gathered when the tab is actually open,
        // so a closed Settings tab costs nothing per frame.
        let settings_open = dock_state.find_tab(&crate::dock::EditorTab::Settings).is_some();
        let settings_scene_files = if settings_open {
            crate::project::scene_files_in(&self.project_root)
        } else {
            Vec::new()
        };
        let settings_pad_names =
            if settings_open { self.pads.slot_names() } else { Vec::new() };
        let (settings_input_map, settings_input_pending) = {
            let sys = self.script_host.input_system().borrow();
            if settings_open {
                (sys.map().clone(), sys.pending_rebind().cloned())
            } else {
                (floptle_input::InputMap::default(), None)
            }
        };
        // Every named entity, Matter nodes and the Lighting node alike.
        let entity_names: Vec<(Entity, String)> =
            self.world.query::<Name>().map(|(e, n)| (e, n.0.clone())).collect();
        // Read before `self` is split into the panel context's borrows.
        let gi_status = crate::gi_bake::gi_status(
            &self.world,
            self.gi_bake.as_ref(),
            self.gi_baked.as_ref(),
            self.gi_show_only,
            self.gi_show_probes,
        );
        let nav_status = crate::nav_bake::nav_status(
            &self.world,
            crate::nav_bake::nav_node(&self.world).as_ref().map(|(_, m)| m),
            crate::nav_bake::NavHeld {
                mesh: self.nav_baked.as_ref(),
                seconds: self.nav_seconds,
                triangles: self.nav_triangles,
                file: self.nav_loaded_from.as_deref(),
                baking: self.nav_job.is_some(),
                coverage: self.nav_coverage.as_ref(),
            },
            &self.project_root,
        );
        // Bone names per rigged Mesh entity (name + parent index) — for the hierarchy's
        // expandable sub-objects and the inspector's bone-attach picker. Built read-only
        // before the borrow split so the UI never touches the mesh registry itself.
        let bone_names: HashMap<Entity, Vec<crate::RigNode>> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Mesh { asset_path } => self
                    .mesh_registry
                    .get(asset_path)
                    .and_then(|a| a.rig.as_ref())
                    .map(|rig| {
                        let nodes = rig
                            .skeleton
                            .nodes
                            .iter()
                            .enumerate()
                            .map(|(i, n)| crate::RigNode {
                                name: n.name.clone(),
                                parent: n.parent,
                                is_object: rig.node_is_object.get(i).copied().unwrap_or(true),
                            })
                            .collect();
                        (e, nodes)
                    }),
                _ => None,
            })
            .collect();
        let map_arm = self.map_arm;
        let map_knife_on = self.map_knife_on;
        let map_tool_on = self.tool == Tool::MapEdit;
        let map_playing = self.playing;
        // Copied, not borrowed: it is read by panels that also hold &mut borrows
        // of half the editor, and it does not change during the dock draw.
        let focused_tab = self.focused_tab;
        // Copied out before the mutable borrow below: the panels read the lock,
        // and ask for the flip through `cmd`.
        let selection_locked = self.selection_locked;
        // Labels only — the parked documents themselves stay on the Editor, so
        // the tab strip can name them without the panel being able to reach into
        // another document's undo stack.
        let image_parked: Vec<String> =
            self.image_stash.iter().map(|s| s.tab_label()).collect();
        let terrain_present = !self.terrains.is_empty();
        // Terrain 2.0 stats: volumes, resident data chunks, resident bytes — the
        // honest sparse numbers (the dense field's O(n³) voxel count is gone).
        let terrain_stats = (!self.terrains.is_empty()).then(|| {
            let chunks: usize = self.terrains.values().map(|t| t.field.data_chunks()).sum();
            let bytes: usize = self.terrains.values().map(|t| t.field.memory_bytes()).sum();
            (self.terrains.len(), chunks, bytes)
        });
        let has_active_camera = floptle_core::active_camera(&self.world).is_some();
        // The selected camera's POV preview texture (only when a camera is selected).
        let cam_preview = self.selection
            .last()
            .copied()
            .filter(|&e| matches!(self.world.get::<Matter>(e), Some(Matter::Camera { .. })))
            .and(self.cam_preview.as_ref().map(|p| p.tex_id));
        let particles_active = crate::dock::tab_is_front(dock_state, EditorTab::Particles);
        let game_tex = self.game_vp.as_ref().map(|p| p.tex_id);
        let layer_names = self.project.build_layers().names;
        let sorting_names = self.project.sorting_order();
        // The package extensions and their window. `ext_host` is handed to the
        // dock (its Scene overlays draw in the viewport), and used again after
        // for the floating panels — sequentially, so one `&mut` covers both.
        // What the last load found, read off the host before it is borrowed
        // mutably for the tab viewer — the 📦 Packages tab draws from inside
        // that viewer and cannot hold a second borrow of the host itself.
        let pkg_load = crate::packages_ui::PkgLoad::of(&self.ext);
        let ext_project_root = self.project_root.clone();
        // The gizmo menu's checkbox writes this directly; remember it so the change can
        // be persisted after the dock UI runs.
        let game_gizmos_before = self.game_gizmos;
        let grabbed = self.grabbed;
        let ui_overlay_snapshot = self.ui_overlay.clone();
        let ui_canvas_snapshot = self.ui_canvas.clone();
        // Accessibility is `Copy`, so the tab edits a copy and reports back
        // — no field borrow to thread through the tab viewer.
        let access = self.access;
        // ---- dockable panels: Hierarchy / Inspector / Assets / Scene + Scripting ----
        // The Scene tab is transparent so the 3D render shows through; the others
        // paint opaque over it. Users can drag/re-dock/tab these freely.
        //
        // Clear the Scene rect first: egui_dock only runs the active tab's `ui`,
        // so if Scene is tabbed behind Scripting, scene_ui never runs and the rect
        // would otherwise stay pinned to the old viewport region — letting clicks,
        // context-menus and model-drops fall through onto whatever panel now
        // occupies that space. `scene_ui` re-arms it only on frames it draws.
        self.scene_rect = None;
        let mut viewer = EditorTabViewer {
            world: &mut self.world,
            selection: &mut self.selection,
            selection_locked,
            maps: &self.maps,
            map_sel: &self.map_sel,
            map_mode,
            map_slot_name: &mut self.map_slot_name,
            map_viz: &self.map_viz,
            tile_viz: &self.tile_viz,
            map_opts: &mut self.map_opts,
            tiles: &mut self.tiles,
            tile_tools: &mut self.tile_tools,
            map_size_buf: &mut self.map_size_buf,
            map_spec_buf: &mut self.map_spec_buf,
            map_arm,
            map_knife_on,
            map_orient: &mut self.map_orient,
            map_xform: &mut self.map_xform,
            map_select_hidden: &mut self.map_select_hidden,
            map_bevel: &mut self.map_bevel,
            map_tool_on,
            map_playing,
            light_counts: self.light_counts,
            map_hud_open: &mut self.map_hud_open,
            map_keys: &mut self.map_keys,
            map_rebind: &mut self.map_rebind,
            map_rebind_err: &mut self.map_rebind_err,
            gizmo_tool,
            ui_overlay: &ui_overlay_snapshot,
            ui_canvas: &ui_canvas_snapshot,
            ref_kinds: &self.ref_kinds,
            script_meta: &mut self.script_meta,
            bone_selection: &mut self.bone_selection,
            pivot_edit: &mut self.pivot_edit,
            fullscreen_tab: &mut self.fullscreen_tab,
            focused_tab,
            hier_search: &mut self.hier_search,
            hier_scope: &mut self.hier_scope,
            collapsed: &mut self.collapsed,
            hier_fold_pending: &mut self.hier_fold_pending,
            bone_names: &bone_names,
            console: &mut self.console,
            preview: preview_view.clone(),
            preview_zoom: &mut self.preview_zoom,
            preview_spin: &mut self.preview_spin,
            preview_spinning: &mut self.preview_spinning,
            preview_material: &mut self.preview_material,
            map_asset_preview: &mut self.map_asset_preview,
            entity_names: &entity_names,
            gi: gi_status,
            nav: nav_status.clone(),
            materials: &self.materials,
            mat_name_buf: &mut self.mat_name_buf,
            flsl_cache: &self.flsl_cache,
            ui_flsl_cache: &self.ui_flsl_cache,
            post_flsl_cache: &self.post_flsl_cache,
            ui_styles: &self.ui_styles,
            ui_tokens: &self.ui_tokens,
            ui_design: &mut self.ui_design,
            sdf_cache: &self.sdf_cache,
            sky_uniforms: self.sky_shader.as_ref().map_or(&[], |(_, _, u)| u.as_slice()),
            component_clip: &self.component_clip,
            add_component_filter: &mut self.add_component_filter,
            layer_names: &layer_names,
            sorting_names: &sorting_names,
            tag_edit: &mut self.tag_edit,
            hier_scrolled: &mut self.hier_scrolled,
            hier_revealed: &mut self.hier_revealed,
            place_align: &mut self.place_align,
            asset_tree: &self.asset_tree,
            texture_settings: &self.texture_settings,
            cam_preview,
            has_active_camera,
            vertex_brush: &mut self.vertex_brush,
            terrain_brush: &mut self.terrain_brush,
            terrain_voxel: &mut self.terrain_voxel,
            terrain_textures: &mut self.terrain_textures,
            terrain_glow: &mut self.terrain_glow_mask,
            terrain_tex_scale: &mut self.terrain_tex_scale,
            terrain_present,
            terrain_stats,
            assets_grid: &mut self.assets_grid,
            assets_grid_dir: &mut self.assets_grid_dir,
            asset_thumbs: &mut self.asset_thumbs,
            material_view: &mut self.material_view,
            project_root,
            selected_asset: &mut self.selected_asset,
            asset_selection: &mut self.asset_selection,
            ide: &mut self.ide,
            learn: &mut self.learn,
            script_errors: self.script_errors.as_slice(),
            ide_diag: self.ide_diag.as_ref(),
            gizmo: self.gizmo.as_ref(),
            terrain_viz: self.terrain_viz.as_ref(),
            paint_viz: self.paint_viz.as_ref(),
            camera_gizmos: self.camera_gizmos.as_slice(),
            light_gizmos: self.light_gizmos.as_slice(),
            volume_gizmos: self.volume_gizmos.as_slice(),
            rig_gizmos: self.rig_gizmos.as_slice(),
            gi_probe_dots: self.gi_probe_dots.as_slice(),
            body_gizmos: self.body_gizmos.as_slice(),
            contact_gizmos: self.contact_gizmos.as_slice(),
            script_gizmo_lines: self.script_gizmo_lines.as_slice(),
            ext: &mut self.ext,
            ext_painted: self.ext_painted.as_slice(),
            game_gizmo_lines: self.game_gizmo_lines.as_slice(),
            game_gizmos: &mut self.game_gizmos,
            terrain_wire: self.terrain_wire_gizmo.as_slice(),
            nav_wire: self.nav_gizmo.as_slice(),
            mesh_wire: self.mesh_wire_gizmo.as_slice(),
            particle_gizmo: self.particle_gizmo.as_slice(),
            show_gizmos: &mut self.show_gizmos,
            panels: &mut self.panels,
            view_lock: &mut out.view_lock,
            view_ortho: &mut out.view_ortho,
            gizmo_filter: &mut self.gizmo_filter,
            grabbed,
            tool,
            scene_rect: &mut self.scene_rect,
            game_rect: &mut self.game_rect,
            game_offscreen,
            game_tex,
            aspect: &mut self.aspect_mode,
            zoom: &mut self.viewport_zoom,
            scene_name,
            editing_prefab: self.editing_prefab.is_some(),
            ppp,
            code_theme,
            anim: &mut self.anim,
            vfx: &mut self.vfx,
            vfx_ui: &mut self.vfx_ui,
            audio: &mut self.audio,
            mixer_ui: &mut self.mixer_ui,
            project: &mut self.project,
            particles_active,
            anim_ui: &mut self.anim_ui,
            shader_graph: &mut self.shader_graph,
            image: &mut self.image,
            image_parked: &image_parked,
            shader_preview: &mut self.shader_preview,
            mesh_registry: &self.mesh_registry,
            pointer_down,
            playing,
            player_mode,
            settings: crate::settings_ui::SettingsCtx {
                scene_files: &settings_scene_files,
                layer_new: &mut self.layer_new,
                section: &mut self.settings_section,
                search: &mut self.settings_search,
                input_map: &settings_input_map,
                input_pending: settings_input_pending.as_ref(),
                input_scan: &self.input_scan,
                input_test: &self.input_test_state,
                pad_names: &settings_pad_names,
                input_new_action: &mut self.input_new_action,
                access,
            },
            packages: &mut self.packages_ui,
            packages_ctx: crate::packages_ui::PkgCtx {
                project_root: &ext_project_root,
                load: &pkg_load,
                account: self.account.as_ref(),
            },
            packages_action: &mut out.pkg_action,
            cmd: &mut out.cmd,
        };
        // Fullscreen: one tab maximized over the whole window (double-click a tab to
        // toggle). A slim header lets you restore (or press Esc); the dock layout is
        // untouched underneath and comes back exactly as it was.
        if let Some(ft) = *viewer.fullscreen_tab {
            let mut exit = false;
            // A build has nothing to restore to — no header, and Escape
            // belongs to the game (cursor release), not the layout.
            if !player_mode {
                // A panel, not a bare `ui.horizontal`. A plain row paints no
                // background of its own, so the strip it occupied stayed
                // transparent and the 3D surface render showed through it —
                // a band of scene along the top edge of every maximized tab,
                // whichever tab it was. A panel fills itself, the same way
                // the menu bar above it always has.
                egui::Panel::top("fullscreen_header").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button(format!("⛶ Restore  ·  {}", ft.title()))
                            .on_hover_text(
                                "double-click a tab to toggle fullscreen · Esc to restore",
                            )
                            .clicked()
                        {
                            exit = true;
                        }
                        ui.small("double-click a tab or press Esc to restore");
                    });
                });
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    exit = true;
                }
            }
            // Scene/Game are transparent (the 3D shows through); every other tab
            // needs an opaque fill so the surface render doesn't bleed behind it.
            // Everything from here down belongs to the tab — take the whole
            // remaining rect rather than letting a stray margin leave a seam.
            let body = ui.available_rect_before_wrap();
            if !matches!(ft, EditorTab::Scene | EditorTab::Game) {
                let bg = ui.style().visuals.panel_fill;
                ui.painter().rect_filled(body, 0.0, bg);
            }
            let mut t = ft;
            let mut body_ui = ui.new_child(
                egui::UiBuilder::new().max_rect(body).layout(*ui.layout()),
            );
            egui_dock::TabViewer::ui(&mut viewer, &mut body_ui, &mut t);
            if exit {
                *viewer.fullscreen_tab = None;
            }
        } else {
            egui_dock::DockArea::new(dock_state)
                .style(egui_dock::Style::from_egui(ui.style()))
                .show_inside(ui, &mut viewer);
        }

        if *viewer.game_gizmos != game_gizmos_before {
            crate::prefs::save_game_gizmos(*viewer.game_gizmos);
        }
        // Where the Scene view's floating panels ended up. Compared against
        // what is on disk rather than written on every change, because a
        // drag is a change per frame and that would be a file write per
        // frame of it.
        let panels_now = *viewer.panels;
        if panels_now != self.panels_saved {
            crate::prefs::save_viewport_panels(&panels_now);
            self.panels_saved = panels_now;
        }
        // The Scene view's plane lock, chosen in the viewport toolbar.
        // `set_lock` snaps the camera square without moving it.
        // Read both out in one go: each is a `&mut` into the local below, so
        // the borrow has to be finished with before either can be written.
        let (chosen_lock, chosen_ortho) = (*viewer.view_lock, *viewer.view_ortho);
        out.view_lock = chosen_lock;
        out.view_ortho = chosen_ortho;

    }

    /// the packages' own panels
    fn ui_package_panels(
        &mut self,
        ui: &mut egui::Ui,
    ) {
        let ext_focus_window = self.ext_focus_window.take();
        // ---- the packages' own panels ----
        // Floating windows, like every other tool window here: they can be
        // moved and resized, they remember where they were put, and a
        // package cannot take a docked slot away from the editor's own
        // panels. Drawn after the dock, so a panel is over the viewport it
        // is about.
        for i in 0..self.ext.windows.len() {
            if !self.ext.windows[i].open {
                continue;
            }
            let title = self.ext.windows[i].title.clone();
            let id = self.ext.windows[i].id;
            let mut open = true;
            let win = egui::Window::new(&title)
                .id(egui::Id::new(("ext_window", id)))
                .open(&mut open)
                .default_width(320.0)
                .resizable(true);
            // `ed.window(...):focus()` brings the panel to the front. It
            // does not move it: a window that jumps to the middle of the
            // screen because a script mentioned it is a window somebody has
            // to put back.
            if ext_focus_window == Some(i) {
                ui.ctx().move_to_top(egui::LayerId::new(
                    egui::Order::Middle,
                    egui::Id::new(("ext_window", id)),
                ));
            }
            win.show(ui, |ui| self.ext.draw_window(i, ui));
            if !open {
                self.ext.set_window_open(i, false);
            }
        }

        // 📦 Packages is a dock tab now, drawn with the other tabs — see
        // `EditorTab::Packages`. Nothing to draw here.

    }

    /// what a package's `ed.message` asked to say
    fn ui_package_messages(
        &mut self,
        ui: &mut egui::Ui,
    ) {
        // ---- what a package's `ed.message` asked to say ----
        if let Some((title, body)) = self.ext_message.clone() {
            let mut open = true;
            let mut dismissed = false;
            egui::Window::new(&title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui, |ui| {
                    ui.label(&body);
                    dismissed = ui.button("OK").clicked();
                });
            if !open || dismissed {
                self.ext_message = None;
            }
        }

    }

    /// a package's keyboard shortcut
    fn ui_package_shortcuts(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- a package's keyboard shortcut ----
        // Read here rather than in the editor's own key handling so an
        // extension cannot fire while a text field has the keyboard.
        if !self.ext.shortcuts.is_empty() && !ui.ctx().egui_wants_keyboard_input() {
            let pressed = crate::ext_wire::pressed_shortcut(ui.ctx());
            if let Some(p) = pressed {
                out.ext_shortcut_click = self.ext.shortcuts.iter().position(|s| s.keys == p);
            }
        }

        // Vertex snapping: V held with the Move or Place tool, over the Scene.
        // Read from egui's key state, which also sees the release that lands
        // while a panel has the pointer.
        let over_scene = matches!(
            (ui.input(|i| i.pointer.hover_pos()), self.scene_rect),
            (Some(p), Some(r)) if r.contains(p)
        );
        self.vsnap_held = over_scene
            && !self.playing
            && !self.ctrl
            && matches!(self.tool, Tool::Move | Tool::Place)
            && !ui.ctx().egui_wants_keyboard_input()
            && ui.input(|i| i.key_down(egui::Key::V));
        if self.vsnap_held || self.vertex_drag.is_some() {
            self.paint_vertex_snap(ui.ctx());
        }

        // While a model, prefab or map is dragged over the Scene tab, mark where
        // it will land: a ring lying on the surface under the cursor.
        if let Some(p) = egui::DragAndDrop::payload::<AssetPayload>(ui.ctx())
            && (crate::assets::is_model(&p.path)
                || crate::assets::is_prefab(&p.path)
                || crate::assets::is_map_sidecar(&p.path))
            && let (Some(pos), Some(r)) = (ui.input(|i| i.pointer.hover_pos()), self.scene_rect)
            && r.contains(pos)
        {
            self.paint_landing_marker(ui.ctx());
        }

        // Viewport drop: spawn a model when an asset is released over the Scene
        // tab (panel drops — script-on-node — are consumed by those tabs first).
        // No opaque region is allocated, so the viewport never greys mid-drag.
        if egui::DragAndDrop::has_payload_of_type::<AssetPayload>(ui.ctx())
            && ui.input(|i| i.pointer.any_released())
        {
            let pos = ui.input(|i| i.pointer.interact_pos());
            let over_scene = matches!((pos, self.scene_rect), (Some(p), Some(r)) if r.contains(p));
            if over_scene
                && let Some(p) = egui::DragAndDrop::take_payload::<AssetPayload>(ui.ctx()) {
                    out.cmd.drop_asset = Some(p.path.clone());
                }
        }

    }

    /// Export Game… (File menu): binary + assets + manifest
    fn ui_export_game(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        export_base: &Path,
    ) {
        let export_building = self.export_job.is_some();
        let export_done = self.export_done.clone();
        // ---- Export Game… (File menu): binary + assets + manifest ----
        if self.show_export {
            let mut open = true;
            egui::Window::new("📦 Export Game")
                .open(&mut open)
                .resizable(false)
                .default_width(340.0)
                .show(ui, |ui| {
                    ui.label(
                        "A build = this engine binary + the project folder. It runs \
                         the game directly (no editor) — F1 in-game opens the \
                         multiplayer menu.",
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Title");
                        ui.text_edit_singleline(&mut self.export_title);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Folder");
                        ui.text_edit_singleline(&mut self.export_dir)
                            .on_hover_text("the build lands here (created if missing)");
                    });
                    // Exactly where that lands — no guessing at relative paths.
                    let resolved = {
                        let t = self.export_dir.trim();
                        let p = std::path::Path::new(t);
                        if p.is_absolute() { p.to_path_buf() } else { export_base.join(p) }
                    };
                    ui.small(format!("→  {}", resolved.display()));
                    ui.horizontal(|ui| {
                        ui.label("Target");
                        egui::ComboBox::from_id_salt("export_target")
                            .selected_text(EXPORT_TARGETS[self.export_target].label)
                            .show_ui(ui, |ui| {
                                for (i, t) in EXPORT_TARGETS.iter().enumerate() {
                                    ui.selectable_value(&mut self.export_target, i, t.label);
                                }
                            });
                    });
                    ui.small(
                        "any target exports from any machine: the engine binary for \
                         that platform is downloaded once (matched to this engine \
                         version, checksum-verified) and reused after that. No \
                         compiler or toolchain needed.",
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let can = !export_building && !self.export_dir.trim().is_empty();
                        if ui.add_enabled(can, egui::Button::new("📦 Export")).clicked() {
                            out.cmd.export_game =
                                Some((self.export_dir.trim().to_string(), self.export_target));
                        }
                        if export_building {
                            ui.spinner();
                        }
                    });
                    if let Some(status) = &self.export_status {
                        ui.add_space(4.0);
                        ui.label(status.as_str());
                    }
                    if let Some(done) = &export_done
                        && ui.button("📂 Open build folder").clicked()
                    {
                        out.cmd.open_folder = Some(done.clone());
                    }
                });
            if !open {
                self.show_export = false;
            }
        }

    }

    /// last run crashed
    fn ui_crash_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        let crash_prompt = self.crash_prompt.clone();
        // ---- last run crashed ----
        if let Some(note) = &crash_prompt {
            let first = note.lines().find(|l| l.starts_with("panic:")).unwrap_or("").to_string();
            egui::Window::new("⚠ Floptle crashed last time")
                .resizable(false)
                .collapsible(false)
                .default_width(460.0)
                .show(ui.ctx(), |ui| {
                    ui.label(
                        "The previous session ended in a crash. A report was saved — \
                         sending it is the single most useful thing you can do about it.",
                    );
                    if !first.is_empty() {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(&first).monospace().small());
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button("🐛 Report it")
                            .on_hover_text(
                                "opens the issue tracker with the version, platform and \
                                 backtrace already filled in — you can read and edit it \
                                 before posting. Nothing is sent automatically.",
                            )
                            .clicked()
                        {
                            out.cmd.crash_report = Some(true);
                        }
                        if ui.button("Not now").clicked() {
                            out.cmd.crash_report = Some(false);
                        }
                    });
                });
        }

    }

    /// autosave recovery (a newer autosave than the scene file)
    fn ui_autosave_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        let autosave_prompt = self.autosave_prompt.clone();
        let scene_name_now = self.scene_name.clone();
        // ---- autosave recovery (a newer autosave than the scene file) ----
        if let Some(auto) = &autosave_prompt {
            let age = floptle_vfs::modified(auto)
                .and_then(|t| t.elapsed().ok())
                .map(|d| {
                    let s = d.as_secs();
                    if s < 120 { format!("{s} s ago") } else { format!("{} min ago", s / 60) }
                })
                .unwrap_or_else(|| "recently".into());
            egui::Window::new("💾 Recover unsaved work?")
                .resizable(false)
                .collapsible(false)
                .default_width(360.0)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "'{scene_name_now}' has an AUTOSAVE newer than its saved file (written {age}) — usually the editor closed with unsaved changes. Restore it?"
                    ));
                    ui.small("Restoring loads the autosaved version (still unsaved — Ctrl+S to keep it). Discard deletes the autosave.");
                    ui.horizontal(|ui| {
                        if ui.button("♻ Restore autosave").clicked() {
                            out.cmd.autosave_action = Some(true);
                        }
                        if ui.button("🗑 Discard it").clicked() {
                            out.cmd.autosave_action = Some(false);
                        }
                    });
                });
        }

        // Project Settings is the ⚙ Settings dock tab (see `settings_ui.rs`):
        // draggable, dockable beside the viewport, searchable, and closed by
        // default.
    }

    /// preferences window (user-wide editor settings)
    fn ui_preferences_window(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        code_theme: usize,
        engine_theme: usize,
    ) {
        // ---- preferences window (user-wide editor settings) ----
        egui::Window::new("Preferences")
            .open(&mut self.show_preferences)
            .resizable(false)
            .default_width(320.0)
            .show(ui.ctx(), |ui| {
                ui.label("External editor — \"Open in IDE\"");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.external_editor)
                            .desired_width(150.0)
                            .hint_text("code"),
                    );
                    if ui.button("Save").clicked() {
                        out.cmd.set_external_editor = Some(self.external_editor.clone());
                    }
                });
                ui.small("Binary name or path (e.g. code, codium, subl). VSCode-family editors open the project folder and jump to the file. Saved as a user preference.");
                if ui
                    .checkbox(&mut self.prefer_external_editor, "Open scripts in my external editor")
                    .on_hover_text("When on, double-clicking a script (or its Edit button, or a console line) opens it here instead of the in-engine IDE.")
                    .changed()
                {
                    out.cmd.set_prefer_external = Some(self.prefer_external_editor);
                }

                ui.add_space(12.0);
                ui.label("Play-mode tint");
                ui.separator();
                let mut tint_changed = ui
                    .checkbox(&mut self.play_tint_enabled, "Tint the editor while playing")
                    .on_hover_text("Tints the editor chrome while in play mode so you never mistake it for edit mode (and lose edits on Stop).")
                    .changed();
                ui.add_enabled_ui(self.play_tint_enabled, |ui| {
                    // The stored value is an additive RGB offset, so editing it as a color
                    // reads naturally: black = no tint, brighter = a stronger nudge.
                    let mut col =
                        egui::Color32::from_rgb(self.play_tint[0], self.play_tint[1], self.play_tint[2]);
                    ui.horizontal(|ui| {
                        ui.label("tint amount");
                        if ui.color_edit_button_srgba(&mut col).changed() {
                            self.play_tint = [col.r(), col.g(), col.b()];
                            tint_changed = true;
                        }
                    });
                    ui.small("Color added to the editor background while playing (black = no tint).");
                    if ui.button("Reset to default").clicked() {
                        self.play_tint = DEFAULT_PLAY_TINT;
                        tint_changed = true;
                    }
                });
                if tint_changed {
                    out.cmd.set_play_tint = Some((self.play_tint_enabled, self.play_tint));
                }

                ui.add_space(12.0);
                ui.label("Themes");
                ui.separator();
                // Engine (chrome) theme.
                ui.horizontal(|ui| {
                    ui.label("Engine theme");
                    let cur = engine_theme.min(ENGINE_THEMES.len() - 1);
                    egui::ComboBox::from_id_salt("engine_theme_combo")
                        .selected_text(ENGINE_THEMES[cur].name)
                        .show_ui(ui, |ui| {
                            for (i, t) in ENGINE_THEMES.iter().enumerate() {
                                if ui.selectable_label(i == cur, t.name).clicked() {
                                    out.cmd.set_engine_theme = Some(i);
                                }
                            }
                        });
                });
                ui.small("Recolors the editor windows, panels and menus.");
                // Code-editor theme.
                ui.horizontal(|ui| {
                    ui.label("Editor theme");
                    let cur = code_theme.min(CODE_THEMES.len() - 1);
                    egui::ComboBox::from_id_salt("code_theme_combo")
                        .selected_text(CODE_THEMES[cur].name)
                        .show_ui(ui, |ui| {
                            for (i, t) in CODE_THEMES.iter().enumerate() {
                                if ui.selectable_label(i == cur, t.name).clicked() {
                                    out.cmd.set_code_theme = Some(i);
                                }
                            }
                        });
                });
                ui.small("Syntax colors + background of the in-engine script editor.");
            });

    }

    /// frame timing window
    fn ui_frame_timing_window(
        &mut self,
        ui: &mut egui::Ui,
        gpu_spans: &Vec<floptle_render::Span>,
        gpu_timing_supported: bool,
        gpu_total: f32,
    ) {
        // ---- frame timing window ----
        //
        // **The number that answers "why is this slow".** Wall-clock frame
        // time says a frame was slow; this says which pass was, which is the
        // only version of the question anybody can act on. Measured with GPU
        // timestamps, so it is time the card spent rather than time the CPU
        // spent asking — those differ by orders of magnitude and it is
        // routinely the second one that looks fine.
        egui::Window::new("⏱ Frame timing")
            .open(&mut self.gpu_timing_open)
            .resizable(false)
            .default_width(300.0)
            .show(ui.ctx(), |ui| {
                if !gpu_timing_supported {
                    ui.label("This GPU does not offer timestamp queries.");
                    ui.small(
                        "Nothing can be measured per pass here. The frame cost in the title \
                         bar still applies.",
                    );
                    return;
                }
                if gpu_spans.is_empty() {
                    ui.label("measuring…");
                    ui.small("a frame's timings arrive a frame or two after it is drawn");
                    return;
                }
                ui.horizontal(|ui| {
                    ui.strong(format!("{gpu_total:.2} ms"));
                    ui.small("on the GPU, this frame");
                });
                ui.separator();
                let worst = gpu_spans.iter().map(|s| s.ms).fold(0.0f32, f32::max).max(1e-4);
                for s in gpu_spans {
                    ui.horizontal(|ui| {
                        // A bar, because the ordering is the point: the eye
                        // finds the longest one without reading six numbers.
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(96.0, 12.0),
                            egui::Sense::hover(),
                        );
                        let vis = ui.visuals();
                        ui.painter().rect_filled(rect, 2.0, vis.extreme_bg_color);
                        let w = (s.ms / worst).clamp(0.0, 1.0) * rect.width();
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height())),
                            2.0,
                            vis.selection.bg_fill,
                        );
                        ui.label(format!("{:>6.2} ms", s.ms));
                        ui.label(&s.label);
                    });
                }
                ui.separator();
                ui.small(
                    "GPU time per pass. A frame the display is pacing shows a small total \
                     here and a large one in the title bar — that is the display waiting, \
                     not the scene costing.",
                );
            });

    }

    /// grid settings window
    fn ui_grid_settings_window(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- grid settings window ----
        egui::Window::new("Grid Settings")
            .open(&mut self.show_grid_settings)
            .resizable(false)
            .default_width(240.0)
            .show(ui.ctx(), |ui| {
                let mut changed = false;
                changed |= ui.checkbox(&mut self.grid.show, "show grid").changed();
                changed |= ui.checkbox(&mut self.grid.snap, "snap objects to grid").changed();
                changed |= ui.add(egui::Slider::new(&mut self.grid.size, 0.1..=10.0).text("cell size")).changed();
                changed |= ui.add(egui::Slider::new(&mut self.grid.extent, 4..=120).text("extent (cells)")).changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.grid.y_offset, 0.0..=50.0)
                            .text("drop below camera")
                            .suffix(" m"),
                    )
                    .on_hover_text("How far below the camera the grid floor sits. Your value is saved between sessions.")
                    .changed();
                changed |= ui.add(egui::Slider::new(&mut self.grid.alpha, 0.0..=1.0).text("opacity")).changed();
                ui.horizontal(|ui| {
                    ui.label("color");
                    changed |= ui.color_edit_button_rgb(&mut self.grid.color).changed();
                });
                if ui.small_button("Reset to defaults").clicked() {
                    self.grid = GridConfig::default();
                    changed = true;
                }
                // Persist the grid settings whenever a control changes (so they don't
                // reset every launch).
                if changed {
                    out.cmd.save_grid = true;
                }
            });

    }

    /// viewport context menu (RMB click on an object / empty space)
    fn ui_viewport_context_menu(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        map_mode: MapSubMode,
        tool: Tool,
    ) {
        let context_menu = self.context_menu;
        // ---- viewport context menu (RMB click on an object / empty space) ----
        if let Some((pos, hit)) = context_menu {
            egui::Area::new(egui::Id::new("ctx_menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(190.0);
                        // ---- ▦ Model tool: the operations for what's selected ----
                        //
                        // Right-click is where people look for "what can I do to
                        // this?" — every one of these was previously a key you had
                        // to already know, or a button on a panel that may not even
                        // be open. Same `MapOp`s the panel emits, so there is one
                        // implementation and no stale subset.
                        if tool == Tool::MapEdit {
                            let sel = self.map_sel.as_ref();
                            let nv = sel.map_or(0, |s| s.verts.len());
                            let ne = sel.map_or(0, |s| s.edges.len());
                            let nf = sel.map_or(0, |s| s.faces.len());
                            let any = nv + ne + nf > 0;
                            // Chosen here, applied after the closures — `cmd` is
                            // borrowed by the outer menu.
                            let mut pick: Option<crate::map_edit::MapOp> = None;
                            let mut detach = false;
                            let mut extrude_new = false;
                            let mut mode_pick: Option<crate::map_edit::MapSubMode> = None;
                            ui.label(
                                egui::RichText::new(match map_mode {
                                    crate::map_edit::MapSubMode::Vertex => format!("{nv} vertices"),
                                    crate::map_edit::MapSubMode::Edge => format!("{ne} edges"),
                                    crate::map_edit::MapSubMode::Face => format!("{nf} faces"),
                                })
                                .small()
                                .weak(),
                            );
                            ui.separator();
                            let op = |ui: &mut egui::Ui,
                                          label: &str,
                                          tip: &str,
                                          on: bool,
                                          o: crate::map_edit::MapOp,
                                          pick: &mut Option<crate::map_edit::MapOp>| {
                                if ui.add_enabled(on, egui::Button::new(label)).on_hover_text(tip).clicked() {
                                    *pick = Some(o);
                                }
                            };
                            if map_mode == crate::map_edit::MapSubMode::Face {
                                op(ui, "Extrude  (E)", "push the selected faces out along their average normal", nf > 0, crate::map_edit::MapOp::Extrude, &mut pick);
                                if ui.add_enabled(nf > 0, egui::Button::new("Extrude as new object  (')")).on_hover_text("grow a separate block out of the selected faces, leaving this mesh as it is").clicked() {
                                    extrude_new = true;
                                }
                                op(ui, "Inset  (I)", "a smaller copy of each face inside itself", nf > 0, crate::map_edit::MapOp::Inset, &mut pick);
                                op(ui, "Subdivide", "split each face into four", nf > 0, crate::map_edit::MapOp::Subdivide, &mut pick);
                                op(ui, "Bridge", "join two face outlines with a tube", nf == 2, crate::map_edit::MapOp::Bridge, &mut pick);
                                op(ui, "Flip", "reverse the winding — turn a face inside out", nf > 0, crate::map_edit::MapOp::FlipFaces, &mut pick);
                                if ui.add_enabled(nf > 0, egui::Button::new("Detach")).on_hover_text("split the selected faces off into their own map node").clicked() {
                                    detach = true;
                                }
                                op(ui, "Delete faces  (Del)", "remove them, leaving a hole", nf > 0, crate::map_edit::MapOp::DeleteFaces, &mut pick);
                            } else {
                                op(ui, "Weld selected", "merge vertices closer than the weld radius into one", nv > 1 || ne > 0, crate::map_edit::MapOp::WeldSelected, &mut pick);
                                op(ui, "Snap to grid", "move the selection onto the grid", any, crate::map_edit::MapOp::SnapToGrid, &mut pick);
                            }
                            ui.separator();
                            ui.menu_button("Select", |ui| {
                                op(ui, "All", "", true, crate::map_edit::MapOp::SelectAll, &mut pick);
                                op(ui, "None", "", any, crate::map_edit::MapOp::SelectNone, &mut pick);
                                op(ui, "Invert", "everything of this kind that isn't selected", true, crate::map_edit::MapOp::SelectInvert, &mut pick);
                                ui.separator();
                                op(ui, "Grow", "add the neighbouring ring", any, crate::map_edit::MapOp::Grow, &mut pick);
                                op(ui, "Shrink", "drop the outermost ring", any, crate::map_edit::MapOp::Shrink, &mut pick);
                                op(ui, "Linked", "everything connected to the selection", any, crate::map_edit::MapOp::SelectConnected, &mut pick);
                                op(ui, "Coplanar", "faces lying in the same plane", nf > 0, crate::map_edit::MapOp::SelectCoplanar, &mut pick);
                                op(ui, "Edge loop", "run along the quad loop", ne > 0, crate::map_edit::MapOp::SelectLoop, &mut pick);
                                ui.separator();
                                op(ui, "Warped faces", "faces whose corners no longer lie in one plane — the ones that look folded", true, crate::map_edit::MapOp::SelectNonPlanar, &mut pick);
                            });
                            ui.menu_button("Mode", |ui| {
                                for m in [
                                    crate::map_edit::MapSubMode::Vertex,
                                    crate::map_edit::MapSubMode::Edge,
                                    crate::map_edit::MapSubMode::Face,
                                ] {
                                    if ui.radio(map_mode == m, m.label()).clicked() {
                                        mode_pick = Some(m);
                                    }
                                }
                            });
                            if let Some(o) = pick {
                                out.cmd.map_op = Some(o);
                                out.cmd.close_menu = true;
                            }
                            if detach {
                                out.cmd.map_detach = true;
                                out.cmd.close_menu = true;
                            }
                            if extrude_new {
                                out.cmd.map_extrude_new = true;
                                out.cmd.close_menu = true;
                            }
                            if let Some(m) = mode_pick {
                                out.cmd.set_map_mode = Some(m);
                                out.cmd.close_menu = true;
                            }
                            ui.separator();
                        }
                        if hit.is_some() {
                            if ui.button("Duplicate  (Ctrl+D)").clicked() {
                                out.cmd.duplicate = true;
                                out.cmd.close_menu = true;
                            }
                            if ui.button("Copy  (Ctrl+C)").clicked() {
                                out.cmd.copy = true;
                                out.cmd.close_menu = true;
                            }
                            if ui.button("Delete  (Del)").clicked() {
                                out.cmd.delete = true;
                                out.cmd.close_menu = true;
                            }
                            ui.separator();
                        }
                        if ui.button("Paste  (Ctrl+V)").clicked() {
                            out.cmd.paste = true;
                            out.cmd.close_menu = true;
                        }
                        // The same node catalog as the Hierarchy's ✚ New and
                        // the menu-bar Add — one list, no stale subset.
                        ui.menu_button("Add", |ui| {
                            crate::hierarchy::node_new_menu(ui, &mut out.cmd, None);
                            out.cmd.close_menu |=
                                out.cmd.add.is_some() || out.cmd.add_ui.is_some();
                        });
                    });
                });
        }

    }

    /// new / open project window (rfd unavailable ⏵ a text path)
    fn ui_project_window(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- new / open project window (rfd unavailable ⏵ a text path) ----
        egui::Window::new("Project")
            .open(&mut self.show_project_mgr)
            .resizable(false)
            .default_width(420.0)
            .show(ui.ctx(), |ui| {
                ui.label("A project is a folder holding scenes/, models/, scripts/, …");
                ui.horizontal(|ui| {
                    ui.label("path");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.project_path_buf)
                            .desired_width(290.0)
                            .hint_text("/path/to/project"),
                    );
                });
                ui.horizontal(|ui| {
                    let p = self.project_path_buf.trim().to_string();
                    if ui.add_enabled(!p.is_empty(), egui::Button::new("Open")).clicked() {
                        out.cmd.project_action = Some(ProjectAction::Open(p.clone()));
                    }
                    if ui.add_enabled(!p.is_empty(), egui::Button::new("Create New")).clicked() {
                        out.cmd.project_action = Some(ProjectAction::New(p));
                    }
                });
                ui.add_space(4.0);
                ui.small("Open loads an existing folder; Create New scaffolds a fresh one.");
            });

    }

    /// rename modal (for the asset browser)
    fn ui_rename_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- rename modal (for the asset browser) ----
        if let Some((path, buf)) = self.rename_target.as_mut() {
            let mut open = true;
            let mut close = false;
            // The fixed suffix = everything after the first dot, so compound
            // extensions (.prefab.ron, .vfx.ron) ride along whole. Folders
            // have no suffix.
            let ext = if floptle_vfs::is_dir(Path::new(path.as_str())) {
                String::new()
            } else {
                Path::new(path.as_str())
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.find('.').map(|i| n[i..].to_string()))
                    .unwrap_or_default()
            };
            egui::Window::new("Rename")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(320.0)
                .show(ui.ctx(), |ui| {
                    ui.small(path.as_str());
                    // Edit just the base name; the extension rides along as a suffix.
                    let edit = ui
                        .horizontal(|ui| {
                            let e = ui.add(
                                egui::TextEdit::singleline(buf)
                                    .desired_width(240.0)
                                    .hint_text("name"),
                            );
                            if !ext.is_empty() {
                                ui.monospace(&ext);
                            }
                            e
                        })
                        .inner;
                    edit.request_focus();
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        let valid = !buf.trim().is_empty();
                        if ui.add_enabled(valid, egui::Button::new("Rename")).clicked() || (enter && valid) {
                            out.cmd.do_rename = Some((path.clone(), buf.clone()));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if !open || close {
                self.rename_target = None;
            }
        }

    }

    /// new scene modal
    fn ui_new_scene_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- new scene modal ----
        if let Some(buf) = self.new_scene_buf.as_mut() {
            let mut open = true;
            let mut close = false;
            egui::Window::new("New scene")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(300.0)
                .show(ui.ctx(), |ui| {
                    ui.label("Name your new blank scene:");
                    let edit = ui.add(
                        egui::TextEdit::singleline(buf).desired_width(260.0).hint_text("scene name"),
                    );
                    edit.request_focus();
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        let valid = !buf.trim().is_empty();
                        if ui.add_enabled(valid, egui::Button::new("Create")).clicked() || (enter && valid) {
                            out.cmd.new_scene = Some(buf.clone());
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if !open || close {
                self.new_scene_buf = None;
            }
        }

    }

    /// name a new asset
    fn ui_new_asset_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- name a new asset ----
        //
        // One modal for every "✚ New <thing>" that writes a file, so the
        // rule is the same everywhere: you name it, then it exists. The
        // words come from the kind; the mechanics (Enter to accept, Escape
        // or ✖ to cancel, empty is refused) do not vary.
        if let Some((kind, buf)) = self.new_asset_prompt.as_mut() {
            let (title, prompt, hint) = kind.words();
            let kind = *kind;
            let mut open = true;
            let mut close = false;
            egui::Window::new(title)
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(300.0)
                .show(ui.ctx(), |ui| {
                    ui.label(prompt);
                    let edit = ui.add(
                        egui::TextEdit::singleline(buf).desired_width(260.0).hint_text(hint),
                    );
                    edit.request_focus();
                    let enter =
                        edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        let valid = !buf.trim().is_empty();
                        if ui.add_enabled(valid, egui::Button::new("Create")).clicked()
                            || (enter && valid)
                        {
                            match kind {
                                crate::NewAsset::Effect(e) => {
                                    out.cmd.do_new_particles = Some((e, buf.clone()));
                                }
                            }
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if !open || close {
                self.new_asset_prompt = None;
            }
        }

    }

    /// quit with unsaved changes
    fn ui_quit_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
        scene_dirty_now: bool,
    ) {
        // The 🖼 tab keeps its own dirty flag — an unsaved image is unsaved
        // work, and quitting past it silently is the same loss as quitting past
        // a scene.
        let image_dirty_now = self.image.dirty && self.image.doc.is_some();
        // …and one that has never been written has no filename to save under,
        // so "Save & Quit" cannot silently do it: it has to ask first.
        let image_unnamed = image_dirty_now && self.image.path.is_none();
        // ---- quit with unsaved changes ----
        if self.show_quit_confirm {
            let mut open = true;
            let mut close = false;
            egui::Window::new("Unsaved changes")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(320.0)
                .show(ui.ctx(), |ui| {
                    match (scene_dirty_now, image_dirty_now) {
                        (true, true) => ui.label("The scene and the open image have unsaved changes."),
                        (true, false) => ui.label("The scene has unsaved changes."),
                        (false, true) => ui.label("The open image has unsaved changes."),
                        (false, false) => ui.label("Quit Floptle?"),
                    };
                    ui.horizontal(|ui| {
                        // Save & Quit: save everything, then close (the save runs after
                        // this closure, then `about_to_wait` exits — a real close, not a
                        // ViewportCommand, which is a no-op here).
                        let save_label =
                            if image_unnamed { "💾 Save…" } else { "💾 Save & Quit" };
                        if (scene_dirty_now || image_dirty_now)
                            && ui.button(save_label)
                                .on_hover_text(if image_unnamed {
                                    "the image has never been saved — it needs a name, so this \
                                     stays open"
                                } else {
                                    "save everything, then close"
                                })
                                .clicked()
                        {
                            out.want_save_all = true;
                            out.want_exit = !image_unnamed;
                            close = true;
                        }
                        // Discard: leave without saving.
                        if ui.button("Discard & Quit").clicked() {
                            out.want_exit = true;
                            close = true;
                        }
                        // Cancel: just dismiss — no save, no exit.
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if !open || close {
                self.show_quit_confirm = false;
            }
        }

    }

    /// closing an image with unsaved changes
    fn ui_close_image_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- closing an image with unsaved changes ----
        //
        // Three answers, because there are three things a person means.
        // The old code offered one — "save first" — and a document that has
        // never been named cannot be saved without a name, so that answer
        // was sometimes not available and the close simply never happened.
        // Discard is the arm that was missing, and it is the arm that turns
        // "I'm stuck editing this image" back into an ordinary decision.
        if let Some(which) = self.image_close_confirm {
            let mut decided = None;
            let mut open = true;
            egui::Window::new("Close this image?")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(340.0)
                .show(ui.ctx(), |ui| {
                    ui.label("This image has unsaved changes.");
                    ui.small(
                        "Saving writes the layered .flimg and the flat .png beside it.",
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        // Only offered for the live document: saving a parked
                        // one would mean making it live first, and a button
                        // that silently switches which image you are looking
                        // at is worse than not being there.
                        if which.is_none() && ui.button("💾  Save & close").clicked() {
                            decided = Some(1);
                        }
                        if ui
                            .button("🗑  Discard")
                            .on_hover_text("close it and lose the changes")
                            .clicked()
                        {
                            decided = Some(2);
                        }
                        if ui.button("Cancel").clicked() {
                            decided = Some(0);
                        }
                    });
                });
            if !open {
                decided = Some(0);
            }
            match decided {
                Some(0) => self.image_close_confirm = None,
                Some(1) => self.image_close_confirm = Some(which), // saved below, then closed
                Some(2) => {
                    self.image_close_confirm = None;
                    out.cmd.image_discard = Some(which);
                }
                _ => {}
            }
            if decided == Some(1) {
                self.image_close_confirm = None;
                out.cmd.image_save_then_close = true;
            }
        }

    }

    /// transient toast (save confirmation etc.) — top-center, fades out
    fn ui_transient_toast(
        &mut self,
        ui: &mut egui::Ui,
    ) {
        // ---- transient toast (save confirmation etc.) — top-center, fades out ----
        if let Some((msg, secs)) = self.toast.as_mut() {
            *secs -= ui.input(|i| i.stable_dt).min(0.1);
            if *secs <= 0.0 {
                self.toast = None;
            } else {
                let a = (*secs).clamp(0.0, 1.0); // fade over the last second
                egui::Area::new(egui::Id::new("save-toast"))
                    .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 48.0))
                    .interactable(false)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style())
                            .fill(egui::Color32::from_rgba_unmultiplied(30, 120, 60, (220.0 * a) as u8))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(msg.as_str())
                                        .color(egui::Color32::from_white_alpha((255.0 * a) as u8))
                                        .strong(),
                                );
                            });
                    });
            }
        }

    }

    /// delete asset confirmation (deletion is irreversible)
    fn ui_delete_asset_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- delete asset confirmation (deletion is irreversible) ----
        if let Some(paths) = self.delete_confirm.clone() {
            let mut open = true;
            let mut close = false;
            let name = |p: &String| {
                Path::new(p)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.clone())
            };
            egui::Window::new("Delete asset")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(340.0)
                .show(ui.ctx(), |ui| {
                    match paths.as_slice() {
                        [p] if floptle_vfs::is_dir(Path::new(p)) => {
                            ui.label(format!(
                                "Delete the folder \"{}\" and everything in it?",
                                name(p)
                            ));
                        }
                        [p] => {
                            ui.label(format!("Delete \"{}\"?", name(p)));
                        }
                        many => {
                            ui.label(format!("Delete these {} files?", many.len()));
                            for p in many.iter().take(8) {
                                ui.small(format!("  {}", name(p)));
                            }
                            if many.len() > 8 {
                                ui.small(format!("  …and {} more", many.len() - 8));
                            }
                        }
                    }
                    ui.small("This can't be undone.");
                    ui.horizontal(|ui| {
                        if ui.button("🗑 Delete").clicked() {
                            out.cmd.do_delete_asset = Some(paths.clone());
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if !open || close {
                self.delete_confirm = None;
            }
        }

    }

    /// collision layer: do the children come too?
    fn ui_collision_layer_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- collision layer: do the children come too? ----
        //
        // See `LayerChildrenPrompt` for why this is a question and not a
        // default. Both answers are offered as buttons that say what they
        // will do, with the counts in them — "Yes/No" on a dialog nobody
        // reads carefully is how the wrong one gets clicked every time.
        if let Some(pending) = self.layer_children_confirm.clone() {
            let mut open = true;
            let mut close = false;
            let n_targets = pending.targets.len();
            let n_kids = pending.children.len();
            egui::Window::new("Collision layer")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(380.0)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "{} has {} child node{} under it.",
                        if n_targets == 1 {
                            "This node".to_string()
                        } else {
                            format!("These {n_targets} nodes have")
                        },
                        n_kids,
                        if n_kids == 1 { "" } else { "s" },
                    ));
                    ui.small(format!(
                        "Put them on \"{}\" as well? A collider usually hangs under the \
                         node you just changed, so leaving the children behind is often \
                         why a layer change looks like it did nothing.",
                        pending.layer
                    ));
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .button(format!("⬇  Include the {n_kids} children"))
                            .clicked()
                        {
                            let mut all = pending.targets.clone();
                            all.extend_from_slice(&pending.children);
                            out.cmd.do_set_layer = Some(crate::SetLayer {
                                targets: all,
                                layer: pending.layer.clone(),
                            });
                            close = true;
                        }
                        if ui
                            .button(if n_targets == 1 {
                                "Just this node".to_string()
                            } else {
                                format!("Just these {n_targets}")
                            })
                            .clicked()
                        {
                            out.cmd.do_set_layer = Some(crate::SetLayer {
                                targets: pending.targets.clone(),
                                layer: pending.layer.clone(),
                            });
                            close = true;
                        }
                        // Closing the window is Cancel, and cancel changes
                        // nothing — the layer has not been written yet.
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if !open || close {
                self.layer_children_confirm = None;
            }
        }

    }

    /// new terrain dialog
    fn ui_new_terrain_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- new terrain dialog ----
        // Lets a fresh terrain arrive already the size/look you want (a tiny
        // rock-grey patch or a massive grass field) instead of always starting as
        // the same small default slab you'd otherwise have to sculpt/fill out by
        // hand — see NewTerrainCfg.
        if let Some(cfg) = self.new_terrain_cfg.as_mut() {
            let mut open = true;
            let mut close = false;
            egui::Window::new("New terrain")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(320.0)
                .show(ui.ctx(), |ui| {
                    ui.label("Footprint (X/Z) and thickness (Y), world units:");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut cfg.size_xz)
                                .range(0.5..=4000.0)
                                .speed(1.0)
                                .prefix("size ")
                                .suffix(" (x/z)"),
                        );
                        ui.add(
                            egui::DragValue::new(&mut cfg.thickness)
                                .range(0.2..=500.0)
                                .speed(0.5)
                                .prefix("thick ")
                                .suffix(" (y)"),
                        );
                    });
                    // The size/detail pair silently decides quality, and the old
                    // copy here ("set detail higher before sculpting a large one")
                    // Terrain 2.0: the field is sparse and unbounded — the dialog
                    // sizes a starting slab, and memory scales with the surface,
                    // not the volume. Show the honest estimate live.
                    let (chunks, mb) = crate::terrain_ui::new_terrain_preview(
                        cfg.size_xz,
                        cfg.thickness,
                        self.terrain_voxel,
                    );
                    ui.small(format!(
                        "→ voxel {:.2} units · ~{chunks} chunks · ~{mb:.1} MB (sparse — grows as you sculpt)",
                        self.terrain_voxel,
                    ));
                    ui.horizontal(|ui| {
                        ui.label("color");
                        ui.color_edit_button_rgb(&mut cfg.color);
                    });
                    ui.label("texture (optional — paints the whole slab)");
                    let mut tex_list = Vec::new();
                    collect_texture_paths(&self.asset_tree, &mut tex_list);
                    let cur_label = if cfg.texture.is_empty() {
                        "(none — flat color)".to_string()
                    } else {
                        Path::new(&cfg.texture)
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    };
                    egui::ComboBox::from_id_salt("new_terrain_tex")
                        .selected_text(cur_label)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(cfg.texture.is_empty(), "(none — flat color)")
                                .clicked()
                            {
                                cfg.texture.clear();
                            }
                            for p in &tex_list {
                                let n = Path::new(p)
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                if ui.selectable_label(&cfg.texture == p, n).clicked() {
                                    cfg.texture = p.clone();
                                }
                            }
                        });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Create").clicked() {
                            out.cmd.create_terrain = Some(cfg.clone());
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if !open || close {
                self.new_terrain_cfg = None;
            }
        }

    }

    /// open-scene unsaved-changes confirm
    fn ui_open_scene_prompt(
        &mut self,
        ui: &mut egui::Ui,
        out: &mut UiOut,
    ) {
        // ---- open-scene unsaved-changes confirm ----
        if let Some(path) = self.pending_open_scene.clone() {
            let name = Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            // One gate, both directions: a prefab replaces the world exactly
            // as thoroughly as a scene does, so it comes through here too and
            // only the wording differs.
            let kind = if crate::assets::is_prefab(&path) { "prefab" } else { "scene" };
            let name = name.trim_end_matches(".prefab").to_string();
            let mut keep = true;
            egui::Window::new("Unsaved changes")
                .open(&mut keep)
                .resizable(false)
                .collapsible(false)
                .default_width(320.0)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("Open {kind} \"{name}\"?"));
                    ui.label("The current scene has unsaved changes.");
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Save & open").clicked() {
                            out.cmd.do_open_scene = Some((path.clone(), true));
                            self.pending_open_scene = None;
                        }
                        if ui.button("Discard & open").clicked() {
                            out.cmd.do_open_scene = Some((path.clone(), false));
                            self.pending_open_scene = None;
                        }
                        if ui.button("Cancel").clicked() {
                            self.pending_open_scene = None;
                        }
                    });
                });
            if !keep {
                self.pending_open_scene = None;
            }
        }

    }

    /// who has the pointer --
    fn ui_who_has_the_pointer(
        &mut self,
        ui: &mut egui::Ui,
        player_mode: bool,
        playing: bool,
    ) {
        // Who owns the pointer this frame, for the Game-view hint. Read as plain
        // fields (not through `game_holds_cursor`) because the closure below only
        // ever holds disjoint field borrows and a `&self` method would collide.
        let cursor_held_by_game = self.game_trap || (self.script_mouse_lock && !self.cursor_freed);
        let cursor_held_by_editor = self.cursor_freed && self.script_mouse_lock;
        // ---- who has the pointer ------------------------------------------
        // A grabbed cursor is invisible by definition, so the one thing that
        // says how to get it back cannot itself be the cursor. Without this
        // the way out (Escape) was findable only by reading the source, and
        // what people did instead was alt-tab out of the whole application
        // to reach the Inspector.
        //
        // Only while playing, only over the Game view, and only when the
        // pointer is actually contested — a game that never grabs never
        // sees it.
        // Never in a shipped build: `player_mode` has no editor to hand the
        // pointer back to, so the hint names a negotiation that does not
        // exist there. The build's own way out is the player-mode hint
        // above, which says Escape once and then goes away.
        if playing
            && !player_mode
            && (cursor_held_by_game || cursor_held_by_editor)
            && let Some(r) = self.game_rect
        {
            let (msg, fg) = if cursor_held_by_editor {
                ("Click the game to give the mouse back", egui::Color32::from_rgb(150, 210, 255))
            } else {
                ("Esc — free the mouse", egui::Color32::from_rgb(215, 220, 230))
            };
            egui::Area::new(egui::Id::new("pointer_owner_hint"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(r.center().x, r.max.y - 34.0))
                .pivot(egui::Align2::CENTER_CENTER)
                // Purely a label: it must never eat the click that hands
                // the pointer back, which lands in this very corner.
                .interactable(false)
                .show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_black_alpha(150))
                        .corner_radius(9.0)
                        .inner_margin(egui::Margin::symmetric(10, 5))
                        .show(ui, |ui| ui.colored_label(fg, msg));
                });
        }

        // (Terrain tools live in the dockable Terrain tab now; the gizmo paints
        // inside the Scene tab, clipped to its rect.)
    }
}

#[cfg(feature = "editor-ui")]
/// Per-player pings, host side.
fn net_pings_ui(ui: &mut egui::Ui, net_hosting: bool, net_peer_rtts: &[(floptle_net::PeerId, f32)]) {
    if net_hosting && !net_peer_rtts.is_empty() {
        ui.small(
            net_peer_rtts
                .iter()
                .map(|(p, r)| format!("peer {p}: {r:.0} ms"))
                .collect::<Vec<_>>()
                .join(" · "),
        )
        .on_hover_text(
            "measured host↔player round trip. Probed end to end rather \
             than read off the transport, because through a relay the \
             transport only sees its own leg — it would report host↔relay \
             and call it the player's ping.",
        );
    }
}

#[cfg(feature = "editor-ui")]
/// Interest management, when the host turned it on: the radius, and what each client is not being sent.
fn net_interest_ui(
    ui: &mut egui::Ui,
    net_interest: Option<&(floptle_net::InterestConfig, Vec<(floptle_net::PeerId, floptle_net::InterestStat)>)>,
) {
    // ---- interest management, when the host turned it on ----
    if let Some((cfg, stats)) = net_interest {
        ui.separator();
        ui.label(format!(
            "👁 interest · {:.0} m radius · {} KB/s per client{}",
            cfg.radius,
            cfg.budget_bytes_per_sec / 1024,
            if cfg.occlusion { " · line of sight" } else { "" }
        ))
        .on_hover_text(
            "each client is told about its own neighbourhood instead of the \
             whole world. Nothing is dropped for good — what doesn't fit \
             the budget accrues priority and goes in a later snapshot.\n\n\
             A radius is a BANDWIDTH boundary, not a security one: a client \
             told where everyone within it is standing knows where they \
             are, whatever it draws. Line of sight is the part that answers \
             that — net.host{ interestOcclusion = \"Level\" }.",
        );
        if stats.is_empty() {
            ui.small("no clients yet — nothing to build a relevant set from");
        }
        for (peer, st) in stats {
            let line = format!(
                "peer {peer}: {} of {} sent · {} B{}{}",
                st.sent,
                st.relevant,
                st.bytes,
                if st.deferred > 0 {
                    format!(" · {} waiting", st.deferred)
                } else {
                    String::new()
                },
                // What this client is not being told, and
                // by which rule. "Is my filter working"
                // has to be a number, or a project turns
                // one on and cannot tell it from a typo.
                if st.withheld() > 0 {
                    format!(
                        " · withheld {} ({} far{}{})",
                        st.withheld(),
                        st.withheld_radius,
                        if st.withheld_occluded > 0 {
                            format!(", {} unseen", st.withheld_occluded)
                        } else {
                            String::new()
                        },
                        if st.withheld_filter > 0 {
                            format!(", {} by the game", st.withheld_filter)
                        } else {
                            String::new()
                        },
                    )
                } else {
                    String::new()
                }
            );
            // A backlog that never clears is the one shape
            // worth colouring: it means the budget cannot
            // keep up with the scene, and distant things
            // will visibly lag rather than merely update
            // less often.
            if st.deferred > st.sent && st.sent > 0 {
                ui.colored_label(egui::Color32::from_rgb(255, 170, 60), line)
                    .on_hover_text(
                        "more entities are waiting for a turn than got one. \
                         They are not lost — they accrue priority — but if \
                         this stays high, raise interestBudget or lower the \
                         radius.",
                    );
            } else {
                ui.small(line).on_hover_text(
                    "relevant = what this client may hear about at all; \
                     sent = what fit in the last snapshot's budget; \
                     withheld = replicable nodes it was told nothing \
                     about, split by which rule decided — out of range, \
                     out of sight, or net.setRelevant.",
                );
            }
        }
    }
}

/// One speaker's row in the voice readout: peer, buffered ms, cushion ms,
/// concealed frames, late packets.
#[cfg(feature = "editor-ui")]
type VoiceRow = (u64, f32, f32, u64, u64);

#[cfg(feature = "editor-ui")]
/// Voice chat, when anything is speaking or listening: the mic, and each voice's jitter buffer.
fn net_voice_ui(
    ui: &mut egui::Ui,
    out: &mut UiOut,
    net_voice: Option<&(Vec<VoiceRow>, String)>,
    voice_test_peer: Option<u64>,
) {
    // ---- voice chat, when anything is speaking or listening ----
    if let Some((rows, mic)) = net_voice {
        ui.separator();
        ui.label(format!("🎤 voice · {mic}")).on_hover_text(
            "the local microphone. The level meter is live whether or not \
             transmit is on, so a settings screen can prove the mic works \
             without joining a lobby.",
        );
        if rows.is_empty() {
            ui.small("nobody else is speaking");
        }
        // The harness microphone. Voice normally needs two
        // machines and two people to try at all; this makes
        // a WAV stand in for the far end, through the real
        // forwarding rules.
        ui.horizontal(|ui| {
            match voice_test_peer {
                Some(p) => {
                    if ui.small_button("⏹ stop test voice").clicked() {
                        out.voice_test_stop = true;
                    }
                    ui.small(format!("speaking as peer {p}"));
                }
                None => {
                    if ui
                        .small_button("🎤 test voice from a WAV…")
                        .on_hover_text(
                            "play an audio file in as though a remote \
                             player were speaking it — through the real \
                             forwarding rules, jitter buffer and spatial \
                             voice. Proves the routing without a second \
                             machine or a microphone.",
                        )
                        .clicked()
                    {
                        out.voice_test_pick = true;
                    }
                }
            }
        });
        for (peer, buffered, cushion, concealed, late) in rows {
            let line = format!(
                "peer {peer}: {buffered:.0} ms buffered (target {cushion:.0})\
                 {}{}",
                if *concealed > 0 {
                    format!(" · {concealed} concealed")
                } else {
                    String::new()
                },
                if *late > 0 { format!(" · {late} late") } else { String::new() }
            );
            // A cushion pinned at its ceiling means the link
            // is the problem, and it is the one shape worth
            // colouring: the voice still works, it is just
            // permanently 60 ms behind and will stay there.
            if *cushion >= 60.0 {
                ui.colored_label(egui::Color32::from_rgb(255, 170, 60), line)
                    .on_hover_text(
                        "the jitter buffer is as wide as it goes. Packets \
                         keep arriving late, so this speaker is held at the \
                         maximum delay to stop them dropping out.",
                    );
            } else {
                ui.small(line).on_hover_text(
                    "buffered = audio waiting to play; target = the cushion \
                     the jitter buffer is holding, which widens on lateness \
                     and shrinks again when the link settles. concealed = \
                     gaps Opus filled in for packets that never came.",
                );
            }
        }
    }
}

#[cfg(feature = "editor-ui")]
/// Rollback health: depth, mispredicts, the stall indicator, and how far behind the referee sim is.
fn net_rollback_ui(
    ui: &mut egui::Ui,
    net_rollback: Option<&crate::rollback_session::RollbackStats>,
    referee: Option<(u64, u64)>,
) {
    if let Some(rb) = net_rollback {
        ui.separator();
        if rb.stalled {
            ui.colored_label(
                egui::Color32::from_rgb(255, 170, 60),
                "⚔ ROLLBACK · waiting for input",
            )
            .on_hover_text(
                "past the depth cap the sim waits instead of guessing \
                 further: the game runs slightly slow rather than \
                 teleporting the opponent. It catches up on its own.",
            );
        } else {
            // Delay and mispredict rate on one line, because
            // neither means anything alone: a rollback
            // implementation working perfectly and one badly
            // misconfigured look identical from outside, and
            // "delay 2 — 99% guessed" is the whole diagnosis.
            let line = format!(
                "⚔ ROLLBACK · {} fighter(s) · delay {} · {:.0}% guessed",
                rb.fighters,
                rb.input_delay,
                rb.mispredict_rate * 100.0,
            );
            // Only once there is enough of a match to judge:
            // the opening ticks always guess.
            let bad = rb.mispredict_rate > 0.5 && rb.current > 120;
            if bad {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 170, 60),
                    line,
                )
                .on_hover_text(format!(
                    "almost every tick is being guessed and re-simulated. \
                     Nothing is broken — the fight is identical on both \
                     machines — but this peer is doing several times the \
                     work and it feels like it. The delay is too low for \
                     this link: raise it between matches with \
                     net.setInputDelay(n) (max {}), or set \
                     net.host{{ inputDelay = n }}.",
                    floptle_net::MAX_DELAY
                ));
            } else {
                ui.label(line);
            }
        }
        ui.small(format!(
            "corrections {} · depth last {} / max {} / avg {:.1} · \
             ring {} ticks / {} KB",
            rb.corrections,
            rb.last_depth,
            rb.max_depth_seen,
            rb.average_depth,
            rb.ring_ticks,
            rb.ring_bytes / 1024,
        ))
        .on_hover_text(
            "the delay is FIXED for the session — it never changes \
             mid-match, because how the game feels must not. These \
             numbers are the measurement you choose it from: a healthy \
             match sits at low average depth.",
        );
        // who is starved, and on what. A frozen match used
        // to look identical from both screens; this names
        // the side that stopped keeping up.
        ui.small(format!(
            "frontier · confirmed {} of {} simulated ({} ahead)",
            rb.confirmed,
            rb.current,
            rb.current.saturating_sub(rb.confirmed),
        ))
        .on_hover_text(
            "\"confirmed\" is the newest tick every peer's REAL input is \
             known for. Everything past it was simulated from a guess and \
             can still be corrected. When the gap reaches the depth cap \
             the sim stalls — so a gap pinned at the cap means someone's \
             input has stopped arriving.",
        );
        for (peer, frontier, backlog) in &rb.peers {
            let who = if *peer == floptle_net::SERVER {
                "host".to_string()
            } else {
                format!("peer {peer}")
            };
            // A backlog past the fan-out window is a peer
            // that has stopped confirming — the shape of a
            // starved or departed player, not of a slow one.
            let stuck = *backlog > 24;
            let line =
                format!("   {who} · frontier {frontier} · {backlog} tick(s) held");
            if stuck {
                ui.colored_label(egui::Color32::from_rgb(255, 170, 60), line)
                    .on_hover_text(
                        "this peer has stopped confirming ticks: the host \
                         is holding its inputs and re-sending them, and \
                         will keep doing so until they land. If it stays \
                         here, that peer is the one that fell out of the \
                         match.",
                    );
            } else {
                ui.small(line);
            }
        }
        // Checksum status. "Never checked" and "checked and
        // agreeing" are very different states to be in.
        if rb.desynced {
            ui.colored_label(
                egui::Color32::from_rgb(255, 90, 90),
                "⚠ DESYNCED — the peers no longer agree",
            )
            .on_hover_text(
                "from the reported tick on, the two machines are playing \
                 different matches. The Console names the tick. Usual \
                 causes: a gameplay value outside snapshot()/restore(), \
                 an unseeded rng() (use net.random()), or reading node.x \
                 inside fixedUpdate instead of node.tickPos.",
            );
        } else if rb.checksum_tick > 0 {
            ui.small(format!(
                "✔ checksums agree through tick {}",
                rb.checksum_tick
            ));
        } else {
            ui.small("checksums: none due yet (every 30 confirmed ticks)");
        }
        if let Some(rf) = referee {
            ui.small(format!(
                "⚖ referee at tick {} ({} behind)",
                rf.0,
                rf.1.saturating_sub(rf.0)
            ))
            .on_hover_text(
                "a second simulation of this match on the host, advanced \
                 only to ticks every peer's input has actually arrived \
                 for. It never guesses and never rolls back, so it is \
                 never wrong — only behind. Every peer's checksum is \
                 judged against it, which is the difference between \
                 \"someone is out of sync\" and \"THAT machine is\".",
            );
        }
        ui.separator();
    }
}

#[cfg(feature = "editor-ui")]
/// Recorded matches: a click plays one back.
fn net_replays_ui(ui: &mut egui::Ui, replays: &[(String, std::path::PathBuf)], out: &mut UiOut) {
    // Replays. A match's inputs and its seed are the match,
    // so a replay is kilobytes and playing it back is
    // re-simulation rather than re-enactment.
    if !replays.is_empty() {
        ui.small("🎞 replays");
        for (name, path) in replays {
            if ui
                .button(name.as_str())
                .on_hover_text(
                    "re-simulate this match in a headless second world. \
                     Enter Play on its scene first — a replay is the match \
                     run again, so it needs the world it was played in.",
                )
                .clicked()
            {
                out.cmd.net_play_replay = Some(path.clone());
            }
        }
        ui.separator();
    }
}

#[cfg(feature = "editor-ui")]
/// The dev-only link impairment knob.
fn net_impair_ui(ui: &mut egui::Ui) {
    // Dev-only rehearsal knob. The section only exists at
    // all when FLOPTLE_NET_IMPAIR was set on the command
    // line, so it cannot appear in front of someone who did
    // not ask for it — the whole point is that a real
    // session can never be silently degraded from the UI.
    if let Some(knob) = Editor::net_impair() {
        let mut imp = knob.get();
        let before = imp;
        let hot = imp.is_active();
        ui.colored_label(
            if hot {
                egui::Color32::from_rgb(255, 170, 60)
            } else {
                egui::Color32::GRAY
            },
            "⚠ LINK IMPAIRMENT (dev build)",
        )
        .on_hover_text(
            "adds latency and loss to THIS build's real transports (QUIC \
             and the relay) so a rollback match can be rehearsed at match \
             conditions between two instances on one desk. It is not a \
             network emulator — no jitter, no reordering — and it is not \
             a substitute for the two-machine acceptance run.",
        );
        let rtt = imp.rtt_ms();
        ui.add(
            egui::Slider::new(&mut imp.latency_ms, 0..=250)
                .text(format!("one-way ms  (≈{rtt} ms RTT)")),
        );
        let mut loss_pct = imp.loss * 100.0;
        if ui
            .add(egui::Slider::new(&mut loss_pct, 0.0..=25.0).text("% loss"))
            .changed()
        {
            imp.loss = loss_pct / 100.0;
        }
        if hot && ui.button("off").clicked() {
            imp = floptle_net::Impairment::default();
        }
        if imp != before {
            knob.set(imp);
        }
        ui.small(
            "reliable traffic is never dropped — a real reliable channel \
             retransmits, so dropping handshakes would only invent \
             failures the field can't produce.",
        );
        ui.separator();
    }
}
