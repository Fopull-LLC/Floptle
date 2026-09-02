//! Driving the Lua `app.*` table: what the game currently is, and what a script
//! asked to change about it (`floptle/0175`).
//!
//! ## What `app.quit()` does depends on where the game is running
//!
//! There is one honest answer per host, and they are genuinely different things:
//!
//! * **In an exported build**, the game IS the program, so quitting ends it. The
//!   save store is flushed first — somebody quitting from a settings menu
//!   expects the setting they just changed to have been kept, and the ordinary
//!   flush happens on Stop, which a build never reaches.
//! * **In the editor**, Play is the game and the editor is not. Stopping Play is
//!   the equivalent; closing the editor because a game under test called `quit`
//!   would lose an afternoon's unsaved work to one line of Lua.
//! * **Headless (`floptle run`)**, the run ends where it stands, and the verb
//!   reports the frame it stopped on rather than claiming it ran the whole span.
//!
//! ## A video setting a script changes is for this session only
//!
//! Vsync and the retro presentation live in `project.ron`, which is the file
//! that ships to everybody who plays the game. A player turning vsync off in an
//! options menu must not edit the game, so the project doc is **snapshotted at
//! Play and restored at Stop** — the rule `audio.track(…):setVolume` already
//! follows, and the reason `access.*` leaves persistence to `save.*`.
//!
//! That restore is not decoration. `save_project` writes the whole live
//! `ProjectConfigDoc`, so without it a script that set vsync during a playtest
//! would have that value written into `project.ron` the next time anybody
//! touched Project Settings — and shipped.

use floptle_script::app_api::{AppInfo, AppRequests, Vsync};

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
            fullscreen: self.window.as_ref().is_some_and(|w| w.fullscreen().is_some()),
        };
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
        if let Some(on) = req.fullscreen {
            self.app_set_fullscreen(on);
        }
        if req.quit {
            self.app_quit();
        }
    }

    /// Cover the screen, or stop — `app.setFullscreen`, and the F11 /
    /// Alt+Enter a build answers on its own.
    ///
    /// Only the game's own window does this. In the editor the window is the
    /// EDITOR's, and a game under test taking it over would be the same
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
        let Some(window) = self.window.as_ref() else { return };
        // Borderless on the current monitor: no mode switch, no resolution
        // change, the thing every game means by the word. `None` picks the
        // monitor the window is on.
        window.set_fullscreen(on.then_some(winit::window::Fullscreen::Borderless(None)));
    }

    /// End the game — see the module docs for why this is three different things.
    fn app_quit(&mut self) {
        // The save store, first and in every host. A player quitting from a
        // settings menu expects the setting they just changed to have been
        // kept, and the ordinary flush is on Stop, which a build never reaches.
        self.script_host.flush_save();
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
        std::fs::create_dir_all(dir.join("scenes")).unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(
            dir.join("project.ron"),
            "(title: Some(\"Test Game\"), vsync: Adaptive, retro_height: 240)",
        )
        .unwrap();
        std::fs::write(dir.join("scripts/menu.lua"), body).unwrap();
        std::fs::write(
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
            "function update(node, dt)\n  app.setVsync('Off')\n  app.setRetroHeight(360)\nend\n",
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

        ed.toggle_play();
        assert!(!ed.playing);
        assert_eq!(
            ed.project.vsync,
            floptle_scene::VsyncDoc::Adaptive,
            "Stop left a player's vsync setting in the project — the next Project Settings \
             save would write it into project.ron and ship it"
        );
        assert_eq!(ed.project.retro_height, 240, "…and the same for the retro height");
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
        // Play is NOT stopped in a build: the process is leaving, and stopping
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
        std::fs::create_dir_all(dir.join("scenes")).unwrap();
        // `entry_scene` is an Option; a bare string is the typo somebody makes.
        std::fs::write(dir.join("project.ron"), "(entry_scene: \"scenes/first.ron\")").unwrap();

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

        // A project with NO project.ron is not a fault — that is a project with
        // default settings, and warning about it would cry wolf on every new one.
        std::fs::remove_file(dir.join("project.ron")).unwrap();
        let mut fresh = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let _ = fresh.read_project_config();
        assert!(fresh.console.entries.is_empty(), "a missing project.ron is not an error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
