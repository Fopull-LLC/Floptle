//! Files the player hands a running game: `app.openFile` and `filesDropped`
//! (see `floptle_script::file_api`). The game gets names and bytes, never a
//! path.

use std::path::Path;

/// Read the file at `path` for a game, if it is at most `max` bytes: its
/// name and its bytes, or why not (`too-large`, or what the read said).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn read_for_game(path: &Path, max: u64) -> Result<(String, Vec<u8>), String> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    // Asked before reading, so a film dropped by mistake is never loaded.
    if meta.len() > max {
        return Err("too-large".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max {
        return Err("too-large".into());
    }
    Ok((name, bytes))
}

/// The browser has no disk: nothing reaches a game this way there yet.
#[cfg(target_arch = "wasm32")]
pub(crate) fn read_for_game(_path: &Path, _max: u64) -> Result<(String, Vec<u8>), String> {
    Err("unsupported".into())
}

impl crate::Editor {
    /// Once a Play frame, before the scripts: open a picker for each
    /// `app.openFile`, answer the pickers that closed, and hand the files
    /// dropped on the window to `filesDropped`.
    pub(crate) fn pump_files(&mut self) {
        for req in self.script_host.take_open_file_requests() {
            self.start_open_file(req);
        }
        #[cfg(feature = "dialogs")]
        {
            let mut open = std::mem::take(&mut self.open_files);
            open.retain(|(id, max, rx)| match crate::native_dialog::poll(rx) {
                crate::native_dialog::Answer::Waiting => true,
                crate::native_dialog::Answer::Chose(paths) => {
                    let answer = match paths.first() {
                        Some(p) => read_for_game(p, *max),
                        None => Err("cancelled".into()),
                    };
                    self.script_host.answer_open_file(*id, answer);
                    false
                }
                crate::native_dialog::Answer::Closed => {
                    self.script_host.answer_open_file(*id, Err("cancelled".into()));
                    false
                }
            });
            self.open_files = open;
        }
        if self.dropped_for_game.is_empty() {
            return;
        }
        let files: Vec<(String, Vec<u8>)> = std::mem::take(&mut self.dropped_for_game)
            .iter()
            .filter_map(|p| match read_for_game(p, floptle_script::OPEN_FILE_MAX_BYTES) {
                Ok(f) => Some(f),
                Err(why) => {
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("filesDropped: \"{name}\" was not handed to the game ({why})"),
                        None,
                    );
                    None
                }
            })
            .collect();
        self.script_host.call_files_dropped(&mut self.world, &files);
    }

    #[cfg(feature = "dialogs")]
    fn start_open_file(&mut self, req: floptle_script::OpenFileRequest) {
        let label = req.filter.join(", ");
        let filter = (!req.filter.is_empty()).then_some((label.as_str(), req.filter.as_slice()));
        let rx = crate::native_dialog::pick_files_filtered(&req.title, filter, false);
        self.open_files.push((req.id, req.max_bytes, rx));
    }

    /// No picker in this build (a dedicated server, the browser for now).
    #[cfg(not(feature = "dialogs"))]
    fn start_open_file(&mut self, req: floptle_script::OpenFileRequest) {
        self.script_host.answer_open_file(req.id, Err("unsupported".into()));
    }
}

#[cfg(test)]
mod tests {
    use floptle_core::{ScriptInst, Scripts, Transform};

    const SCRIPT: &str = r#"
function start(node)
  app.openFile({ title = "Emblem", filter = { "PNG", ".jpg" }, maxBytes = 1000 }, function(n, b, e)
    name = n; size = b and #b; err = e
  end)
  app.openFile(nil, function(n, b, e) second = e end)
  ok, bad = pcall(app.openFile, { colour = 1 }, function() end)
end
function filesDropped(node, files)
  dropped = #files
  dropName = files[1].name
  dropSize = #files[1].bytes
end
"#;

    fn setup(tag: &str) -> (crate::Editor, floptle_core::Entity, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("floptle-files-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("asker.lua"), SCRIPT).unwrap();
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::IDENTITY);
        ed.world.insert(
            e,
            Scripts(vec![ScriptInst {
                kind: "asker".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        (ed, e, dir)
    }

    fn get<T: mlua::FromLua>(ed: &crate::Editor, e: floptle_core::Entity, key: &str) -> Option<T> {
        ed.script_host.instance_env(e.index(), "asker").and_then(|env| env.get::<Option<T>>(key).ok().flatten())
    }

    /// `app.openFile` asks the driver for one picker, filtered and capped as
    /// the script said; a second while it is open answers "busy"; and the
    /// picked file comes back as its name and bytes, never its path.
    #[test]
    fn a_game_asks_for_a_file_and_gets_its_name_and_bytes() {
        let (mut ed, e, dir) = setup("open");
        ed.script_host.run(&mut ed.world, &dir, 1.0 / 60.0, 0.0);
        assert_eq!(get::<bool>(&ed, e, "ok"), Some(false), "an unknown option should be refused");
        let reqs = ed.script_host.take_open_file_requests();
        assert_eq!(reqs.len(), 1, "one picker at a time: {reqs:?}");
        assert_eq!(reqs[0].filter, ["png", "jpg"]);
        assert_eq!(reqs[0].max_bytes, 1000);
        let file = dir.join("logo.png");
        std::fs::write(&file, [7u8; 300]).unwrap();
        // The driver's part: what `pump_files` does once the picker closes.
        let answer = super::read_for_game(&file, reqs[0].max_bytes);
        ed.script_host.answer_open_file(reqs[0].id, answer);
        ed.script_host.run(&mut ed.world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert_eq!(get::<String>(&ed, e, "name").as_deref(), Some("logo.png"), "the name, not the path");
        assert_eq!(get::<f64>(&ed, e, "size"), Some(300.0));
        assert_eq!(get::<String>(&ed, e, "err"), None);
        assert_eq!(get::<String>(&ed, e, "second").as_deref(), Some("busy"));
        assert_eq!(super::read_for_game(&file, 100), Err("too-large".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file dropped on the window during Play reaches `filesDropped` as its
    /// name and bytes; one that cannot be read is named in the Console and
    /// left out.
    #[test]
    fn a_file_dropped_during_play_reaches_files_dropped() {
        let (mut ed, e, dir) = setup("drop");
        ed.project_root = dir.clone();
        ed.script_host.run(&mut ed.world, &dir, 1.0 / 60.0, 0.0);
        let _ = ed.script_host.take_open_file_requests();
        let file = dir.join("save.dat");
        std::fs::write(&file, [1u8; 42]).unwrap();
        ed.dropped_for_game = vec![file, dir.join("missing.dat")];
        ed.pump_files();
        assert_eq!(get::<f64>(&ed, e, "dropped"), Some(1.0));
        assert_eq!(get::<String>(&ed, e, "dropName").as_deref(), Some("save.dat"));
        assert_eq!(get::<f64>(&ed, e, "dropSize"), Some(42.0));
        assert!(
            ed.console.entries.iter().any(|m| m.msg.contains("missing.dat")),
            "the file that could not be read should be named"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
