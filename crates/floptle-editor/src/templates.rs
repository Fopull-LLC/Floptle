//! Starter templates: `floptle --new <dir> --template <name>`.
//!
//! A template is a **finished small game**, not a folder of stubs. Each one is
//! the end state of a tutorial in the 🎓 Learn tab, so there are two honest ways
//! into the same project: build it yourself with the steps, or open the answer
//! and take it apart. Both are legitimate ways to learn, and which one suits you
//! is not something an engine should have an opinion about.
//!
//! The Lua is **not duplicated here**. Every script is the same `&'static str`
//! the tutorial hands you in [`crate::learn_content`], so a template whose
//! behaviour had quietly drifted from the lesson teaching it is not a mistake
//! this file can make. The scenes are RON next to this module, embedded at build
//! time, because a hand-placed level is data and writing it as Rust would help
//! nobody.
//!
//! Adding one: drop its files in `templates/<name>/`, add a [`Template`] below,
//! and the tests will insist it scaffolds, loads, and that every script its
//! scenes reference actually exists.

use crate::learn_content as lua;

/// One starter project.
pub(crate) struct Template {
    /// What `--template` takes, and the id the Hub stores.
    pub(crate) name: &'static str,
    /// The project title (`project.ron`, and the window caption of a build).
    pub(crate) title: &'static str,
    /// One line, for `--help` and the Hub's picker.
    pub(crate) blurb: &'static str,
    /// The tutorial that builds this from nothing, if there is one.
    pub(crate) tutorial: Option<&'static str>,
    /// Files written into the project, relative to its root.
    pub(crate) files: &'static [(&'static str, &'static str)],
    /// Binary files (images) written the same way.
    pub(crate) art: &'static [(&'static str, &'static [u8])],
}

/// The name of the do-nothing template — a plain scaffold, and the default, so
/// `--new` on its own behaves exactly as it always has.
pub(crate) const EMPTY: &str = "empty";

