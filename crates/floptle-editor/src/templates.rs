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
}

/// The name of the do-nothing template — a plain scaffold, and the default, so
/// `--new` on its own behaves exactly as it always has.
pub(crate) const EMPTY: &str = "empty";

pub(crate) const TEMPLATES: &[Template] = &[
    Template {
        name: "platformer",
        title: "Platformer",
        blurb: "run, jump, ride a moving platform, collect coins, reach the goal",
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
    },
    Template {
        name: "topdown",
        title: "Top-down RPG",
        blurb: "walk a village, talk to someone, take a key, unlock a door to another scene",
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
    },
    Template {
        name: "flappy",
        title: "Flappy",
        blurb: "one button, endless obstacles, a score, and a game over you can restart",
        tutorial: Some("flappy"),
        files: &[
            ("scenes/first.ron", include_str!("../templates/flappy/first.ron")),
            ("prefabs/Pipe.prefab.ron", include_str!("../templates/flappy/Pipe.prefab.ron")),
            ("scripts/flappyBird.lua", lua::FLAPPY_BIRD_LUA),
            ("scripts/flappyPipe.lua", lua::FLAPPY_PIPE_LUA),
            ("scripts/flappyGame.lua", lua::FLAPPY_GAME_LUA),
        ],
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Scaffold `name` into a temp dir the way `--new --template` does.
    ///
    /// `tag` is the calling test's own name. Tests in one binary run in
    /// PARALLEL threads, so a path keyed only on the template would have two of
    /// them scaffolding and deleting the same directory underneath each other —
    /// which fails as "script not found" and looks exactly like a broken
    /// template.
    fn scaffold(tag: &str, name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("floptle-tpl-{}-{tag}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let code = crate::new_project(&dir, "0.0.0-test", name);
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
    #[test]
    fn every_script_a_template_asks_for_is_there() {
        for t in TEMPLATES {
            let dir = scaffold("asks-for", t.name);
            for (kind, whence) in kinds_used(&dir) {
                let path = dir.join("scripts").join(format!("{kind}.lua"));
                assert!(
                    path.is_file(),
                    "{}: {whence} runs {kind}.lua, which the project doesn't have",
                    t.name
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
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
                // Under BOTH vectors. A new project is `fast`, so a template
                // exercised only in `exact` (the host's default) would be tested
                // under the one mode no new project has — and `v.x = n` in a
                // template raises only in `fast`.
                let modes: &[floptle_script::Vec3Mode] = if cfg!(feature = "vm-luau") {
                    &[floptle_script::Vec3Mode::Exact, floptle_script::Vec3Mode::Fast]
                } else {
                    &[floptle_script::Vec3Mode::Exact]
                };
                for &mode in modes {
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
}
