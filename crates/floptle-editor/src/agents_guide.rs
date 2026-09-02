//! The `AGENTS.md` a new project is scaffolded with.
//!
//! A command line nobody knows about is a command line nobody uses. An
//! assistant opening a project folder has the files and a terminal, and no
//! reason to suspect that `floptle check` exists — so it does what anything
//! does without a tool: reads `.ron` by eye, edits it, and finds out whether
//! that worked when a person opens the editor.
//!
//! This page is the pointer that closes that gap. It is deliberately short and
//! almost entirely references: the authority is `floptle help --json`, which
//! cannot go stale, and a long guide here would be a second description of the
//! CLI to keep in step with the first.
//!
//! Written once, by `floptle new`. It is never re-written, topped up or
//! reconciled: it belongs to the project from the moment it is created, and
//! somebody who edits it or deletes it has said what they want.

use std::path::Path;

/// The file's name at the project root.
pub(crate) const FILE: &str = "AGENTS.md";

/// What gets written. `{title}` is the only substitution.
const GUIDE: &str = r#"# Working in this project

This is a [Floptle](https://fopull.com) project. The `floptle` command drives the
engine from a terminal — you do not need to open the editor to find out whether
something works.

## Before you change anything

```sh
floptle help --json        # every command, its arguments, its exit codes, what it returns
floptle inspect            # what this project is: scenes, node counts, engine version
```

`help --json` is the authority on the command line, and it is generated from the
same table the parser is, so it cannot describe a command that does not exist.
Read it once instead of guessing at flags.

## While you are changing things

```sh
floptle check              # does it still load? run this after every edit
floptle inspect --scene first
floptle inspect --select Player --json     # the whole node document, ready to patch
floptle api node:setSprite                 # what a call does, before you write it
```

## Seeing whether it actually works

```sh
floptle run --frames 120           # play it headlessly; reports what raised, and where
floptle run --frames 600 --timing   # …and what the steps cost: p50/p95/p99, in real ms
floptle shot --out look.png        # one frame through the active camera, as a PNG
floptle vfx --effect Sparks        # a particle effect, across its own timeline, as PNGs
```

If a render fails, ask the machine before you conclude the project is wrong:

```sh
floptle doctor              # can this machine render at all? exits non-zero if not
```

Neither `run` nor `check` needs a graphics adapter. `shot` does, and on a
machine without one it says so and stops rather than looking like a crash.

`run` executes the real scripts and
physics for a fixed number of steps and reports every warning, error and
`print` with its file and line — a `.ron` that loads is not a game that runs.
`shot` renders through the same path the editor's Game view uses, so the picture
is what the editor would show. **Look at it.** A render that came out wrong is
something no assertion will tell you.

`vfx` is the same idea for a particle effect, which a single frame cannot show:
it renders several moments across the effect's own timeline — through one fixed
camera, so they can be compared — and tiles them into a contact sheet. Look at
the sheet first, then `--at <seconds>` for a close look at the moment that turns
out to be the interesting one. Editing a `.vfx.ron` without doing this is
guessing.

## Changing it from a script

```sh
floptle exec fix.lua               # the editor's own API, headless
```

`scene.find`, `scene.setPos`, `scene.add`, `scene.destroy`, `ed.saveScene()` and
the rest of `docs/editor-scripting.md`. Use this instead of hand-editing `.ron`
when the change is structural — it goes through the same code the editor's own
panels do, so node ids, parent links and defaults come out right.

**It does not save unless you call `ed.saveScene()`.** If you change something
and forget, the run says so rather than losing it quietly.

**`floptle check` is the one to build a habit around.** A `.ron` file that parses
is not a scene that works: a parent link can point at the wrong node, a material
can name a texture that is not there, a node can carry a script with no file
behind it. Parsing is the only part of that a text editor can tell you about.

**`floptle api` before you call anything.** Every name a script can reach is in
there with its description, searchable by part of a name or by a word from what
it does. It exits 1 when nothing matches, so `floptle api node:doesNotExist`
answers a yes/no question.

## What lives where

| | |
| --- | --- |
| `project.ron` | the project: title, entry scene, layers, the engine version it was stamped with |
| `scenes/` | scenes — the node graph, as text |
| `scripts/` | Lua. A node names a script by its path here, without the `.lua` |
| `materials/` | materials shared between nodes |
| `textures/`, `models/`, `audio/` | assets, referenced project-relative |

Hand-editing any of these is fine and expected. `floptle check` is how you know
it worked.

## Two things that will otherwise cost you an afternoon

**A scene's `params:` list silently overrides a script's own defaults.** If a
value you changed in the `.lua` has no effect, look for it pinned in the scene
file — the authored value wins, and nothing says so.

**A node's `parent_id` beats its positional `parent`.** When you are reading a
scene by hand, follow the id. The index is a fallback that a later insertion can
quietly re-point at a different node.

## Running it

`floptle play` runs the project as a game, and `floptle open` opens the editor.
`floptle serve` runs it as a dedicated server for a networked scene.
Both need a display — check `needsGpu` in `floptle help --json` before reaching
for a command in an environment that has none.
"#;

/// Write `AGENTS.md` into a freshly scaffolded project.
///
/// Never overwrites: `floptle new` refuses to scaffold over an existing project
/// anyway, and if this file is somehow already here it is somebody's own.
pub(crate) fn write(project_root: &Path) {
    let path = project_root.join(FILE);
    if path.exists() {
        return;
    }
    // A project that scaffolded fine except for this is still a project. The
    // guide is a courtesy, not a component, so a failure to write it is a line
    // on stderr rather than a failed `new`.
    if let Err(e) = std::fs::write(&path, GUIDE) {
        eprintln!("could not write {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every command the guide teaches has to be a command that exists.**
    ///
    /// This page is the first thing an assistant reads and it will be believed
    /// literally — a command named here and absent from the binary sends
    /// somebody to debug their installation. Held against the verb table rather
    /// than proofread, because the table is what the parser is built from.
    #[test]
    fn the_guide_only_teaches_commands_that_exist() {
        let mut taught: Vec<String> = Vec::new();
        for line in GUIDE.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("floptle ") else { continue };
            // The verb, and a second word when the verb is a nested one.
            let mut words = rest.split_whitespace().filter(|w| !w.starts_with('-'));
            let Some(head) = words.next() else { continue };
            let full = match words.next() {
                Some(second) if crate::cli::VERBS.iter().any(|v| v.name == format!("{head} {second}")) => {
                    format!("{head} {second}")
                }
                _ => head.to_string(),
            };
            taught.push(full);
        }
        assert!(!taught.is_empty(), "the guide stopped naming any commands at all");
        for name in &taught {
            assert!(
                crate::cli::VERBS.iter().any(|v| v.name == *name),
                "AGENTS.md teaches `floptle {name}`, which is not a verb"
            );
        }
        // …and the two it exists to establish are actually in it.
        for must in ["check", "api"] {
            assert!(
                taught.iter().any(|t| t == must),
                "the guide no longer teaches `floptle {must}`, which is most of its point"
            );
        }
    }

    /// A scaffold writes it, and a second call leaves whatever is there alone.
    #[test]
    fn it_is_written_once_and_never_rewritten() {
        let d = std::env::temp_dir().join(format!(
            "flagents-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        write(&d);
        let path = d.join(FILE);
        assert!(path.exists(), "the guide was not written");

        std::fs::write(&path, "mine now").unwrap();
        write(&d);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "mine now",
            "somebody's own notes were overwritten"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