pub(crate) const TEMPLATES: &[Template] = &[
    Template {
        name: "platformer2d",
        title: "Pixel Platformer",
        blurb: "2D: a pixel-art level of tiles with slopes and ledges, and a player who runs and jumps",
        tutorial: Some("platformer2d"),
        files: &[
            ("scenes/first.ron", include_str!("../templates/platformer2d/first.ron")),
            ("tilesets/ground.tileset.ron", include_str!("../templates/platformer2d/ground.tileset.ron")),
            ("scripts/pixelPlayer.lua", lua::PIXEL_PLAYER_LUA),
        ],
        art: &[
            ("textures/tiles.png", include_bytes!("../templates/platformer2d/tiles.png")),
            ("textures/player.png", include_bytes!("../templates/platformer2d/player.png")),
        ],
    },
    Template {
        name: "village2d",
        title: "Pixel Village",
        blurb: "2D: walk up to people and talk — a dialogue box, choices and a pixel font, all in one script you can change",
        tutorial: None,
        files: &[
            ("scenes/first.ron", include_str!("../templates/village2d/first.ron")),
            ("tilesets/ground.tileset.ron", include_str!("../templates/platformer2d/ground.tileset.ron")),
            ("scripts/pixelPlayer.lua", lua::PIXEL_PLAYER_LUA),
            ("scripts/talk.lua", include_str!("../templates/village2d/talk.lua")),
            ("data/Mira.json", include_str!("../templates/village2d/Mira.json")),
            ("data/Sign.json", include_str!("../templates/village2d/Sign.json")),
            ("fonts/pixel.fnt", include_str!("../templates/village2d/pixel.fnt")),
        ],
        art: &[
            ("textures/tiles.png", include_bytes!("../templates/platformer2d/tiles.png")),
            ("textures/player.png", include_bytes!("../templates/platformer2d/player.png")),
            ("textures/mira.png", include_bytes!("../templates/village2d/mira.png")),
            ("textures/sign.png", include_bytes!("../templates/village2d/sign.png")),
            ("textures/portraits.png", include_bytes!("../templates/village2d/portraits.png")),
            ("textures/dialogue_frame.png", include_bytes!("../templates/village2d/dialogue_frame.png")),
            ("fonts/pixel.png", include_bytes!("../templates/village2d/pixel.png")),
        ],
    },
    Template {
        name: "platformer",
        title: "Platformer",
        blurb: "3D: run, jump, ride a moving platform, collect coins, reach the goal",
        tutorial: Some("platformer"),
        files: &[
            ("scenes/first.ron", include_str!("../templates/platformer/first.ron")),
            ("scripts/platformerPlayer.lua", lua::PLATFORMER_PLAYER_LUA),
            ("scripts/platformerCamera.lua", lua::PLATFORMER_CAMERA_LUA),
            ("scripts/platformMover.lua", lua::PLATFORM_MOVER_LUA),
            ("scripts/platformerGame.lua", lua::PLATFORMER_GAME_LUA),
            ("scripts/coin.lua", lua::COIN_LUA),
            ("scripts/goal.lua", lua::GOAL_LUA),
        ],
        art: &[],
    },
    Template {
        name: "topdown",
        title: "Top-down RPG",
        blurb: "3D: walk a village, talk to someone, take a key, unlock a door to another scene",
        tutorial: Some("topdown"),
        files: &[
            ("scenes/first.ron", include_str!("../templates/topdown/first.ron")),
            ("scenes/cave.ron", include_str!("../templates/topdown/cave.ron")),
            ("scripts/topdownPlayer.lua", lua::TOPDOWN_PLAYER_LUA),
            ("scripts/topdownCamera.lua", lua::TOPDOWN_CAMERA_LUA),
            ("scripts/npcTalk.lua", lua::NPC_TALK_LUA),
            ("scripts/inventory.lua", lua::INVENTORY_LUA),
            ("scripts/itemPickup.lua", lua::ITEM_PICKUP_LUA),
            ("scripts/door.lua", lua::DOOR_LUA),
        ],
        art: &[],
    },
    Template {
        name: "flappy",
        title: "Flappy",
        blurb: "3D shapes on a flat stage: one button, endless obstacles, a score, a restart",
        tutorial: Some("flappy"),
        files: &[
            ("scenes/first.ron", include_str!("../templates/flappy/first.ron")),
            ("prefabs/Pipe.prefab.ron", include_str!("../templates/flappy/Pipe.prefab.ron")),
            ("scripts/flappyBird.lua", lua::FLAPPY_BIRD_LUA),
            ("scripts/flappyPipe.lua", lua::FLAPPY_PIPE_LUA),
            ("scripts/flappyGame.lua", lua::FLAPPY_GAME_LUA),
        ],
        art: &[],
    },
];

/// Look a template up by name. `None` for [`EMPTY`] (and for an unknown name —
/// callers validate first with [`known`]).
pub(crate) fn find(name: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.name == name)
}

/// Whether `name` is something `--template` accepts.
pub(crate) fn known(name: &str) -> bool {
    name == EMPTY || find(name).is_some()
}

/// Every accepted name, for error messages and `--help`.
pub(crate) fn names() -> Vec<&'static str> {
    let mut out = vec![EMPTY];
    out.extend(TEMPLATES.iter().map(|t| t.name));
    out
}

