//! Driving the Lua `app.*` table: what the game currently is, and what a
//! script asked to change about it.
//!
//! What `app.quit()` does depends on where the game is running:
//!
//! * In an exported build the game is the program, so quitting ends it. The
//!   save store is flushed first, since the ordinary flush happens on Stop,
//!   which a build never reaches, and somebody quitting from a settings menu
//!   expects the setting they just changed to have been kept.
//! * In the editor, Play is the game and the editor is not, so quitting stops
//!   Play. Closing the editor because a game under test called `quit` would
//!   lose an afternoon's unsaved work to one line of Lua.
//! * Headless (`floptle run`), the run ends where it stands, and the verb
//!   reports the frame it stopped on rather than claiming it ran the whole
//!   span.
//!
//! A video setting a script changes is for this session only. Vsync and the
//! retro presentation live in `project.ron`, the file that ships to everybody
//! who plays the game, and a player turning vsync off in an options menu must
//! not edit the game. So the project doc is snapshotted at Play and restored
//! at Stop, the rule `audio.track(…):setVolume` follows, and the reason
//! `access.*` leaves persistence to `save.*`. `save_project` writes the whole
//! live `ProjectConfigDoc`, so without the restore a vsync set during a
//! playtest would be written into `project.ron` the next time anybody touched
//! Project Settings, and shipped.

use std::rc::Rc;

use floptle_script::app_api::{AppInfo, AppRequests, DisplayMode, DisplayModeRequest, MonitorInfo, Vsync};

/// What the displays were when last asked, and for how many frames more that
/// answer stands. Two seconds: a monitor plugged in shows up in a settings
/// page that is open, and the frames between pay nothing.
pub(crate) struct DisplayCache {
    frames_left: u32,
    monitors: Rc<Vec<MonitorInfo>>,
    modes: Rc<Vec<DisplayMode>>,
}

/// The window's monitors as `app.monitors()` lists them, and every mode of
/// each. A mode a monitor lists once per colour depth is listed once.
fn gather_displays(window: &winit::window::Window) -> (Vec<MonitorInfo>, Vec<DisplayMode>) {
    let current = window.current_monitor();
    // The same screen by what it is as well as by handle: a platform that
    // hands back a fresh handle for the same output would otherwise mark no
    // monitor current, which is what a game saw under Wayland.
    let key = |m: &winit::monitor::MonitorHandle| (m.name(), m.position(), m.size());
    let current_key = current.as_ref().map(key);
    let raw = window.available_monitors().map(|m| {
        let (size, pos) = (m.size(), m.position());
        let info = MonitorInfo {
            name: m.name().unwrap_or_default(),
            width: size.width,
            height: size.height,
            refresh: m.refresh_rate_millihertz().map(|r| r as f32 / 1000.0),
            x: pos.x,
            y: pos.y,
            current: current.as_ref() == Some(&m) || current_key.as_ref().is_some_and(|k| *k == key(&m)),
        };
        let modes = m
            .video_modes()
            .map(|v| (v.size().width, v.size().height, v.refresh_rate_millihertz() as f32 / 1000.0))
            .collect();
        (info, modes)
    });
    merge_displays(raw)
}

/// Monitor `n` (1-based) as `app.monitors()` numbers them: copies of one
/// screen count once, the way [`merge_displays`] lists them.
fn nth_monitor(window: &winit::window::Window, n: usize) -> Option<winit::monitor::MonitorHandle> {
    let mut seen: Vec<(Option<String>, winit::dpi::PhysicalPosition<i32>, winit::dpi::PhysicalSize<u32>)> = Vec::new();
    window
        .available_monitors()
        .filter(|m| {
            let key = (m.name(), m.position(), m.size());
            let fresh = !seen.contains(&key);
            if fresh {
                seen.push(key);
            }
            fresh
        })
        .nth(n.checked_sub(1)?)
}

