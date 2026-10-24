//! **A model the engine cannot open becomes one it can, on its own.**
//!
//! An `.fbx`, `.obj`, `.stl`, `.ply`, `.blend`, `.dae` or `.usd` anywhere in
//! the project is converted to a `.glb` beside it in the background: dropped
//! in, copied in from a file manager, or imported through the Assets panel,
//! it is usable a moment later without anybody pressing anything. A source
//! that changes is converted again. All the actual work is
//! [`floptle_convert`]; this is the part that finds the work, keeps the
//! editor responsive while it happens, and says what happened afterwards.
//!
//! **Beside the source, not in a cache.** The `.glb` is the model a scene
//! names and a build ships; a cache would make every scene name a file that
//! is not in the project, that a teammate's clone does not have, and that an
//! export has to know to go and fetch. Next to the source it is an ordinary
//! asset, and the Assets panel shows the pair as the one model it is.
//!
//! **Only its own output is written over.** Every converted model carries a
//! stamp naming its source (see `floptle_convert::stamp`); a `.glb` without
//! one is somebody's own and is never touched, whatever sits beside it.
//!
//! **Off the main thread.** The input is a file somebody else made and there
//! is no bound on its size — a character FBX is tens of megabytes. Blocking the
//! frame loop on that is reported as "the editor froze", never as "the
//! conversion took a while", so both the search for work and the work itself
//! run on workers and are drained next frame.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use floptle_convert::Report;
use floptle_script::LogLevel;

/// How often the project is looked through for models to convert. Often
/// enough that a file copied in from outside the editor is ready by the time
/// anybody goes looking for it; a scan is a directory walk and a few stats.
const SCAN_EVERY: Duration = Duration::from_secs(3);

/// The conversions to do and the one being done.
#[derive(Default)]
pub(crate) struct ModelConverts {
    job: Option<Job>,
    queue: VecDeque<(PathBuf, bool)>,
    scan: Option<Receiver<Scan>>,
    last_scan: Option<Instant>,
    /// Sources whose last conversion failed, with the time the source had
    /// then. A broken file is not retried every three seconds; one that is
    /// saved again is.
    failed: HashMap<PathBuf, Option<SystemTime>>,
    /// Look for work without a window. The editor's window is what turns the
    /// search on: a headless verb (`floptle shot`, `floptle check`) must not
    /// start writing files into the project it was asked to look at.
    pub(crate) search_headless: bool,
    /// The project's model texture filter and imported-material rules as the
    /// loaded models were drawn with, so a change to either in Project
    /// Settings draws every model again.
    look_seen: Option<(floptle_scene::ModelTextureFilterDoc, floptle_scene::ImportedMaterialsDoc)>,
}

struct Job {
    rx: Receiver<Result<(PathBuf, Report), String>>,
    src: PathBuf,
    manual: bool,
    /// The output was the old button's, unstamped: said so when it is done.
    migrating: bool,
}

/// What a scan found: sources that need converting, and every source whose
/// converted model is in place.
struct Scan {
    pending: Vec<PathBuf>,
    pairs: Vec<(PathBuf, PathBuf)>,
}

/// Source → its converted model, for every model this converter wrote whose
/// source is still beside it. Read by the Assets panel, which shows each pair
/// as one model, and by everything that moves or deletes a file.
fn pairs() -> &'static RwLock<HashMap<PathBuf, PathBuf>> {
    static P: OnceLock<RwLock<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// The converted model of `source`, when there is one in place.
pub(crate) fn converted_model_of(source: &str) -> Option<PathBuf> {
    pairs().read().ok()?.get(Path::new(source)).cloned()
}

/// The source `glb` was converted from, when it is in place.
pub(crate) fn source_of_model(glb: &str) -> Option<PathBuf> {
    let p = Path::new(glb);
    pairs().read().ok()?.iter().find(|(_, g)| g.as_path() == p).map(|(s, _)| s.clone())
}

/// The file that is the other half of `path`'s model — its converted `.glb`,
/// or the source it was converted from — so a delete, move or rename takes
/// both. Read from disk, not from the last scan, because it decides what is
/// deleted.
pub(crate) fn companion_of(path: &Path) -> Option<PathBuf> {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("glb")) {
        return floptle_convert::source_of(path);
    }
    if floptle_convert::is_convertible(path)
        && matches!(floptle_convert::output_state(path), floptle_convert::Output::Converted { .. })
    {
        return Some(floptle_convert::output_path(path));
    }
    None
}