/// Write a template's files into an already-scaffolded project.
///
/// Only ever *adds*: a file that already exists is left alone. Templates run
/// after the standard seeding, and the point of that ordering is that a
/// template's `first.ron` replaces the blank starter scene while the seeded
/// default scripts, materials and input map stay exactly as they are.
pub(crate) fn apply(t: &Template, root: &std::path::Path) -> std::io::Result<()> {
    for (rel, body) in t.files {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = (*body).to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        std::fs::write(&path, text)?;
    }
    for (rel, bytes) in t.art {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if !path.exists() {
            std::fs::write(&path, bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Scaffold `name` into a temp dir the way `--new --template` does.
    ///
    /// `tag` is the calling test's own name. Tests in one binary run in
    /// Parallel threads, so a path keyed only on the template would have two of
    /// them scaffolding and deleting the same directory underneath each other —
    /// which fails as "script not found" and looks exactly like a broken
    /// template.
    fn scaffold(tag: &str, name: &str) -> PathBuf {
        scaffold_with(tag, name, true)
    }

    /// [`scaffold`], choosing whether the example scripts come along.
    fn scaffold_with(tag: &str, name: &str, examples: bool) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("floptle-tpl-{}-{tag}-{name}-{examples}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let code = crate::new_project(&dir, "0.0.0-test", name, examples);
        assert_eq!(code, 0, "--new --template {name} failed");
        dir
    }

    /// Every script kind a scene (or prefab) in `root` asks for.
    fn kinds_used(root: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(root.join("scenes")).into_iter().flatten().flatten() {
            let p = entry.path();
            let doc = floptle_scene::load(&p)
                .unwrap_or_else(|e| panic!("{} does not load: {e:?}", p.display()));
            let where_ = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            for n in &doc.nodes {
                for s in &n.scripts {
                    out.push((s.kind.clone(), format!("{where_} / {}", n.name)));
                }
            }
        }
        for entry in std::fs::read_dir(root.join("prefabs")).into_iter().flatten().flatten() {
            let p = entry.path();
            let docs = crate::prefab::load_prefab_docs(&p).expect("prefab loads");
            let where_ = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            for n in &docs {
                for s in &n.scripts {
                    out.push((s.kind.clone(), format!("{where_} / {}", n.name)));
                }
            }
        }
        out
    }

    #[test]
    fn template_names_are_distinct_and_not_the_empty_one() {
        let mut seen = std::collections::HashSet::new();
        for t in TEMPLATES {
            assert!(seen.insert(t.name), "two templates called {:?}", t.name);
            assert_ne!(t.name, EMPTY, "a template may not shadow the blank scaffold");
            assert!(!t.blurb.is_empty() && !t.title.is_empty());
            assert!(!t.files.is_empty(), "{} ships no files", t.name);
        }
        assert!(known(EMPTY) && known("flappy") && !known("nope"));
    }

    /// The blank scaffold must keep behaving exactly as it did before templates
    /// existed — `--new` with no `--template` is the overwhelmingly common case
    /// and it should not have noticed this feature at all.
    #[test]
    fn the_empty_template_is_still_a_plain_project() {
        let dir = scaffold("empty-stays-plain", EMPTY);
        assert!(dir.join("project.ron").is_file());
        assert!(dir.join("scenes/first.ron").is_file());
        assert!(!dir.join("prefabs/Pipe.prefab.ron").exists());
        let cfg = floptle_scene::load_project(&dir.join("project.ron"));
        assert_eq!(cfg.title, None, "a blank project gets no invented title");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Every template scaffolds into a project that actually loads.** Scenes
    /// parse, prefabs parse, and the project config is stamped.
    #[test]
    fn every_template_scaffolds_and_loads() {
        for t in TEMPLATES {
            let dir = scaffold("scaffolds", t.name);
            let cfg = floptle_scene::load_project(&dir.join("project.ron"));
            assert_eq!(cfg.title.as_deref(), Some(t.title), "{} title", t.name);
            assert_eq!(cfg.engine_version.as_deref(), Some("0.0.0-test"));
            for (rel, _) in t.files {
                assert!(dir.join(rel).is_file(), "{} did not write {rel}", t.name);
            }
            // Loading them is the assertion — `kinds_used` panics on a bad file.
            let used = kinds_used(&dir);
            assert!(!used.is_empty(), "{} has scenes but nothing running in them", t.name);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A scene that names a script the project doesn't have is a node that
    /// silently does nothing — the exact failure a starter project must not
    /// ship, because the reader has no way to tell it from "I broke it".
    ///
    /// With and without the example scripts: a scene that leans on one breaks
    /// the day somebody unticks the box.
    #[test]
    fn every_script_a_template_asks_for_is_there() {
        for t in TEMPLATES {
            for examples in [true, false] {
                let dir = scaffold_with("asks-for", t.name, examples);
                for (kind, whence) in kinds_used(&dir) {
                    let path = dir.join("scripts").join(format!("{kind}.lua"));
                    assert!(
                        path.is_file(),
                        "{} (examples: {examples}): {whence} runs {kind}.lua, which the project \
                         doesn't have",
                        t.name
                    );
                }
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    /// `--no-examples` leaves the example scripts out, and opening the project
    /// again does not put them back — seeding runs on every open.
    #[test]
    fn a_project_without_examples_stays_without_them() {
        let dir = scaffold_with("no-examples", EMPTY, false);
        let scripts = dir.join("scripts");
        let check = |when: &str| {
            for (name, _) in crate::lua_support::DEFAULT_SCRIPTS {
                let required = crate::lua_support::REQUIRED_SCRIPTS.contains(name);
                assert_eq!(scripts.join(name).is_file(), required, "{name} {when}");
            }
        };
        check("after scaffolding");
        assert!(!floptle_scene::load_project(&dir.join("project.ron")).example_scripts);
        let ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        ed.seed_project_dirs();
        check("after reopening");
        let _ = std::fs::remove_dir_all(&dir);

        // The default still ships them all.
        let dir = scaffold_with("no-examples", EMPTY, true);
        for (name, _) in crate::lua_support::DEFAULT_SCRIPTS {
            assert!(dir.join("scripts").join(name).is_file(), "{name} missing by default");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// …and the other direction: a template shipping a script nothing uses is
    /// either dead weight or a scene that forgot to attach it.
    #[test]
    fn every_script_a_template_ships_is_used() {
        for t in TEMPLATES {
            let dir = scaffold("ships-used", t.name);
            let used: std::collections::HashSet<String> =
                kinds_used(&dir).into_iter().map(|(k, _)| k).collect();
            for (rel, _) in t.files {
                let Some(stem) = rel.strip_prefix("scripts/").and_then(|s| s.strip_suffix(".lua"))
                else {
                    continue;
                };
                assert!(used.contains(stem), "{} ships {stem}.lua but nothing runs it", t.name);
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// **The scripts run.** Parsing is not working: a call to a name that no
    /// longer exists, or a method on a nil field, compiles perfectly and dies on
    /// the first frame. This drives each one for a few ticks against a real
    /// `ScriptHost` and insists nothing raised.
    ///
    /// Modelled on `floptle-script`'s own `shipped_controller_scripts_run…`,
    /// which exists for the same reason.
    #[test]
    fn every_template_script_runs_without_errors() {
        use floptle_core::transform::Transform;
        use floptle_core::{Scripts, World};

        for t in TEMPLATES {
            let dir = scaffold("runs", t.name);
            let scripts = dir.join("scripts");
            for (rel, _) in t.files {
                let Some(kind) = rel.strip_prefix("scripts/").and_then(|s| s.strip_suffix(".lua"))
                else {
                    continue;
                };
                // Under both vectors. A new project is `fast`, so a template
                // exercised only in `exact` (the host's default) would be tested
                // under the one mode no new project has — and `v.x = n` in a
                // template raises only in `fast`.
                for mode in [floptle_script::Vec3Mode::Exact, floptle_script::Vec3Mode::Fast] {
                let mut world = World::default();
                let e = world.spawn();
                world.insert(e, Transform::IDENTITY);
                world.insert(e, floptle_core::Name("Player".into()));
                world.insert(e, floptle_core::Matter::Empty);
                world.insert(e, floptle_core::RigidBody::default());
                world.insert(
                    e,
                    Scripts(vec![floptle_core::ScriptInst {
                        kind: kind.to_string(),
                        enabled: true,
                        params: Vec::new(),
                        refs: Vec::new(),
                        strs: Vec::new(),
                        pinned_bools: None,
                    }]),
                );

                let mut host = floptle_script::ScriptHost::new();
                host.set_vec3_mode(mode).expect("this build offers the mode");
                // What the physics step would publish: standing on flat ground,
                // moving, with a real up. Without it `node.vel` is nil and every
                // controller is testing something other than itself.
                let mut bodies = std::collections::HashMap::new();
                bodies.insert(
                    e.index(),
                    floptle_script::BodyState {
                        vel: [0.5, 0.0, -1.0],
                        up: [0.0, 1.0, 0.0],
                        grounded: true,
                        height: 2.0,
                        pos: [0.0, 0.0, 0.0],
                        ground_normal: Some([0.0, 1.0, 0.0]),
                        wall_normal: None,
                        asleep: false,
                        wake: "settling",
                        spin: 0.0,
                    },
                );
                host.set_bodies(bodies);
                for i in 0..4 {
                    host.run(&mut world, &scripts, 1.0 / 60.0, i as f32 / 60.0);
                }
                assert!(
                    host.errors().is_empty(),
                    "{} / {kind}.lua raised under {mode:?}: {:?}",
                    t.name,
                    host.errors()
                );
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The 2D starter plays: the player lands on the ground the template drew,
    /// and its script runs without an error. Scaffolding and loading say the
    /// files are there; this says the game in them works.
    #[test]
    fn the_pixel_platformer_lands_its_player() {
        let dir = scaffold("pixel-lands", "platformer2d");
        let mut ed = crate::Editor::default();
        ed.open_project(dir.clone());
        ed.open_scene_file(&dir.join("scenes/first.ron").to_string_lossy());
        let player = ed
            .world
            .query::<floptle_core::Name>()
            .find(|(_, n)| n.0 == "Player")
            .map(|(e, _)| e)
            .expect("the template has a Player");
        // A 2D scene opens in a 2D Scene view.
        assert_eq!(ed.camera.lock, floptle_render::ViewLock::Front);
        assert!(ed.camera.is_ortho());
        ed.toggle_play();
        for _ in 0..150 {
            ed.play_step(1.0 / 60.0, true);
            ed.drain_script_logs();
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let at = floptle_core::world_transform(&ed.world, player).translation;
        assert!((at.y - 3.5).abs() < 0.1, "the player stands on the grass, whose top is y = 3; got {:.3}", at.y);
        // Holding right runs right: the script is driving the body.
        ed.raw_input.keys.insert(floptle_input::Key::KeyD);
        for _ in 0..30 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let ran = floptle_core::world_transform(&ed.world, player).translation.x - at.x;
        assert!(ran > 1.5, "half a second of running right at 7 m/s should cover over 1.5 units; got {ran:.2}");
        // …and a press of Jump leaves the ground.
        ed.raw_input.keys.remove(&floptle_input::Key::KeyD);
        let ground = floptle_core::world_transform(&ed.world, player).translation.y;
        ed.raw_input.keys.insert(floptle_input::Key::Space);
        ed.raw_input.pressed.insert(floptle_input::Source::Key(floptle_input::Key::Space));
        ed.tick_input_edges.0.insert(floptle_input::Source::Key(floptle_input::Key::Space));
        let mut top = ground;
        for _ in 0..30 {
            ed.play_step(1.0 / 60.0, true);
            ed.raw_input.pressed.clear();
            top = top.max(floptle_core::world_transform(&ed.world, player).translation.y);
        }
        assert!(top - ground > 2.0, "a full jump clears two units; rose {:.2}", top - ground);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every text the Dialogue layer shows right now, joined.
    fn dialogue_text(ed: &crate::Editor) -> String {
        let mut out = String::new();
        for (_, spec) in ed.world.query::<floptle_ui::ElementSpec>() {
            if let Some(t) = &spec.text {
                out.push_str(&t.text);
                out.push('|');
            }
        }
        out
    }

    fn press_e(ed: &mut crate::Editor) {
        use floptle_input::{Key, Source};
        ed.raw_input.keys.insert(Key::KeyE);
        ed.raw_input.pressed.insert(Source::Key(Key::KeyE));
        ed.tick_input_edges.0.insert(Source::Key(Key::KeyE));
        ed.play_step(1.0 / 60.0, true);
        ed.raw_input.pressed.clear();
        ed.raw_input.keys.remove(&Key::KeyE);
        ed.play_step(1.0 / 60.0, true);
    }

    /// The village plays a conversation from its data file: E beside Mira
    /// opens it, the words type out, a choice is made, and it ends handing the
    /// player back their controls.
    #[test]
    fn the_pixel_village_holds_a_conversation() {
        let dir = scaffold("village-talk", "village2d");
        let mut ed = crate::Editor::default();
        ed.open_project(dir.clone());
        ed.open_scene_file(&dir.join("scenes/first.ron").to_string_lossy());
        let player = ed.world.query::<floptle_core::Name>().find(|(_, n)| n.0 == "Player").map(|(e, _)| e).unwrap();
        if let Some(t) = ed.world.get_mut::<floptle_core::transform::Transform>(player) {
            t.translation.x = 22.8;
        }
        ed.toggle_play();
        for _ in 0..60 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        assert!(dialogue_text(&ed).contains("talk to Mira"), "a hint near Mira: {}", dialogue_text(&ed));

        press_e(&mut ed);
        for _ in 0..90 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert!(dialogue_text(&ed).contains("You're new around here."), "the first line typed out: {}", dialogue_text(&ed));
        // Walking is swallowed while talking.
        let x = floptle_core::world_transform(&ed.world, player).translation.x;
        ed.raw_input.keys.insert(floptle_input::Key::KeyD);
        for _ in 0..20 {
            ed.play_step(1.0 / 60.0, true);
        }
        ed.raw_input.keys.remove(&floptle_input::Key::KeyD);
        let moved = floptle_core::world_transform(&ed.world, player).translation.x - x;
        assert!(moved.abs() < 0.05, "the player must stand still while talking; moved {moved:.2}");

        press_e(&mut ed);
        for _ in 0..120 {
            ed.play_step(1.0 / 60.0, true);
        }
        let shown = dialogue_text(&ed);
        assert!(shown.contains("> Not yet.") && shown.contains("  What's so special"), "the choices, the first focused: {shown}");
        // E picks the focused choice, which is the first.
        press_e(&mut ed);
        for _ in 0..120 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert!(dialogue_text(&ed).contains("Jump up on the planks"), "the first choice's answer: {}", dialogue_text(&ed));
        for _ in 0..2 {
            press_e(&mut ed);
            for _ in 0..90 {
                ed.play_step(1.0 / 60.0, true);
            }
        }
        assert!(!dialogue_text(&ed).contains("See you around"), "the last line ends it: {}", dialogue_text(&ed));
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let x = floptle_core::world_transform(&ed.world, player).translation.x;
        ed.raw_input.keys.insert(floptle_input::Key::KeyD);
        for _ in 0..20 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert!(floptle_core::world_transform(&ed.world, player).translation.x - x > 0.5, "free to walk again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// …and a 3D scene leaves the Scene view free.
    #[test]
    fn a_3d_template_opens_in_a_free_view() {
        let dir = scaffold("free-view", "platformer");
        let mut ed = crate::Editor::default();
        ed.open_project(dir.clone());
        ed.open_scene_file(&dir.join("scenes/first.ron").to_string_lossy());
        assert_eq!(ed.camera.lock, floptle_render::ViewLock::Free);
        assert!(!ed.camera.is_ortho());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