/// One entry per physical monitor, numbered from 1, with its modes largest
/// first.
///
/// Some platforms hand the same monitor over more than once (X11 and some
/// Wayland compositors list one per output and again per CRTC), so a settings
/// page offered each screen twice, with two marked current. The same name at
/// the same place and size is the same screen; its copies add nothing but
/// their `current` flag.
fn merge_displays(
    raw: impl IntoIterator<Item = (MonitorInfo, Vec<(u32, u32, f32)>)>,
) -> (Vec<MonitorInfo>, Vec<DisplayMode>) {
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    let mut modes = Vec::new();
    for (info, own) in raw {
        let same = |m: &MonitorInfo| {
            (&m.name, m.x, m.y, m.width, m.height) == (&info.name, info.x, info.y, info.width, info.height)
        };
        if let Some(seen) = monitors.iter_mut().find(|m| same(m)) {
            seen.current |= info.current;
            continue;
        }
        monitors.push(info);
        let mut own: Vec<DisplayMode> = own
            .into_iter()
            .map(|(width, height, refresh)| DisplayMode { width, height, refresh, monitor: monitors.len() })
            .collect();
        own.sort_by(|a, b| (b.width, b.height).cmp(&(a.width, a.height)).then(b.refresh.total_cmp(&a.refresh)));
        own.dedup();
        modes.extend(own);
    }
    (monitors, modes)
}

/// The API's vsync mode as the file format's.
pub(crate) fn to_doc(v: Vsync) -> floptle_scene::VsyncDoc {
    match v {
        Vsync::On => floptle_scene::VsyncDoc::On,
        Vsync::Adaptive => floptle_scene::VsyncDoc::Adaptive,
        Vsync::Off => floptle_scene::VsyncDoc::Off,
    }
}

/// The file format's vsync mode as the API's.
pub(crate) fn from_doc(v: floptle_scene::VsyncDoc) -> Vsync {
    match v {
        floptle_scene::VsyncDoc::On => Vsync::On,
        floptle_scene::VsyncDoc::Adaptive => Vsync::Adaptive,
        floptle_scene::VsyncDoc::Off => Vsync::Off,
    }
}