/// `paths` and the other half of each one's model, once each.
pub(crate) fn with_companions(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths.to_vec();
    for p in paths {
        if let Some(c) = companion_of(Path::new(p)) {
            let c = c.to_string_lossy().into_owned();
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// Every model source under `root` and the state of its output: what a scan
/// does on its worker. Dot-folders are the editor's own and are skipped.
fn scan(root: &Path) -> Scan {
    let mut out = Scan { pending: Vec::new(), pairs: Vec::new() };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if !floptle_convert::converts_automatically(&p) {
                continue;
            }
            match floptle_convert::output_state(&p) {
                floptle_convert::Output::Missing => out.pending.push(p),
                floptle_convert::Output::Converted { current } => {
                    out.pairs.push((p.clone(), floptle_convert::output_path(&p)));
                    if !current {
                        out.pending.push(p);
                    }
                }
                // Converted before outputs were stamped: done again, so it
                // is stamped and follows its source from now on.
                floptle_convert::Output::Unstamped => out.pending.push(p),
                floptle_convert::Output::TheirOwn => {}
            }
        }
    }
    out.pending.sort();
    out
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// The Console entry for one conversion: one line of counts, then which
/// picture went into which slot of which material, the clips carried, and
/// what was not found or not carried. A warning when any of the last is there,
/// so it is not lost among the lines that need nothing doing.
pub(crate) fn import_report(name: &str, out: &str, report: &Report) -> (LogLevel, String) {
    let mut s = format!("Imported {name} as {out}: {}", report.summary());
    let mut attention = false;
    for l in report.detail_lines() {
        s.push_str(&format!("\n  {l}"));
    }
    for l in report.missing_lines() {
        attention = true;
        s.push_str(&format!("\n  ⚠ {l}"));
    }
    for w in &report.warnings {
        // "Converted by Blender" and the unit note are information, not trouble.
        attention |= !(w.starts_with("Converted by Blender") || w.starts_with("The source measured in"));
        s.push_str(&format!("\n  {w}"));
    }
    if !report.dropped.is_empty() {
        attention = true;
        s.push_str(&format!("\n  Not carried across: {}.", report.dropped.join(", ")));
    }
    (if attention { LogLevel::Warn } else { LogLevel::Debug }, s)
}

impl crate::Editor {
    /// Convert `path` now, ahead of anything waiting — the Assets panel's
    /// Convert / Re-convert, and a source double-clicked or dropped before
    /// the background converter got to it.
    pub(crate) fn start_model_conversion(&mut self, path: &str) {
        let src = PathBuf::from(path);
        self.convert.failed.remove(&src);
        self.convert.queue.retain(|(p, _)| *p != src);
        if self.convert.job.as_ref().is_some_and(|j| j.src == src) {
            return;
        }
        self.convert.queue.push_front((src, true));
    }

    /// The model to place for a dropped `path` that the engine cannot open:
    /// its converted `.glb` when there is one. When there is not yet, the
    /// conversion is started and the reason nothing appeared is said.
    pub(crate) fn model_for_source(&mut self, path: &str) -> Option<String> {
        let src = Path::new(path);
        match floptle_convert::output_state(src) {
            floptle_convert::Output::Converted { .. } | floptle_convert::Output::Unstamped => {
                Some(floptle_convert::output_path(src).to_string_lossy().into_owned())
            }
            floptle_convert::Output::Missing => {
                self.start_model_conversion(path);
                let name = src.file_name().unwrap_or_default().to_string_lossy();
                self.toast = Some((format!("⏳  Converting {name} — drop it again in a moment."), 5.0));
                None
            }
            floptle_convert::Output::TheirOwn => {
                let msg = floptle_convert::ConvertError::WouldOverwrite(floptle_convert::output_path(src)).to_string();
                self.toast = Some((format!("⚠  {msg}"), 8.0));
                None
            }
        }
    }

    /// Find, start and finish conversions. Called once a frame; free when
    /// there is nothing to do.
    pub(crate) fn poll_model_conversion(&mut self) {
        self.redraw_models_if_look_changed();
        // ---- a finished scan queues what it found ----
        if let Some(rx) = &self.convert.scan {
            match rx.try_recv() {
                Ok(found) => {
                    self.convert.scan = None;
                    if let Ok(mut p) = pairs().write() {
                        p.retain(|s, _| !s.starts_with(&self.project_root));
                        p.extend(found.pairs);
                    }
                    for src in found.pending {
                        let failed_same = self.convert.failed.get(&src).is_some_and(|t| *t == mtime(&src));
                        let waiting = self.convert.queue.iter().any(|(p, _)| *p == src)
                            || self.convert.job.as_ref().is_some_and(|j| j.src == src);
                        if !failed_same && !waiting {
                            self.convert.queue.push_back((src, false));
                        }
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.convert.scan = None,
            }
        }
        let due = self.convert.last_scan.is_none_or(|t| t.elapsed() >= SCAN_EVERY);
        let searching = self.window.is_some() || self.convert.search_headless;
        if searching && self.convert.scan.is_none() && due && floptle_vfs::is_dir(&self.project_root) {
            self.convert.last_scan = Some(Instant::now());
            let root = self.project_root.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(scan(&root));
            });
            self.convert.scan = Some(rx);
        }

        // ---- a finished conversion is reported ----
        if let Some(job) = &self.convert.job {
            let result = match job.rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Empty) => None,
                // The worker died without sending — nothing to report but the fact.
                Err(TryRecvError::Disconnected) => Some(Err("the conversion stopped unexpectedly".to_string())),
            };
            if let Some(result) = result {
                let job = self.convert.job.take().expect("a job is running");
                self.finish_conversion(job, result);
            }
        }

        // ---- the next one starts ----
        if self.convert.job.is_none()
            && let Some((src, manual)) = self.convert.queue.pop_front()
        {
            if !floptle_vfs::is_file(&src) {
                return;
            }
            let migrating = floptle_convert::output_state(&src) == floptle_convert::Output::Unstamped;
            let (tx, rx) = std::sync::mpsc::channel();
            let worker_src = src.clone();
            std::thread::spawn(move || {
                let r = floptle_convert::convert_file(&worker_src).map_err(|e| e.to_string());
                let _ = tx.send(r);
            });
            let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if manual {
                self.toast = Some((format!("⏳  Converting {name}…"), 30.0));
            }
            self.convert.job = Some(Job { rx, src, manual, migrating });
        }
    }

    /// Drop every loaded model when the project's model look changes, so the
    /// next frame imports each one again under the new setting.
    fn redraw_models_if_look_changed(&mut self) {
        let now = (self.project.model_texture_filter_resolved(), self.project.imported_materials_resolved());
        let before = self.convert.look_seen.replace(now);
        if before.is_none_or(|b| b == now) {
            return;
        }
        let models: Vec<String> = self.mesh_registry.keys().filter(|k| crate::assets::is_model(k)).cloned().collect();
        for k in &models {
            self.mesh_registry.remove(k);
        }
        if !models.is_empty() {
            self.anim.revision += 1;
        }
    }

    fn finish_conversion(&mut self, job: Job, result: Result<(PathBuf, Report), String>) {
        let name = job.src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match result {
            Ok((out, report)) => {
                self.convert.failed.remove(&job.src);
                let file = out.file_name().unwrap_or_default().to_string_lossy().into_owned();
                if job.migrating {
                    self.console.push(
                        LogLevel::Debug,
                        format!(
                            "Updated {file}: it was converted from {name} by an earlier Floptle, and has been \
                             converted again with every map and stamped, so it now follows {name} when it changes."
                        ),
                        None,
                    );
                }
                let (level, text) = import_report(&name, &file, &report);
                self.console.push(level, text, None);
                if let Ok(mut p) = pairs().write() {
                    p.insert(job.src.clone(), out.clone());
                }
                // A model already in the scene is drawn from the old file
                // until its registration is dropped; the next frame's sweep
                // imports the new one.
                let abs = out.to_string_lossy().into_owned();
                let rel = crate::assets::asset_rel_path(&abs, &self.project_root);
                let reloaded = self.mesh_registry.remove(&rel).is_some() | self.mesh_registry.remove(&abs).is_some();
                if reloaded {
                    self.anim.revision += 1;
                }
                self.asset_tree = crate::assets::build_assets(&self.project_root);
                let note = if level == LogLevel::Warn { "  (see the Console)" } else { "" };
                if job.manual || level == LogLevel::Warn {
                    self.toast = Some((format!("✔  {file} — {}{note}", report.summary()), 8.0));
                }
                // The source row is now this model: keep it selected as that.
                if self.selected_asset.as_deref() == Some(&*job.src.to_string_lossy()) {
                    self.selected_asset = Some(abs);
                }
            }
            Err(e) => {
                self.convert.failed.insert(job.src.clone(), mtime(&job.src));
                self.console.push(LogLevel::Error, format!("Could not import {name}: {e}"), None);
                self.toast = Some((format!("✖  {name} — see the Console"), 8.0));
            }
        }
    }

    /// After `from` was renamed to `to`, give the other half of its model
    /// (found before the rename, as `companion`) the same new name.
    pub(crate) fn rename_model_companion(&mut self, companion: Option<PathBuf>, to: &Path) {
        let Some(c) = companion else { return };
        let (Some(stem), Some(ext)) = (to.file_stem(), c.extension()) else { return };
        let dest = c.with_file_name(stem).with_extension(ext);
        if dest == c || floptle_vfs::exists(&dest) {
            return;
        }
        if let Err(e) = std::fs::rename(&c, &dest) {
            self.console.push(
                LogLevel::Error,
                format!("{} was renamed, but {} did not follow: {e}", to.display(), c.display()),
                None,
            );
        }
        // The pair is found again under its new name on the next scan.
        self.convert.last_scan = None;
    }

    /// Look for models to convert on the next frame rather than at the next
    /// scheduled scan: files were just copied in.
    pub(crate) fn look_for_models_now(&mut self) {
        self.convert.last_scan = None;
    }

    /// Whether a background conversion is running or waiting. For tests and
    /// the headless verbs that wait for a project to settle.
    #[allow(dead_code)]
    pub(crate) fn model_conversions_busy(&self) -> bool {
        self.convert.job.is_some() || !self.convert.queue.is_empty() || self.convert.scan.is_some()
    }
}

#[cfg(feature = "editor-ui")]
#[cfg(test)]
mod tests {
    use super::*;

    fn wait(ed: &mut crate::Editor) {
        let start = Instant::now();
        loop {
            ed.poll_model_conversion();
            if !ed.model_conversions_busy() {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(60), "the converter never settled");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn models() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../floptle-convert/tests/models")
    }

    /// **A model put in a project converts on its own, once; again when it
    /// changes; and a `.glb` somebody made is never touched.** Driven through
    /// the editor's own frame poll, with nothing pressed.
    #[test]
    fn a_model_in_the_project_converts_by_itself_and_only_its_own_output_is_replaced() {
        let root = std::env::temp_dir().join(format!("floptle-autoconvert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("models/pack")).unwrap();
        std::fs::copy(models().join("blender_272_cube_7400_binary.fbx"), root.join("models/cube.fbx")).unwrap();
        std::fs::copy(models().join("blender_279_ball_7400_binary.fbx"), root.join("models/pack/ball.fbx")).unwrap();
        // Somebody's own model, beside a source of the same name.
        std::fs::copy(models().join("blender_272_cube_7400_binary.fbx"), root.join("models/mine.fbx")).unwrap();
        std::fs::write(root.join("models/mine.glb"), b"glTF hand made").unwrap();

        let mut ed = crate::Editor { project_root: root.clone(), ..Default::default() };
        wait(&mut ed);
        assert!(!root.join("models/cube.glb").exists(), "with no window and nothing asked, nothing is written");
        ed.convert.search_headless = true;
        wait(&mut ed);
        let cube = root.join("models/cube.glb");
        assert!(floptle_convert::stamp::read_stamp(&cube).is_some(), "the cube converted with nothing pressed");
        assert!(root.join("models/pack/ball.glb").is_file(), "in a subfolder too");
        assert_eq!(std::fs::read(root.join("models/mine.glb")).unwrap(), b"glTF hand made", "their own .glb is untouched");
        assert_eq!(converted_model_of(&root.join("models/cube.fbx").to_string_lossy()), Some(cube.clone()));
        let report = ed.console.entries.iter().find(|e| e.msg.starts_with("Imported cube.fbx")).expect("a Console report");
        assert!(report.msg.contains("1 mesh, 12 triangles"), "{}", report.msg);

        // Once: a second pass converts nothing.
        let first = std::fs::metadata(&cube).unwrap().modified().unwrap();
        ed.convert.last_scan = None;
        wait(&mut ed);
        assert_eq!(std::fs::metadata(&cube).unwrap().modified().unwrap(), first, "converted once is converted");
        assert_eq!(ed.console.entries.iter().filter(|e| e.msg.starts_with("Imported cube.fbx")).count(), 1);

        // Again when the source changes.
        std::fs::copy(models().join("blender_279_ball_7400_binary.fbx"), root.join("models/cube.fbx")).unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(root.join("models/cube.fbx")).unwrap().set_modified(later).unwrap();
        ed.convert.last_scan = None;
        wait(&mut ed);
        assert_eq!(ed.console.entries.iter().filter(|e| e.msg.starts_with("Imported cube.fbx")).count(), 2, "a changed source converts again");
        assert_eq!(std::fs::read(root.join("models/mine.glb")).unwrap(), b"glTF hand made");

        // Deleting the model takes its source with it, or the source would
        // only convert again.
        assert_eq!(with_companions(&[cube.to_string_lossy().into_owned()]).len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `.glb` written by the engine's own writer, signed `generator`
    /// instead: the name is written over the current one and padded with
    /// spaces after its closing quote, so the JSON stays valid and the chunk
    /// keeps its length.
    fn glb_signed(generator: &str) -> Vec<u8> {
        let mesh = floptle_assets::WriteMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            indices: vec![0, 1, 2],
            base_color: [1.0; 4],
            ..Default::default()
        };
        let mut glb = floptle_assets::write_glb(&[floptle_assets::WriteNode::mesh_node("t", mesh)], &[], &[], &[]);
        let from = format!("\"{}\"", floptle_assets::gltf_import::GENERATOR);
        let at = glb.windows(from.len()).position(|w| w == from.as_bytes()).expect("the generator is in the JSON");
        let mut to = format!("\"{generator}\"").into_bytes();
        assert!(to.len() <= from.len());
        to.resize(from.len(), b' ');
        glb[at..at + from.len()].copy_from_slice(&to);
        glb
    }

    /// **A model the old Convert button made is converted again and
    /// stamped**, recognised by that button's generator and its source
    /// beside it. A file with the old generator and no source, or a source
    /// and a `.glb` from anything else, is somebody's own and is not touched.
    #[test]
    fn an_unstamped_conversion_beside_its_source_is_updated_and_nothing_else_is() {
        let root = std::env::temp_dir().join(format!("floptle-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cube = models().join("blender_272_cube_7400_binary.fbx");
        let old = glb_signed("Floptle glb_write");
        let blender = glb_signed("Khronos Blender I/O");
        std::fs::copy(&cube, root.join("crate.fbx")).unwrap();
        std::fs::write(root.join("crate.glb"), &old).unwrap();
        std::fs::write(root.join("lonely.glb"), &old).unwrap();
        std::fs::copy(&cube, root.join("theirs.fbx")).unwrap();
        std::fs::write(root.join("theirs.glb"), &blender).unwrap();
        assert_eq!(floptle_convert::stamp::read_generator(&root.join("crate.glb")).as_deref(), Some("Floptle glb_write"));

        let mut ed = crate::Editor { project_root: root.clone(), ..Default::default() };
        ed.convert.search_headless = true;
        wait(&mut ed);
        assert!(floptle_convert::stamp::read_stamp(&root.join("crate.glb")).is_some(), "the old conversion was redone and stamped");
        assert!(
            ed.console.entries.iter().any(|e| e.msg.starts_with("Updated crate.glb") && e.msg.contains("crate.fbx")),
            "one line says so"
        );
        assert_eq!(std::fs::read(root.join("lonely.glb")).unwrap(), old, "no source: left byte for byte");
        assert_eq!(std::fs::read(root.join("theirs.glb")).unwrap(), blender, "another generator: left byte for byte");
        assert_eq!(ed.console.entries.iter().filter(|e| e.msg.starts_with("Updated")).count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_report_says_what_went_where_and_what_is_missing() {
        let mut r = Report { meshes: 2, triangles: 10, materials: 1, ..Default::default() };
        r.maps.push(floptle_convert::MapUse {
            material: "Crate".into(),
            slot: floptle_convert::MapSlot::Normal,
            texture: "crate_n.png".into(),
            note: None,
        });
        let (level, text) = import_report("crate.fbx", "crate.glb", &r);
        assert_eq!(level, LogLevel::Debug);
        assert!(text.contains("Crate: normal ← crate_n.png"), "{text}");
        r.missing.push(floptle_convert::MissingMap {
            material: "Crate".into(),
            slot: floptle_convert::MapSlot::BaseColor,
            name: "crate_d.png".into(),
        });
        let (level, text) = import_report("crate.fbx", "crate.glb", &r);
        assert_eq!(level, LogLevel::Warn, "a missing picture asks for attention");
        assert!(text.contains("crate_d.png") && text.contains("base colour"), "{text}");
    }

    fn maps() -> floptle_assets::gltf_import::PartMaps {
        floptle_assets::gltf_import::PartMaps {
            explicit: true,
            alpha: 1.0,
            double_sided: true,
            metallic: 1.0,
            roughness: 0.2,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
            ..Default::default()
        }
    }

    /// **A material's factors draw without a map.** A metal ball exported
    /// with metallic 1 and roughness 0.2 and no pictures used to draw as
    /// plain plastic, because the look was only built for a part with maps.
    /// A part with no material at all keeps the plain look.
    #[test]
    fn an_imported_material_honours_its_factors_with_no_maps() {
        let m = crate::project::imported_material([0.9, 0.8, 0.5], &maps(), AS_FILE);
        assert_eq!(m.shading, floptle_core::Shading::Physical);
        assert_eq!((m.metallic, m.roughness), (1.0, 0.2));
        let none = floptle_assets::gltf_import::PartMaps::default();
        let m = crate::project::imported_material([0.9, 0.8, 0.5], &none, AS_FILE);
        assert_eq!(m.shading, floptle_core::Shading::Classic, "no material, no say");
    }

    const AS_FILE: floptle_scene::ImportedMaterialsDoc = floptle_scene::ImportedMaterialsDoc::AsFile;

    /// **Which filter a model's textures take, most specific first**: the
    /// model's own choice, then the project's, then (with neither) each file's
    /// sampler and crisp where it says nothing. An old project's setting is
    /// "always crisp", as every model drew before 0.117.
    #[test]
    fn a_model_s_choice_beats_the_project_s_which_beats_the_file_s() {
        use crate::assets::FilterMode as F;
        use floptle_render::TexFilter as T;
        let project = |f| floptle_scene::ProjectConfigDoc { model_texture_filter: Some(f), ..floptle_scene::ProjectConfigDoc::for_new_project() };
        let new = floptle_scene::ProjectConfigDoc::for_new_project();
        let old: floptle_scene::ProjectConfigDoc = ron::from_str("()").unwrap();
        let file = [Some(T::SmoothMipmaps), None];
        let filter = |model, cfg: &floptle_scene::ProjectConfigDoc, i| {
            crate::project::embedded_sampling(crate::project::model_filter_choice(model, cfg), &file, Some(i)).filter
        };
        assert_eq!(filter(None, &new, 0), T::SmoothMipmaps, "as each file says");
        assert_eq!(filter(None, &new, 1), T::Pixelated, "crisp where the file says nothing");
        assert_eq!(filter(None, &old, 0), T::Pixelated, "an old project draws crisp, as in 0.116");
        assert_eq!(filter(None, &project(floptle_scene::ModelTextureFilterDoc::Smooth), 1), T::SmoothMipmaps);
        assert_eq!(filter(Some(F::Pixelated), &project(floptle_scene::ModelTextureFilterDoc::Smooth), 0), T::Pixelated, "the model's own choice wins");
    }

    /// **The 0.116 rules, kept for an old project**: a factor-only metal is
    /// not metal, every picture cuts at half alpha, every face is drawn from
    /// both sides and the texture transform is not applied — exactly what
    /// that project's models looked like.
    #[test]
    fn classic_imported_materials_draw_as_0116_did() {
        use floptle_assets::glb_write::AlphaMode;
        let classic = floptle_scene::ImportedMaterialsDoc::Classic;
        let maps = floptle_assets::gltf_import::PartMaps {
            alpha_mode: AlphaMode::Opaque,
            double_sided: false,
            uv_transform: Some(floptle_assets::gltf_import::UvTransform { offset: [0.5, 0.0], scale: [2.0, 2.0], rotation: 0.0 }),
            ..maps()
        };
        let m = crate::project::imported_material([0.9, 0.8, 0.5], &maps, classic);
        assert_eq!(m.shading, floptle_core::Shading::Classic, "a factor-only metal draws as non-metal");
        assert_eq!((m.blend, m.alpha_cutoff), (floptle_core::BlendMode::Cutout, 0.5));
        assert!(m.double_sided);
        assert_eq!(m.tiling, None);
        // A metallic-roughness map was physical in 0.116 too.
        let mapped = floptle_assets::gltf_import::PartMaps { metallic_roughness: Some(0), ..maps };
        assert_eq!(crate::project::imported_material([1.0; 3], &mapped, classic).shading, floptle_core::Shading::Physical);
        // New projects draw what the file says; an old file reads as Classic.
        assert_eq!(floptle_scene::ProjectConfigDoc::for_new_project().imported_materials_resolved(), AS_FILE);
        let old: floptle_scene::ProjectConfigDoc = ron::from_str("()").unwrap();
        assert_eq!(old.imported_materials_resolved(), classic);
    }

    /// Alpha mode, cutoff and sidedness reach the engine's material.
    #[test]
    fn an_imported_material_keeps_its_alpha_mode_and_sides() {
        use floptle_assets::glb_write::AlphaMode;
        let with = |alpha_mode, double_sided| {
            crate::project::imported_material([1.0; 3], &floptle_assets::gltf_import::PartMaps { alpha_mode, double_sided, ..maps() }, AS_FILE)
        };
        use floptle_core::BlendMode;
        let mask = with(AlphaMode::Mask(0.3), true);
        assert_eq!((mask.blend, mask.alpha_cutoff), (BlendMode::Cutout, 0.3), "MASK cuts at its own cutoff");
        let faded = crate::project::imported_material(
            [1.0; 3],
            &floptle_assets::gltf_import::PartMaps { alpha_mode: AlphaMode::Mask(0.5), alpha: 0.5, ..maps() },
            AS_FILE,
        );
        assert_eq!((faded.blend, faded.alpha), (BlendMode::Cutout, 0.5), "a MASK's factor alpha multiplies into the cut");
        assert_eq!(with(AlphaMode::Opaque, true).blend, BlendMode::Opaque, "OPAQUE cuts nothing");
        let blend = with(AlphaMode::Blend, true);
        assert_eq!((blend.blend, blend.alpha), (BlendMode::Blend, 1.0), "BLEND blends at the factor it names");
        assert!(!with(AlphaMode::Opaque, false).double_sided);
        assert!(with(AlphaMode::Opaque, true).double_sided);
    }

    /// **A texture transform lands where glTF puts it.** glTF rotates and
    /// scales about the UV origin, the engine about the centre; the converted
    /// tiling must send every UV to the same place.
    #[test]
    fn a_texture_transform_becomes_the_same_tiling() {
        use floptle_assets::gltf_import::UvTransform;
        for t in [
            UvTransform { offset: [0.25, -0.5], scale: [2.0, 3.0], rotation: 0.0 },
            UvTransform { offset: [0.1, 0.7], scale: [1.5, 0.5], rotation: 0.6 },
            UvTransform { offset: [0.0, 0.0], scale: [1.0, 1.0], rotation: -1.2 },
        ] {
            let floptle_core::Tiling::Uv { count, offset, rotation } = crate::project::uv_tiling(t) else {
                panic!("a UV tiling")
            };
            for uv in [[0.0f32, 0.0], [1.0, 0.0], [0.3, 0.9]] {
                let (s, c) = t.rotation.sin_cos();
                let su = [t.scale[0] * uv[0], t.scale[1] * uv[1]];
                let gltf = [t.offset[0] + c * su[0] - s * su[1], t.offset[1] + s * su[0] + c * su[1]];
                // The engine's shader: R·(count·(uv − ½)) + ½ + offset.
                let (s, c) = rotation.to_radians().sin_cos();
                let v = [count[0] * (uv[0] - 0.5), count[1] * (uv[1] - 0.5)];
                let ours = [c * v[0] - s * v[1] + 0.5 + offset[0], s * v[0] + c * v[1] + 0.5 + offset[1]];
                for k in 0..2 {
                    assert!((gltf[k] - ours[k]).abs() < 1e-4, "{t:?} at {uv:?}: glTF {gltf:?}, engine {ours:?}");
                }
            }
        }
    }
}