impl crate::Editor {
    /// The game's title, as a menu would put it at the top of itself.
    ///
    /// The export manifest's title in a build, the project's otherwise, and the
    /// project folder's name when neither is set — the same fallback chain the
    /// window title uses, so a menu and the title bar never disagree.
    fn app_title(&self) -> String {
        if !self.game_title.is_empty() {
            return self.game_title.clone();
        }
        if let Some(t) = self.project.title.as_deref().filter(|t| !t.trim().is_empty()) {
            return t.to_string();
        }
        self.project_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Tell `app.*` what the game currently is. Called once a frame.
    pub(crate) fn push_app_info(&mut self) {
        let info = AppInfo {
            title: self.app_title(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            vsync: from_doc(self.project.vsync),
            retro: self.project.retro,
            retro_height: self.project.retro_height,
            retro_integer_scale: self.project.retro_integer_scale,
            render_scale: self.project.render_scale,
            render_sharpness: self.render_sharpness(),
            fullscreen: self.window.as_ref().is_some_and(|w| w.fullscreen().is_some()),
            frame_cap: self.frame_cap,
            refresh_hz: (self.refresh_period > 0.0).then(|| 1.0 / self.refresh_period),
            dynamic_resolution: self.dyn_res.is_some(),
            touch: self.touch_device,
            monitors: Rc::default(),
            display_modes: Rc::default(),
        };
        let (monitors, display_modes) = self.displays();
        let info = AppInfo { monitors, display_modes, ..info };
        self.script_host.set_app_info(info);
    }

    /// Apply whatever `app.*` asked for this frame.
    ///
    /// Runs right after the scripts, so a setting changed in an `update` takes
    /// effect on the frame it was changed on rather than the next one — a
    /// settings menu whose control lags a frame behind the click reads as a
    /// control that did not work.
    pub(crate) fn apply_app_requests(&mut self) {
        let req: AppRequests = self.script_host.take_app_requests();
        if req.is_empty() {
            return;
        }
        if let Some(v) = req.vsync {
            self.project.vsync = to_doc(v);
        }
        if let Some(on) = req.retro {
            self.project.retro = on;
        }
        if let Some(px) = req.retro_height {
            self.project.retro_height = px;
        }
        if let Some(on) = req.retro_integer_scale {
            self.project.retro_integer_scale = on;
        }
        if let Some(s) = req.render_scale {
            self.project.render_scale = s;
        }
        if let Some(s) = req.render_sharpness {
            self.render_sharpness = Some(s);
        }
        if let Some(on) = req.fullscreen {
            self.app_set_fullscreen(on);
        }
        if let Some(d) = req.display_mode {
            self.app_set_display_mode(d);
        }
        if let Some(fps) = req.frame_cap {
            self.frame_cap = fps.max(0.0);
        }
        if let Some(spec) = req.dynamic_resolution {
            self.dyn_res = spec.map(|s| {
                let d = floptle_script::app_api::DynamicResolution::new(s, self.project.render_scale);
                self.project.render_scale = d.scale();
                d
            });
        }
        if req.quit {
            self.app_quit();
        }
    }

    /// Cover the screen, or stop — `app.setFullscreen`, and the F11 /
    /// Alt+Enter a build answers on its own.
    ///
    /// Only the game's own window does this. In the editor the window is the
    /// editor's, and a game under test taking it over would be the same
    /// surprise as `app.quit()` closing it — so, like `quit`, it does the
    /// honest thing for the host it is in and says so in the Console once.
    pub(crate) fn app_set_fullscreen(&mut self, on: bool) {
        if !self.player_mode {
            if !self.fullscreen_explained {
                self.fullscreen_explained = true;
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!(
                        "app.setFullscreen({on}) — in an exported build this {} the game's \
                         window; in the editor the window is the editor's, so it is left \
                         alone. app.fullscreen() answers the real state either way.",
                        if on { "fills the screen with" } else { "windows" }
                    ),
                    None,
                );
            }
            return;
        }
        if let Some(window) = self.window.as_ref() {
            crate::set_fullscreen(window, on);
        }
    }

    /// The monitors and their modes, from the cache while it stands. None
    /// without a window.
    fn displays(&mut self) -> (Rc<Vec<MonitorInfo>>, Rc<Vec<DisplayMode>>) {
        let Some(window) = self.window.as_ref() else { return (Rc::default(), Rc::default()) };
        match &mut self.display_cache {
            Some(c) if c.frames_left > 0 => {
                c.frames_left -= 1;
                (c.monitors.clone(), c.modes.clone())
            }
            _ => {
                let (m, d) = gather_displays(window);
                let c = DisplayCache { frames_left: 120, monitors: Rc::new(m), modes: Rc::new(d) };
                let out = (c.monitors.clone(), c.modes.clone());
                self.display_cache = Some(c);
                out
            }
        }
    }

    /// `app.setDisplayMode{...}`: a resolution, refresh rate and monitor.
    ///
    /// Full screen switches the monitor to that mode (exclusive), which is
    /// what a resolution setting means there; a browser has no such thing and
    /// covers the screen instead. A window is resized to that many pixels and
    /// centred on the monitor. Like `app.setFullscreen`, only a game's own
    /// window does this; under the editor it says so once and leaves the
    /// editor's window alone.
    pub(crate) fn app_set_display_mode(&mut self, d: DisplayModeRequest) {
        if !self.player_mode {
            if !self.fullscreen_explained {
                self.fullscreen_explained = true;
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!(
                        "app.setDisplayMode{{ width = {}, height = {} }} — in an exported build this \
                         sets the game's window or screen mode; in the editor the window is the \
                         editor's, so it is left alone.",
                        d.width, d.height
                    ),
                    None,
                );
            }
            return;
        }
        let Some(window) = self.window.as_ref() else { return };
        let target = d
            .monitor
            .and_then(|i| nth_monitor(window, i))
            .or_else(|| window.current_monitor());
        let full = d.fullscreen.unwrap_or(window.fullscreen().is_some());
        if full {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(m) = &target {
                let handles: Vec<winit::monitor::VideoModeHandle> = m.video_modes().collect();
                let listed: Vec<DisplayMode> = handles
                    .iter()
                    .map(|v| DisplayMode {
                        width: v.size().width,
                        height: v.size().height,
                        refresh: v.refresh_rate_millihertz() as f32 / 1000.0,
                        monitor: 1,
                    })
                    .collect();
                if let Some(want) = floptle_script::app_api::pick_mode(&listed, 1, d.width, d.height, d.refresh)
                    && let Some(i) = listed.iter().position(|m| *m == want)
                {
                    window.set_fullscreen(Some(winit::window::Fullscreen::Exclusive(handles[i].clone())));
                    self.display_cache = None;
                    return;
                }
            }
            // No such mode (or a browser): cover the screen at its own size.
            #[cfg(target_arch = "wasm32")]
            crate::set_fullscreen(window, true);
            #[cfg(not(target_arch = "wasm32"))]
            window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(target)));
        } else {
            window.set_fullscreen(None);
            let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(d.width, d.height));
            if let Some(m) = target {
                let (pos, size) = (m.position(), m.size());
                let x = pos.x + (size.width as i32 - d.width as i32).max(0) / 2;
                let y = pos.y + (size.height as i32 - d.height as i32).max(0) / 2;
                window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
        }
        self.display_cache = None;
    }

    /// End the game — see the module docs for why this is three different things.
    fn app_quit(&mut self) {
        // The save store, first and in every host. A player quitting from a
        // settings menu expects the setting they just changed to have been
        // kept, and the ordinary flush is on Stop, which a build never reaches.
        self.script_host.flush_save_and_wait();
        if self.player_mode {
            // `about_to_wait` is the one place every exit path passes through:
            // it writes package prefs and then leaves. Setting the flag rather
            // than exiting here is what makes the shutdown orderly — there is no
            // event loop to reach from inside a frame.
            self.pending_exit = true;
            return;
        }
        if self.playing {
            self.toggle_play();
        }
        self.console.push(
            floptle_script::LogLevel::Debug,
            "app.quit() — Play stopped. In an exported build this closes the game; the \
             editor stops instead, so a game under test cannot take your session with it."
                .into(),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project on disk with a script that changes a video setting and quits.
    fn settings_project(dir: &std::path::Path, body: &str) {
        let _ = std::fs::remove_dir_all(dir);
        floptle_vfs::create_dir_all(dir.join("scenes")).unwrap();
        floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
        floptle_vfs::write(
            dir.join("project.ron"),
            "(title: Some(\"Test Game\"), vsync: Adaptive, retro_height: 240)",
        )
        .unwrap();
        floptle_vfs::write(dir.join("scripts/menu.lua"), body).unwrap();
        floptle_vfs::write(
            dir.join("scenes/first.ron"),
            "(name: \"first\", lighting: (), nodes: [(name: \"Menu\", transform: (translation: \
             (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: \
             Empty, scripts: [(kind: \"menu\", enabled: true, params: [], refs: [], strs: [])], \
             id: Some(1))])",
        )
        .unwrap();
    }

    /// The three modes survive the trip out to Lua and back.
    ///
    /// A settings menu shows the name to a player, saves it, and sets it again
    /// next launch, so a mapping that lost a mode would be a setting that
    /// silently reverted every time.
    #[test]
    fn every_vsync_mode_round_trips_through_the_api() {
        for doc in [
            floptle_scene::VsyncDoc::On,
            floptle_scene::VsyncDoc::Adaptive,
            floptle_scene::VsyncDoc::Off,
        ] {
            assert_eq!(to_doc(from_doc(doc)), doc);
        }
        for v in [Vsync::On, Vsync::Adaptive, Vsync::Off] {
            assert_eq!(from_doc(to_doc(v)), v);
            // …and the name a script says is the name the file uses.
            assert_eq!(format!("{:?}", to_doc(v)), v.name());
        }
    }

    /// **A player's setting must never be written into the game.**
    ///
    /// `app.setVsync` writes the live `ProjectConfigDoc`, and that same doc is
    /// what `save_project` writes to `project.ron` — the file that ships to
    /// everybody who plays. So a script that turned vsync off during a playtest
    /// would have that value written into the project the next time anybody
    /// touched Project Settings, and shipped it. Stop puts the project back.
    #[test]
    fn a_video_setting_a_script_changed_does_not_survive_stop() {
        let dir = std::env::temp_dir().join(format!("floptle-app-revert-{}", std::process::id()));
        settings_project(
            &dir,
            "function update(node, dt)\n  app.setVsync('Off')\n  app.setRetroHeight(360)\n  app.setRenderScale(0.5)\nend\n",
        );
        let mut ed = crate::Editor::default();
        ed.open_project(dir.clone());
        assert_eq!(ed.project.vsync, floptle_scene::VsyncDoc::Adaptive, "the project's own value");
        assert_eq!(ed.project.retro_height, 240);

        ed.toggle_play();
        assert!(ed.playing, "the project did not enter play");
        ed.play_step(1.0 / 60.0, true);
        assert_eq!(
            ed.project.vsync,
            floptle_scene::VsyncDoc::Off,
            "the change has to take effect for the run, or the setting does nothing"
        );
        assert_eq!(ed.project.retro_height, 360);
        assert_eq!(ed.project.render_scale, 0.5, "app.setRenderScale did not reach the project");

        ed.toggle_play();
        assert!(!ed.playing);
        assert_eq!(
            ed.project.vsync,
            floptle_scene::VsyncDoc::Adaptive,
            "Stop left a player's vsync setting in the project — the next Project Settings \
             save would write it into project.ron and ship it"
        );
        assert_eq!(ed.project.retro_height, 240, "…and the same for the retro height");
        assert_eq!(ed.project.render_scale, 1.0, "…and the render scale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A frame cap and dynamic resolution reach the driver, act, and end
    /// with the run.** The governor's choice lands on the render scale the
    /// renderer reads, a slow GPU frame lowers it, and Stop puts back the
    /// project's scale and switches both off.
    #[test]
    fn frame_cap_and_dynamic_resolution_act_for_the_run_and_end_at_stop() {
        let dir = std::env::temp_dir().join(format!("floptle-app-dynres-{}", std::process::id()));
        settings_project(
            &dir,
            "function start(node)\n  app.setFrameCap(72)\n  app.setDynamicResolution{ min = 0.5 }\nend\n",
        );
        let mut ed = crate::Editor::default();
        ed.open_project(dir.clone());
        ed.toggle_play();
        ed.play_step(1.0 / 60.0, true);
        assert_eq!(ed.frame_cap, 72.0, "app.setFrameCap did not reach the driver");
        assert!(ed.dyn_res.is_some(), "app.setDynamicResolution did not reach the driver");
        assert!((ed.frame_period_ms() - 1000.0 / 72.0).abs() < 1e-3, "the cap is the frame's period");
        ed.frame_ms = 1000.0 / 72.0;
        ed.feed_dynamic_resolution(40.0);
        assert!(ed.project.render_scale < 0.95, "a slow GPU frame left the scale at {}", ed.project.render_scale);
        assert!(ed.project.render_scale >= 0.5);
        ed.toggle_play();
        assert_eq!(ed.frame_cap, 0.0, "the cap outlived the run");
        assert!(ed.dyn_res.is_none(), "dynamic resolution outlived the run");
        assert_eq!(ed.project.render_scale, 1.0, "the project's own scale came back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In the editor, `app.quit()` stops Play. It must not be a process exit:
    /// closing the editor because a game under test called quit would take an
    /// afternoon's unsaved work with it.
    #[test]
    fn quit_stops_play_in_the_editor_rather_than_leaving() {
        let dir = std::env::temp_dir().join(format!("floptle-app-quit-{}", std::process::id()));
        settings_project(&dir, "function update(node, dt)\n  app.quit()\nend\n");
        let mut ed = crate::Editor::default();
        ed.open_project(dir.clone());
        ed.toggle_play();
        assert!(ed.playing);
        ed.play_step(1.0 / 60.0, true);
        assert!(!ed.playing, "app.quit() did not stop Play");
        assert!(
            !ed.pending_exit,
            "the EDITOR was asked to exit — a game under test must not be able to close it"
        );
        // …and it says which of the two things it did, since they are different.
        assert!(
            ed.console.entries.iter().any(|e| e.msg.contains("app.quit()")),
            "nothing explained why Play stopped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **In a build, quit ends the process** — through the one flag every exit
    /// path already passes through, so package prefs are written and the
    /// shutdown is orderly. Not a bare `exit()` from inside a frame.
    #[test]
    fn quit_ends_the_process_in_a_build() {
        let dir = std::env::temp_dir().join(format!("floptle-app-build-{}", std::process::id()));
        settings_project(&dir, "function update(node, dt)\n  app.quit()\nend\n");
        let mut ed = crate::Editor { player_mode: true, ..Default::default() };
        ed.open_project(dir.clone());
        ed.toggle_play();
        assert!(ed.playing);
        ed.play_step(1.0 / 60.0, true);
        assert!(ed.pending_exit, "a build was asked to quit and did not");
        // Play is not stopped in a build: the process is leaving, and stopping
        // would run the editor's whole restore path on the way out.
        assert!(ed.playing, "a build stopped Play instead of quitting");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A `project.ron` that will not parse is said out loud.**
    ///
    /// It answers a default config, which is right — a project with a broken
    /// settings file should still open — and it used to do it in silence. One
    /// misplaced bracket and the project comes up with no title, the wrong frame
    /// pacing and none of its layers, looking exactly like a project nobody had
    /// configured. This cost two debugging cycles inside the session that added
    /// it.
    #[test]
    fn a_project_file_that_will_not_parse_says_so() {
        let dir = std::env::temp_dir().join(format!("floptle-app-badcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("scenes")).unwrap();
        // `entry_scene` is an Option; a bare string is the typo somebody makes.
        floptle_vfs::write(dir.join("project.ron"), "(entry_scene: \"scenes/first.ron\")").unwrap();

        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let cfg = ed.read_project_config();
        assert_eq!(cfg.title, None, "it still opens, with defaults");
        let said: String =
            ed.console.entries.iter().map(|e| e.msg.clone()).collect::<Vec<_>>().join("\n");
        assert!(said.contains("project.ron"), "the file is not named: {said}");
        assert!(said.contains("DEFAULT settings"), "it does not say what happened: {said}");
        assert!(
            said.contains("1:"),
            "the parse error's position is what makes this actionable: {said}"
        );

        // A project with no project.ron is not a fault — that is a project with
        // default settings, and warning about it would cry wolf on every new one.
        floptle_vfs::remove_file(dir.join("project.ron")).unwrap();
        let mut fresh = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let _ = fresh.read_project_config();
        assert!(fresh.console.entries.is_empty(), "a missing project.ron is not an error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    fn monitor(name: &str, x: i32, current: bool) -> MonitorInfo {
        MonitorInfo { name: name.into(), width: 2560, height: 1440, refresh: Some(144.0), x, y: 0, current }
    }

    /// **Each screen is listed once.** A platform that reports two monitors
    /// twice each gave `app.monitors()` four entries, two of them current;
    /// the copies fold into the first, keep its number, and add no modes.
    #[test]
    fn a_monitor_reported_twice_is_listed_once() {
        let modes = || vec![(2560, 1440, 144.0), (1920, 1080, 60.0), (2560, 1440, 144.0)];
        let (monitors, listed) = merge_displays([
            (monitor("DP-2", 0, false), modes()),
            (monitor("HDMI-A-1", 2560, false), modes()),
            (monitor("DP-2", 0, true), modes()),
            (monitor("HDMI-A-1", 2560, false), modes()),
        ]);
        let names: Vec<&str> = monitors.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["DP-2", "HDMI-A-1"]);
        assert_eq!(monitors.iter().filter(|m| m.current).count(), 1, "one current screen");
        assert!(monitors[0].current, "the copy's current flag reaches the screen it copies");
        assert_eq!(listed.len(), 4, "two modes each: {listed:?}");
        assert_eq!(listed.iter().filter(|d| d.monitor == 2).count(), 2);
        assert!(listed.iter().all(|d| d.monitor <= 2));
    }
}
