//! `floptle check`: does this project still load?
//!
//! The question anything editing a project by hand or by script needs
//! answered after every edit, without opening the editor. A `.ron` file that
//! parses is not a scene that works: a parent index can point past the end
//! of the list, a material can name a texture that is not there, a node can
//! carry a script with no file behind it. Each of those reaches you later as
//! a symptom somewhere else.
//!
//! It runs the engine's own checks, not a second opinion. Scenes load through
//! `floptle_scene::load`, the wiring goes through `validate_parents` and
//! `validate_ui_visibility` at the same two levels
//! `Project::report_scene_wiring` pushes them to the Console at, and assets
//! resolve through `project::resolve_asset_path`, the same rescue chain the
//! editor resolves them with, so a reference that works in the editor is not
//! reported as broken here.
//!
//! No GPU and no window. Nothing in this file draws anything; a verb that
//! wanted to render a thumbnail would be a different verb.
//!
//! What it looks at, since a checker's silence is read as a pass: every
//! scene, prefab, effect and standalone material; every node's material maps
//! (colour, normal, roughness, metallic, ambient occlusion), its surface
//! shader and that shader's own textures, its model and its scripts; every
//! effect track's texture, mesh and trail; and the entry scene `project.ron`
//! names.
//!
//! What it does not: animation controllers, audio references, and anything
//! inside an installed package.

use std::path::{Path, PathBuf};

/// How bad one finding is.
///
/// The two levels the Console uses for the same checks. An error means the
/// project is wrong; a warning means it is suspicious and still loads.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(Debug))]
pub(crate) enum Level {
    Error,
    Warning,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
        }
    }
}

/// One finding, in the shape every verb reports diagnostics in.
#[cfg_attr(test, derive(Debug))]
pub(crate) struct Finding {
    pub(crate) level: Level,
    pub(crate) message: String,
    /// Project-relative, so the same finding reads the same from any directory.
    pub(crate) file: Option<String>,
}

/// Everything one run found.
#[derive(Default)]
pub(crate) struct Report {
    pub(crate) findings: Vec<Finding>,
    /// What was actually looked at, so "no findings" can be told apart from
    /// "nothing was examined" — which is the failure mode a checker has.
    pub(crate) scenes: usize,
    pub(crate) prefabs: usize,
    pub(crate) effects: usize,
    pub(crate) materials: usize,
    pub(crate) shaders: usize,
    /// Every shader a scene, prefab or material names, with the stage the
    /// place that names it needs. Judged once the whole tree is read.
    shader_refs: Vec<ShaderRef>,
}

/// A `.flsl` named somewhere, and what it has to be there.
struct ShaderRef {
    path: String,
    stage: floptle_shader::Stage,
    who: String,
    where_: String,
}

impl Report {
    fn error(&mut self, file: Option<String>, message: impl Into<String>) {
        self.findings.push(Finding { level: Level::Error, message: message.into(), file });
    }

    fn warn(&mut self, file: Option<String>, message: impl Into<String>) {
        self.findings.push(Finding { level: Level::Warning, message: message.into(), file });
    }

    pub(crate) fn errors(&self) -> usize {
        self.findings.iter().filter(|f| f.level == Level::Error).count()
    }

    pub(crate) fn warnings(&self) -> usize {
        self.findings.len() - self.errors()
    }

    fn examined(&self) -> usize {
        self.scenes + self.prefabs + self.effects + self.materials + self.shaders
    }
}

/// Directories a check never descends into.
///
/// `packages/` holds somebody else's code, installed rather than authored, and
/// reporting its contents as this project's problems would be noise nobody here
/// can act on. `.floptle/` is generated.
fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || name == "packages" || name == "target"
}

/// Every file under `root`, depth-first, skipping the directories above.
fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // Sorted, so two runs over the same project report in the same order — a
    // diff of two reports should show what changed, not what was walked first.
    paths.sort();
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if p.is_dir() {
            if !skip_dir(&name) {
                walk(&p, out);
            }
        } else {
            out.push(p);
        }
    }
}

/// `path`, written the way somebody would type it from the project root.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Check `root`. Returns the process exit code: 0 clean, 1 something is wrong
/// with the project, 2 the thing named is not a project.
///
/// Those last two are kept apart, the way `run`, `shot` and `exec` keep them:
/// "your path is wrong" and "your project is broken" are the exact distinction
/// this verb exists to draw, so a caller must not have to read the prose to
/// tell them apart.
pub(crate) fn run(root: &Path, json: bool, fix: bool) -> i32 {
    if !root.join("project.ron").is_file() {
        floptle_say::say_err!("{} is not a project directory (no project.ron)", root.display());
        return 2;
    }
    if fix {
        for (file, n) in fix_absolute_paths(root) {
            floptle_say::say_err!("fixed {n} absolute path(s) in {file}");
        }
    }
    let report = examine(root);
    if json {
        print_json(&report);
    } else {
        print_text(&report, root);
    }
    i32::from(report.errors() > 0)
}

/// The project's per-texture import settings, keyed the way scenes name
/// textures. Empty when a project has never sliced one.
fn texture_settings(root: &Path) -> std::collections::HashMap<String, crate::assets::TexSetting> {
    let path = root.join(".floptle").join("textures.ron");
    let raw: std::collections::HashMap<String, crate::assets::TexSetting> =
        std::fs::read_to_string(&path).ok().and_then(|s| ron::from_str(&s).ok()).unwrap_or_default();
    raw.into_iter().map(|(k, v)| (crate::assets::asset_rel_path(&k, root), v)).collect()
}

/// Everything the check looks at, in one pass.
pub(crate) fn examine(root: &Path) -> Report {
    let mut r = Report::default();

    if !root.is_dir() {
        r.error(None, format!("{} is not a directory", root.display()));
        return r;
    }

    // The project file first: everything else is read relative to what it says.
    let cfg_path = root.join("project.ron");
    match floptle_scene::try_load_project(&cfg_path) {
        Ok(Some(c)) => {
            // **The scene the game starts in.** Nothing else in a project points
            // at it, so a rename or a delete leaves a project that checks clean
            // and opens on nothing.
            if let Some(entry) = c.entry_scene.as_deref().filter(|e| !e.is_empty())
                && crate::inspect::resolve_scene(root, entry).is_none()
            {
                r.error(
                    Some("project.ron".into()),
                    format!("entry_scene names {entry}, and there is no such scene"),
                );
            }
        }
        // `run` refuses this before it gets here (exit 2, not a finding); kept
        // because `examine` is also called directly.
        Ok(None) => r.error(
            Some("project.ron".into()),
            "no project.ron here — this is not a project directory",
        ),
        Err(e) => r.error(Some("project.ron".into()), format!("{e}")),
    }

    let settings = texture_settings(root);
    let mut files = Vec::new();
    walk(root, &mut files);

    check_linked_packages(root, &mut r);
    for path in &files {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let where_ = rel(root, path);
        if (name.ends_with(".ron") && where_ != "packages.ron") || name.ends_with(".flsl") {
            check_absolute_paths(root, path, &where_, &mut r);
        }
        if name.ends_with(".palette") {
            check_palette_paths(root, path, &where_, &mut r);
        }
        if name.ends_with(".vfx.ron") {
            r.effects += 1;
            match floptle_scene::load_vfx_effect(path) {
                Ok(doc) => check_effect(root, &doc, &where_, &mut r),
                Err(e) => r.error(Some(where_), format!("{e}")),
            }
        } else if name.ends_with(".prefab.ron") {
            r.prefabs += 1;
            check_prefab(root, path, &where_, &settings, &mut r);
        } else if name.ends_with(".ron") && in_dir(root, path, "scenes") {
            r.scenes += 1;
            check_scene(root, path, &where_, &settings, &mut r);
        } else if name.ends_with(".ron") && in_dir(root, path, "materials") {
            r.materials += 1;
            check_material_file(root, path, &where_, &settings, &mut r);
        } else if name.ends_with(".flsl") {
            r.shaders += 1;
            check_shader_file(path, &where_, &mut r);
        }
    }
    check_shader_refs(root, &mut r);
    check_cloud_collections(root, &files, &mut r);
    r
}

/// Every literal `cloud.rank/docs/blobs/counter("name")` in the project's
/// scripts: `(file, line, kind, collection, says private)`. A board's
/// collection is the part before its first `:`.
fn cloud_calls(root: &Path, files: &[PathBuf]) -> Vec<(String, usize, &'static str, String, bool)> {
    let mut out = Vec::new();
    for path in files.iter().filter(|p| p.extension().is_some_and(|x| x == "lua")) {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for (i, line) in text.lines().enumerate() {
            for kind in ["rank", "docs", "blobs", "counter"] {
                let call = format!("cloud.{kind}(");
                let mut rest = line;
                while let Some(at) = rest.find(&call) {
                    rest = &rest[at + call.len()..];
                    let t = rest.trim_start();
                    let Some(q) = t.chars().next().filter(|c| *c == '"' || *c == '\'') else { continue };
                    let Some(end) = t[1..].find(q) else { continue };
                    let name = &t[1..1 + end];
                    let collection = name.split(':').next().unwrap_or(name).to_string();
                    let private = t[1 + end..].split(')').next().is_some_and(|a| a.contains("private") && a.contains("true"));
                    out.push((rel(root, path), i + 1, kind, collection, private));
                }
            }
        }
    }
    out
}

/// **A collection the code uses and the project does not declare.** An
/// undeclared ranking auto-creates keeping each player's HIGHEST value, so a
/// time trial that ships before somebody declares it keeps every player's
/// slowest time, silently, forever; a public docs or blobs collection and every
/// counter refuse writes until declared.
fn check_cloud_collections(root: &Path, files: &[PathBuf], r: &mut Report) {
    let calls = cloud_calls(root, files);
    if calls.is_empty() {
        return;
    }
    let declared: std::collections::HashSet<String> =
        match std::fs::read_to_string(root.join(crate::cloud_collections::FILE)) {
            Ok(text) => match crate::cloud_collections::parse(&text) {
                Ok(wants) => wants.into_iter().map(|w| w.name).collect(),
                // The file's own mistakes are `floptle cloud collections`'s to report.
                Err(_) => return,
            },
            Err(_) => Default::default(),
        };
    let mut said = std::collections::HashSet::new();
    for (file, line, kind, name, private) in calls {
        if declared.contains(&name) || !said.insert((kind, name.clone())) {
            continue;
        }
        let why = match kind {
            "rank" => "an undeclared ranking is created on its first write keeping each player's HIGHEST value, \
                       which ranks a time trial's slowest time first. Declare it with the `keep` it needs (Min for times)",
            "counter" => "a counter must be declared before anything can count into it",
            _ if private => continue,
            _ => "a public collection must be declared before anything can be written to it (a private one says \
                  { private = true } and creates itself)",
        };
        r.warn(
            Some(file),
            format!(
                "line {line}: cloud.{kind} uses the collection `{name}`, which is not in {}: {why}, then run \
                 `floptle cloud collections --apply`",
                crate::cloud_collections::FILE
            ),
        );
    }
}

/// **A path only this machine has.** Anything the editor wrote as
/// `/home/you/Game/models/tower.glb` loads here and is missing everywhere else,
/// and nothing else in a check would notice, because it resolves here.
fn check_absolute_paths(root: &Path, path: &Path, where_: &str, r: &mut Report) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for a in crate::abs_paths::find(&text, root) {
        let who = match &a.owner {
            Some(n) => format!("\"{n}\""),
            None => "the file".to_string(),
        };
        let what = a.field.as_deref().unwrap_or("a path");
        let file = format!("{where_}:{}", a.line);
        match &a.relative {
            Some(rel) => r.error(
                Some(file),
                format!(
                    "{who}: {what} is an absolute path ({}) — it loads on this machine and will \
                     be missing in an export or anywhere else. It should be \"{rel}\". Fix: \
                     floptle check --fix",
                    a.value
                ),
            ),
            None => r.error(
                Some(file),
                format!(
                    "{who}: {what} is {} — outside the project, so an export will not include it \
                     at all. Copy it into the project and point at the copy.",
                    a.value
                ),
            ),
        }
    }
}

/// **A terrain palette naming an image only this machine has.** One `path`
/// (then `|flags`) per line, not quoted, so [`check_absolute_paths`] cannot
/// see it.
fn check_palette_paths(root: &Path, path: &Path, where_: &str, r: &mut Report) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for (i, line) in text.lines().enumerate() {
        let value = line.split('|').next().unwrap_or("");
        if !(value.starts_with('/') || value.starts_with("\\\\") || value.get(1..3) == Some(":\\") || value.get(1..3) == Some(":/")) {
            continue;
        }
        let rel = floptle_scene::portable::rel_path(value, root);
        let file = format!("{where_}:{}", i + 1);
        if rel != value.replace('\\', "/") {
            r.error(
                Some(file),
                format!(
                    "terrain texture slot {} is an absolute path ({value}) — it loads on this machine and will \
                     be missing in an export or anywhere else. It should be \"{rel}\". Fix: floptle check --fix",
                    i + 1
                ),
            );
        } else {
            r.error(
                Some(file),
                format!(
                    "terrain texture slot {} is {value} — outside the project, so an export will not include \
                     it at all. Copy it into the project and point at the copy.",
                    i + 1
                ),
            );
        }
    }
}

/// Rewrite every in-project absolute path in the project's `.ron` and `.flsl`
/// files and terrain palettes to a project-relative one: `(file, how many)`
/// per file changed.
fn fix_absolute_paths(root: &Path) -> Vec<(String, usize)> {
    let mut files = Vec::new();
    walk(root, &mut files);
    let mut out = Vec::new();
    for path in files {
        let where_ = rel(root, &path);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let (fixed, n) = if where_.ends_with(".palette") {
            let mut n = 0;
            let lines: Vec<String> = text
                .lines()
                .map(|line| {
                    let (value, flags) = line.split_once('|').map_or((line, None), |(v, f)| (v, Some(f)));
                    let rel = floptle_scene::portable::rel_path(value, root);
                    if rel == value {
                        return line.to_string();
                    }
                    n += 1;
                    match flags {
                        Some(f) => format!("{rel}|{f}"),
                        None => rel,
                    }
                })
                .collect();
            (lines.join("\n"), n)
        } else if (where_.ends_with(".ron") && where_ != "packages.ron") || where_.ends_with(".flsl") {
            crate::abs_paths::relativize(&text, root)
        } else {
            continue;
        };
        if n > 0 && std::fs::write(&path, fixed).is_ok() {
            out.push((where_, n));
        }
    }
    out
}

/// **A linked package ships, and only from here.** An export copies an enabled
/// linked package into the build, so the build is fine; a copy of the project
/// on another machine is not, until the package is linked there too. Said as
/// a warning, since the project is not wrong on this machine.
fn check_linked_packages(root: &Path, r: &mut Report) {
    let Ok(reg) = floptle_package::registry::Registry::load(root) else { return };
    for e in reg.packages.iter().filter(|e| e.enabled && e.source.is_linked()) {
        let dir = e.root_in(root);
        if dir.strip_prefix(root).is_ok() {
            continue;
        }
        if !dir.is_dir() {
            r.error(
                Some("packages.ron".into()),
                format!("{} is linked to {}, and there is nothing there", e.id, dir.display()),
            );
        } else {
            r.warn(
                Some("packages.ron".into()),
                format!(
                    "{} is linked from outside the project ({}). An export copies it into the \
                     build; a copy of this project on another machine won't have it until it is \
                     linked there too.",
                    e.id,
                    dir.display()
                ),
            );
        }
    }
}

/// Is `path` inside `<root>/<dir>/`?
fn in_dir(root: &Path, path: &Path, dir: &str) -> bool {
    path.strip_prefix(root.join(dir)).is_ok()
}

/// **What an effect draws with.** Parsing one only proved it was a well-formed
/// effect; a track whose billboard texture, instanced mesh or trail texture is
/// missing still parses, still emits, and draws as untextured quads — which
/// reads on screen as "the effect is wrong" rather than "a file is gone".
fn check_effect(root: &Path, doc: &floptle_scene::VfxEffectDoc, where_: &str, r: &mut Report) {
    for t in &doc.tracks {
        let who = if t.name.is_empty() { "an unnamed track" } else { &t.name };
        let named: [(&str, Option<&str>); 3] = [
            (
                "texture",
                match &t.render {
                    floptle_scene::VfxRenderDoc::Billboard { texture }
                    | floptle_scene::VfxRenderDoc::Beam { texture } => texture.as_deref(),
                    floptle_scene::VfxRenderDoc::Mesh { .. } => None,
                },
            ),
            (
                "model",
                match &t.render {
                    floptle_scene::VfxRenderDoc::Mesh { asset_path } => Some(asset_path.as_str()),
                    _ => None,
                },
            ),
            ("trail texture", t.trail.as_ref().and_then(|tr| tr.texture.as_deref())),
        ];
        for (what, path) in named {
            let Some(path) = path.filter(|p| !p.is_empty()) else { continue };
            if !exists(root, path) {
                r.error(Some(where_.into()), format!("{who}: no {what} at {path}"));
            }
        }
    }
}

/// A prefab is the flat node list the clipboard writes, and it may carry the
/// clipboard's own tag line.
fn check_prefab(root: &Path, path: &Path, where_: &str, settings: &Settings, r: &mut Report) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return r.error(Some(where_.into()), format!("{e}")),
    };
    let body = text
        .trim_start()
        .strip_prefix("//floptle-nodes-v1")
        .unwrap_or(&text)
        .trim_start()
        .to_string();
    match ron::from_str::<Vec<floptle_scene::NodeDoc>>(&body) {
        Ok(docs) => {
            if let Some(i) = docs.iter().filter_map(|d| d.parent).find(|&i| i >= docs.len()) {
                r.error(
                    Some(where_.into()),
                    format!("parent index {i} is past the end of a {}-node prefab", docs.len()),
                );
            }
            for node in &docs {
                check_node_refs(root, node, where_, settings, r);
            }

        }
        Err(e) => r.error(Some(where_.into()), format!("not a prefab: {e}")),
    }
}

/// One scene: does it parse, is its wiring sound, and is everything it names
/// actually there.
type Settings = std::collections::HashMap<String, crate::assets::TexSetting>;

fn check_scene(root: &Path, path: &Path, where_: &str, settings: &Settings, r: &mut Report) {
    let doc = match floptle_scene::load(path) {
        Ok(d) => d,
        Err(e) => return r.error(Some(where_.into()), format!("{e}")),
    };
    // The engine's own two validators, at the levels the Console uses.
    for line in floptle_scene::validate_parents(&doc.nodes) {
        r.error(Some(where_.into()), line);
    }
    for line in floptle_scene::validate_ui_visibility(&doc.nodes) {
        r.warn(Some(where_.into()), line);
    }
    for node in &doc.nodes {
        check_node_refs(root, node, where_, settings, r);
    }
    // A scene file with no Skybox loads with the default one, a flat mid-grey,
    // which reads as a broken render rather than as "no sky". Only a scene with
    // its own camera is meant to be looked at alone; an additive layer carries
    // no Skybox on purpose and keeps the base scene's.
    let has = |f: fn(&floptle_scene::MatterDoc) -> bool| doc.nodes.iter().any(|n| f(&n.matter));
    if has(|m| matches!(m, floptle_scene::MatterDoc::Camera { .. }))
        && !has(|m| matches!(m, floptle_scene::MatterDoc::Skybox { .. }))
    {
        r.warn(
            Some(where_.into()),
            "no Skybox node, so the scene opens with the default flat mid-grey sky; \
             add a Skybox to choose its colour, texture or shader",
        );
    }
}

/// Everything one node names: its materials, its model, its scripts.
///
/// Shared with the prefab pass. A prefab is nodes — the same nodes, written by
/// the same clipboard — so a prefab whose material points at a deleted texture
/// is the same defect as a scene's, and it was going unreported because only
/// the parent indices were being read.
fn check_node_refs(
    root: &Path,
    node: &floptle_scene::NodeDoc,
    where_: &str,
    settings: &Settings,
    r: &mut Report,
) {
    let who = if node.name.is_empty() { "an unnamed node" } else { &node.name };
    // A Sprite node keeps its own cell and ignores the material's, so ask about
    // the one that draws — the same rule `floptle_script::effective_cell` reads
    // by, one layer down where there is no World to ask.
    let cell = |m: &floptle_scene::MaterialDoc| match node.matter {
        floptle_scene::MatterDoc::Sprite { cell, .. } => cell,
        _ => m.cell,
    };
    if let Some(m) = &node.material {
        check_texture(root, m, who, where_, r);
        check_sheet_grid(root, m, cell(m), who, where_, settings, r);
    }
    for m in node.object_materials.values() {
        check_texture(root, m, who, where_, r);
        check_sheet_grid(root, m, cell(m), who, where_, settings, r);
    }
    note_node_shaders(node, who, where_, r);
    if let floptle_scene::MatterDoc::Mesh { asset_path } = &node.matter
        && !asset_path.is_empty()
    {
        if !exists(root, asset_path) {
            r.error(Some(where_.into()), format!("{who}: no model at {asset_path}"));
        } else if !asset_path.contains("://")
            // Read, not just found: a file the importer refuses (a Draco-compressed
            // .glb) draws nothing, and the scene has a hole with no reason given.
            && let Err(e) = floptle_assets::gltf_import::check_readable(&crate::project::resolve_asset_path(root, asset_path))
        {
            r.error(Some(where_.into()), format!("{who}: model {asset_path} cannot be loaded: {e}"));
        }
    }
    for s in &node.scripts {
        check_script(root, &s.kind, who, where_, r);
    }
}

/// Every shader this node names, with the stage each place needs: a Field
/// Shape's material is a distance field, every other material a surface; a
/// Skybox wants a sky, a UI element a ui face, the PostProcess list post passes.
fn note_node_shaders(node: &floptle_scene::NodeDoc, who: &str, where_: &str, r: &mut Report) {
    use floptle_scene::MatterDoc;
    use floptle_shader::Stage;
    let material_stage =
        if matches!(node.matter, MatterDoc::FieldShape { .. }) { Stage::Sdf } else { Stage::Fragment };
    let mut named: Vec<(&str, Stage)> = Vec::new();
    for m in node.material.iter().chain(node.object_materials.values()) {
        if let Some(sh) = m.shader.as_deref() {
            named.push((sh, material_stage));
        }
    }
    match &node.matter {
        MatterDoc::Skybox { shader: Some(sh), .. } => named.push((sh, Stage::Sky)),
        MatterDoc::PostProcess { screen_shaders, .. } => {
            named.extend(screen_shaders.iter().map(|p| (p.shader.as_str(), Stage::Post)));
        }
        _ => {}
    }
    if let Some(ui) = &node.ui {
        named.push((&ui.shader, Stage::Ui));
    }
    for (path, stage) in named {
        if !path.is_empty() {
            r.shader_refs.push(ShaderRef {
                path: path.to_string(),
                stage,
                who: who.to_string(),
                where_: where_.to_string(),
            });
        }
    }
}

/// **A shader is compiled, not just found.** Through the same path its stage's
/// loader takes, naga included, so a shader that would fall back to the plain
/// look in the game fails here with its own line and column.
fn check_shader_file(path: &Path, where_: &str, r: &mut Report) {
    let src = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return r.error(Some(where_.into()), format!("{e}")),
    };
    let Err(msg) = crate::shaders::compile_any_stage(&src) else { return };
    // One finding per line of the compiler's answer, each at its own place in
    // the file when the line carries one (`12:5: unknown name`).
    for line in msg.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.splitn(3, ':');
        let (l, c, rest) = (parts.next(), parts.next(), parts.next());
        match (l.and_then(|l| l.parse::<u32>().ok()), c.and_then(|c| c.parse::<u32>().ok()), rest) {
            (Some(l), Some(c), Some(rest)) => r.error(Some(format!("{where_}:{l}:{c}")), rest.trim().to_string()),
            _ => r.error(Some(where_.into()), line.to_string()),
        }
    }
}

/// The references, once every file is read: a shader that is not there, and
/// one of the wrong stage for the place that names it. (A material's missing
/// shader is reported with its other files, in `check_texture`.)
fn check_shader_refs(root: &Path, r: &mut Report) {
    use floptle_shader::Stage;
    let name = |s: Stage| match s {
        Stage::Fragment => "surface",
        Stage::Sdf => "sdf",
        Stage::Sky => "sky",
        Stage::Ui => "ui",
        Stage::Post => "post",
    };
    let refs = std::mem::take(&mut r.shader_refs);
    for f in &refs {
        if f.path.contains("://") {
            continue;
        }
        let full = crate::project::resolve_asset_path(root, &f.path);
        let Ok(src) = std::fs::read_to_string(&full) else {
            if !matches!(f.stage, Stage::Fragment | Stage::Sdf) {
                r.error(Some(f.where_.clone()), format!("{}: no shader at {}", f.who, f.path));
            }
            continue;
        };
        // A file that does not parse is reported at the file itself.
        let Ok(ir) = floptle_shader::parse(&src) else { continue };
        let found = ir.stage.unwrap_or(Stage::Fragment);
        if found != f.stage {
            r.error(
                Some(f.where_.clone()),
                format!(
                    "{}: {} is a {} shader, and this place draws a {} one; it falls back to the plain look",
                    f.who,
                    f.path,
                    name(found),
                    name(f.stage)
                ),
            );
        }
    }
}

/// A material file on its own — the ones under `materials/`, which nodes share.
fn check_material_file(
    root: &Path,
    path: &Path,
    where_: &str,
    settings: &Settings,
    r: &mut Report,
) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return r.error(Some(where_.into()), format!("{e}")),
    };
    match ron::from_str::<floptle_scene::MaterialDoc>(&text) {
        Ok(m) => {
            check_texture(root, &m, "this material", where_, r);
            check_sheet_grid(root, &m, m.cell, "this material", where_, settings, r);
            if let Some(sh) = m.shader.as_deref().filter(|s| !s.is_empty()) {
                r.shader_refs.push(ShaderRef {
                    path: sh.to_string(),
                    stage: floptle_shader::Stage::Fragment,
                    who: "this material".into(),
                    where_: where_.to_string(),
                });
            }
        }
        Err(e) => r.error(Some(where_.into()), format!("not a material: {e}")),
    }
}

/// **Every file a material names**, not just its colour map.
///
/// This checked `texture` alone, which meant a surface map pointing at nothing
/// passed — and a missing normal map is exactly the reference a person cannot
/// spot by reading the `.ron`, because the material still loads and the surface
/// still draws, just flat. The same went for a `stage surface` shader and the
/// textures it binds by name.
fn check_texture(
    root: &Path,
    m: &floptle_scene::MaterialDoc,
    who: &str,
    where_: &str,
    r: &mut Report,
) {
    let named = [
        ("texture", m.texture.as_deref()),
        ("normal map", m.normal_map.as_deref()),
        ("roughness map", m.roughness_map.as_deref()),
        ("metallic map", m.metallic_map.as_deref()),
        ("ambient occlusion map", m.ao_map.as_deref()),
    ];
    for (what, path) in named {
        let Some(path) = path.filter(|t| !t.is_empty()) else { continue };
        if !exists(root, path) {
            r.error(Some(where_.into()), format!("{who}: no {what} at {path}"));
        }
    }
    // A shader's own textures, bound by the name the shader declares them under.
    for (slot, path) in &m.shader_textures {
        if !path.is_empty() && !exists(root, path) {
            r.error(Some(where_.into()), format!("{who}: no texture at {path} for `{slot}`"));
        }
    }
    if let Some(sh) = m.shader.as_deref().filter(|s| !s.is_empty())
        && !exists(root, sh)
    {
        r.error(Some(where_.into()), format!("{who}: no shader at {sh}"));
    }
}

/// **Does this material's sheet grid still agree with its texture's?**
///
/// The disagreement the design page named and nothing outside the editor could
/// see. A scene saved before its textures were sliced, or one whose materials
/// were built by a script, carries a grid the project's own import settings
/// contradict — and it draws as a sprite showing its whole sheet instead of one
/// frame, which reads as spritesheets being broken.
///
/// The editor corrects this silently when it opens a project, so the fix is to
/// open and save. That is what the warning says, because it is the whole remedy
/// and it is one action.
///
/// A **warning**, not an error: the project loads, and the editor will put it
/// right. Reported through the same rule the editor applies
/// (`assets::sheet_for`), so a checker cannot come to disagree with the fix.
fn check_sheet_grid(
    root: &Path,
    m: &floptle_scene::MaterialDoc,
    cell: u32,
    who: &str,
    where_: &str,
    settings: &Settings,
    r: &mut Report,
) {
    let Some(tex) = m.texture.as_deref().filter(|t| !t.is_empty()) else { return };
    let Some(setting) = settings.get(&crate::assets::asset_rel_path(tex, root)) else { return };
    let want = crate::assets::sheet_for(*setting, cell);
    let have = (m.sheet_cols.max(1), m.sheet_rows.max(1), cell);
    if want == have {
        return;
    }
    r.warn(
        Some(where_.into()),
        format!(
            "{who}: this says the sheet is {}x{} showing cell {}, and {tex} is sliced {}x{} \
             showing cell {} — open the project in the editor and save it to put this right",
            have.0, have.1, have.2, want.0, want.1, want.2
        ),
    );
}

/// A script's `kind` is its path under `scripts/` without the extension, which
/// is how the editor turns one back into a file.
fn check_script(root: &Path, kind: &str, who: &str, where_: &str, r: &mut Report) {
    if kind.is_empty() || kind.contains("://") {
        // A package supplies its own; this check has no business guessing where.
        return;
    }
    if !root.join("scripts").join(format!("{kind}.lua")).exists() {
        r.error(Some(where_.into()), format!("{who}: no script at scripts/{kind}.lua"));
    }
}

/// Does an asset reference resolve to something on disk? Through the editor's
/// own resolver, so the answer matches what the editor would load.
fn exists(root: &Path, reference: &str) -> bool {
    crate::project::resolve_asset_path(root, reference).exists()
}

fn print_text(r: &Report, root: &Path) {
    // **The same problem on forty nodes is one problem.** A real project hits
    // this immediately: a panel authored hidden and shown by a script warns
    // once per child, and thirty-four identical lines train whoever is reading
    // to skip the whole section — which is where a real finding then hides.
    // The Console merges repeats into a count for the same reason
    // (`ConsoleState::push`); this is that habit, applied to a whole run rather
    // than to consecutive lines. `--json` still carries every one of them,
    // because a program has no trouble reading forty.
    let mut groups: Vec<(Level, Option<&String>, String, &str, usize)> = Vec::new();
    for f in &r.findings {
        let key = crate::console::repeat_shape(&f.message);
        match groups
            .iter_mut()
            .find(|(l, file, k, _, _)| *l == f.level && *file == f.file.as_ref() && *k == key)
        {
            Some(g) => g.4 += 1,
            None => groups.push((f.level, f.file.as_ref(), key, &f.message, 1)),
        }
    }
    for (level, file, _, first, count) in &groups {
        let more = match count {
            1 => String::new(),
            n => format!(" (and {} more like it)", n - 1),
        };
        match file {
            Some(file) => floptle_say::say!("{}: {file}: {first}{more}", level.as_str()),
            None => floptle_say::say!("{}: {first}{more}", level.as_str()),
        }
    }
    let counted = format!(
        "{} scene(s), {} prefab(s), {} effect(s), {} material(s), {} shader(s)",
        r.scenes, r.prefabs, r.effects, r.materials, r.shaders
    );
    if r.examined() == 0 {
        // Said plainly: a checker that looked at nothing and printed nothing is
        // indistinguishable from a clean project, and that is the one way this
        // verb can lie.
        floptle_say::say!("checked nothing in {} — no scenes, prefabs, effects, materials or shaders", root.display());
        return;
    }
    match (r.errors(), r.warnings()) {
        (0, 0) => floptle_say::say!("{counted} — all good"),
        (0, w) => floptle_say::say!("{counted} — {w} warning(s)"),
        (e, 0) => floptle_say::say!("{counted} — {e} error(s)"),
        (e, w) => floptle_say::say!("{counted} — {e} error(s), {w} warning(s)"),
    }
}

fn print_json(r: &Report) {
    let findings: Vec<serde_json::Value> = r
        .findings
        .iter()
        .map(|f| {
            let mut o = serde_json::json!({ "level": f.level.as_str(), "message": f.message });
            if let Some(file) = &f.file {
                o["source"] = serde_json::json!({ "file": file });
            }
            o
        })
        .collect();
    let doc = serde_json::json!({
        "ok": r.errors() == 0,
        "examined": {
            "scenes": r.scenes,
            "prefabs": r.prefabs,
            "effects": r.effects,
            "materials": r.materials,
            "shaders": r.shaders,
        },
        "errors": r.errors(),
        "warnings": r.warnings(),
        "findings": findings,
    });
    floptle_say::say!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "flcheck-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("scenes")).unwrap();
        std::fs::create_dir_all(d.join("scripts")).unwrap();
        std::fs::create_dir_all(d.join("textures")).unwrap();
        std::fs::write(d.join("project.ron"), "(title: Some(\"t\"))").unwrap();
        d
    }

    /// A node with a material, a script and nothing missing.
    fn scene(nodes: &str) -> String {
        format!("(name: \"s\", nodes: [{nodes}])")
    }

    /// **A full path to a file in the project fails the check, naming the node
    /// and field, and `--fix` makes it project-relative.** It loads on the
    /// machine that wrote it, which is exactly why nothing else noticed.
    #[test]
    fn an_absolute_path_fails_the_check_and_fix_makes_it_relative() {
        let d = temp("abs");
        std::fs::create_dir_all(d.join("models")).unwrap();
        // The smallest glTF there is: the check reads a model, so it has to be one.
        std::fs::write(d.join("models/tower.glb"), r#"{"asset":{"version":"2.0"}}"#).unwrap();
        let abs = d.join("models/tower.glb");
        let node = format!(
            "(name: \"Building 7\", transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), \
             scale: (1.0, 1.0, 1.0)), matter: Mesh(asset_path: \"{}\"), scripts: [], id: Some(812))",
            abs.display()
        );
        std::fs::write(d.join("scenes/first.ron"), scene(&node)).unwrap();
        let r = examine(&d);
        let hit = r
            .findings
            .iter()
            .find(|f| f.message.contains("absolute path"))
            .unwrap_or_else(|| panic!("no finding for the absolute path: {:?}", r.findings));
        assert_eq!(hit.level, Level::Error);
        assert!(hit.message.contains("\"Building 7\"") && hit.message.contains("asset_path"), "{}", hit.message);
        assert!(hit.message.contains("\"models/tower.glb\""), "{}", hit.message);
        assert!(hit.file.as_deref().is_some_and(|f| f.starts_with("scenes/first.ron:")), "{:?}", hit.file);

        let fixed = fix_absolute_paths(&d);
        assert_eq!(fixed, vec![("scenes/first.ron".to_string(), 1)]);
        let text = std::fs::read_to_string(d.join("scenes/first.ron")).unwrap();
        assert!(text.contains("asset_path: \"models/tower.glb\""), "{text}");
        let r = examine(&d);
        assert_eq!(r.errors(), 0, "still failing after --fix: {:?}", r.findings);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A terrain palette naming an image by its absolute path.** The palette
    /// is `path|flags` per line, not quoted RON, so it needs its own check and
    /// its own fix, and the fix keeps the flags.
    #[test]
    fn an_absolute_terrain_palette_slot_fails_the_check_and_fix_keeps_its_flags() {
        let d = temp("palette");
        std::fs::create_dir_all(d.join("textures")).unwrap();
        std::fs::create_dir_all(d.join("terrain")).unwrap();
        std::fs::write(d.join("scenes/first.ron"), scene("")).unwrap();
        let abs = d.join("textures/grass.png");
        std::fs::write(&abs, b"png").unwrap();
        std::fs::write(d.join("terrain/first.palette"), format!("{}|glow\ntextures/rock.png", abs.display())).unwrap();
        let r = examine(&d);
        let hit = r
            .findings
            .iter()
            .find(|f| f.message.contains("terrain texture slot 1"))
            .unwrap_or_else(|| panic!("no finding for the palette: {:?}", r.findings));
        assert!(hit.message.contains("\"textures/grass.png\""), "{}", hit.message);
        assert!(!r.findings.iter().any(|f| f.message.contains("slot 2")), "a relative slot was reported");

        let fixed = fix_absolute_paths(&d);
        assert_eq!(fixed, vec![("terrain/first.palette".to_string(), 1)]);
        let text = std::fs::read_to_string(d.join("terrain/first.palette")).unwrap();
        assert_eq!(text, "textures/grass.png|glow\ntextures/rock.png");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A collection the code uses and the project does not declare.**
    /// A ranking warns (it would auto-create keeping the highest value); a
    /// private docs collection does not (it creates itself); a declared one
    /// does not; each collection is said once.
    #[test]
    fn a_cloud_collection_the_scripts_use_and_the_project_does_not_declare_warns() {
        let d = temp("cloud");
        std::fs::write(d.join("scenes/first.ron"), scene("")).unwrap();
        std::fs::write(
            d.join("scripts/online.lua"),
            "local times = cloud.rank('time:play:' .. mode)\n\
             local again = cloud.rank(\"time:other\")\n\
             local saves = cloud.docs('saves', { private = true })\n\
             local stages = cloud.docs(\"stages\")\n\
             local likes = cloud.counter('likes')\n",
        )
        .unwrap();
        std::fs::write(d.join("cloud_collections.ron"), "[(name: \"stages\", kind: docs, access: public)]").unwrap();
        let r = examine(&d);
        let cloud: Vec<&String> = r.findings.iter().map(|f| &f.message).filter(|m| m.contains("cloud.")).collect();
        assert_eq!(cloud.len(), 2, "{cloud:?}");
        assert!(cloud.iter().any(|m| m.contains("line 1") && m.contains("`time`") && m.contains("HIGHEST")), "{cloud:?}");
        assert!(cloud.iter().any(|m| m.contains("`likes`") && m.contains("counter")), "{cloud:?}");
        assert!(r.findings.iter().all(|f| f.level != Level::Error), "a warning, not an error");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The happy path — and, more to the point, that the checker actually
    /// looked at something. A pass that examined nothing reads identically to
    /// a pass that examined everything, which is the one way this verb lies.
    #[test]
    fn a_sound_project_passes_and_says_what_it_read() {
        let d = temp("clean");
        std::fs::write(d.join("scenes/first.ron"), scene("")).unwrap();
        let r = examine(&d);
        assert_eq!(r.errors(), 0, "a clean project reported an error");
        assert_eq!(r.scenes, 1, "the scene was not examined");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **Everything a material names, not just its colour map.**
    ///
    /// `texture` was checked and the four surface maps were not, so a normal map
    /// pointing at a deleted file passed — and that is the reference nobody
    /// spots by reading the `.ron`, because the material still loads and the
    /// surface still draws, only flat.
    #[test]
    fn a_missing_surface_map_or_shader_texture_is_an_error() {
        let d = temp("maps");
        std::fs::write(
            d.join("scenes/first.ron"),
            scene(
                "(name: \"Hero\", material: Some((\
                   normal_map: Some(\"textures/gone-n.png\"), \
                   roughness_map: Some(\"textures/gone-r.png\"), \
                   metallic_map: Some(\"textures/gone-m.png\"), \
                   ao_map: Some(\"textures/gone-ao.png\"), \
                   shader: Some(\"shaders/gone.flsl\"), \
                   shader_textures: {\"noise\": \"textures/gone-x.png\"})))",
            ),
        )
        .unwrap();
        let r = examine(&d);
        assert_eq!(r.errors(), 6, "not every reference was checked: {:?}", r.findings);
        for want in ["gone-n.png", "gone-r.png", "gone-m.png", "gone-ao.png", "gone.flsl", "gone-x.png"] {
            assert!(
                r.findings.iter().any(|f| f.message.contains(want)),
                "{want} went unreported"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A prefab is nodes.** Only its parent indices were being read, so a
    /// prefab whose material points at a deleted texture checked clean — and a
    /// prefab is the thing a project spawns most copies of.
    #[test]
    fn a_prefab_is_checked_the_way_a_scene_is() {
        let d = temp("prefab");
        std::fs::write(
            d.join("scenes/Thing.prefab.ron"),
            "//floptle-nodes-v1\n[(name: \"Hero\", scripts: [(kind: \"nowhere\")])]",
        )
        .unwrap();
        let r = examine(&d);
        assert_eq!(r.prefabs, 1, "the prefab was not examined");
        assert_eq!(r.errors(), 1, "a prefab's own references went unchecked: {:?}", r.findings);
        assert!(r.findings[0].message.contains("nowhere"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **The scene the game starts in.** Nothing else in a project points at it,
    /// so a rename leaves a project that checks clean and opens on nothing.
    #[test]
    fn an_entry_scene_that_is_not_there_is_an_error() {
        let d = temp("entry");
        std::fs::write(d.join("scenes/first.ron"), scene("")).unwrap();
        std::fs::write(
            d.join("project.ron"),
            "(title: Some(\"t\"), entry_scene: Some(\"scenes/renamed.ron\"))",
        )
        .unwrap();
        let r = examine(&d);
        assert_eq!(r.errors(), 1, "{:?}", r.findings);
        assert!(r.findings[0].message.contains("renamed"));

        // …and naming the scene that is there passes, by stem as well as by path.
        for spelling in ["scenes/first.ron", "first"] {
            std::fs::write(
                d.join("project.ron"),
                format!("(title: Some(\"t\"), entry_scene: Some(\"{spelling}\"))"),
            )
            .unwrap();
            assert_eq!(examine(&d).errors(), 0, "entry_scene {spelling:?} was rejected");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A material whose sheet grid disagrees with its texture's.**
    ///
    /// The correction the editor applies silently on open, which meant nothing
    /// outside the editor could see the problem — and it draws as a sprite
    /// showing its whole sheet instead of one frame, which reads as
    /// spritesheets being broken rather than as one number being stale.
    #[test]
    fn a_sheet_grid_that_disagrees_with_its_texture_is_reported() {
        let d = temp("sheet");
        std::fs::create_dir_all(d.join(".floptle")).unwrap();
        std::fs::write(
            d.join(".floptle/textures.ron"),
            "{\"textures/hero.png\": (sheet_cols: 4, sheet_rows: 2)}",
        )
        .unwrap();
        std::fs::write(d.join("textures/hero.png"), []).unwrap();

        // A ▫ Sprite keeps its own cell, so the check has to read that one.
        std::fs::write(
            d.join("scenes/first.ron"),
            scene(
                "(name: \"Hero\", matter: Sprite(cell: 30), \
                 material: Some((texture: Some(\"textures/hero.png\"), sheet_cols: 1, \
                 sheet_rows: 1)))",
            ),
        )
        .unwrap();
        let r = examine(&d);
        assert_eq!(r.errors(), 0, "a stale grid still loads — it is a warning");
        let w: Vec<&str> = r.findings.iter().map(|f| f.message.as_str()).collect();
        assert_eq!(w.len(), 1, "{w:?}");
        // Both halves, because the reader has to know which is stale, and the
        // cell has to be reported clamped into the grid it is moving to.
        assert!(w[0].contains("1x1 showing cell 30"), "{}", w[0]);
        assert!(w[0].contains("4x2 showing cell 7"), "{}", w[0]);

        // …and a grid that agrees says nothing at all.
        std::fs::write(
            d.join("scenes/first.ron"),
            scene(
                "(name: \"Hero\", matter: Sprite(cell: 3), \
                 material: Some((texture: Some(\"textures/hero.png\"), sheet_cols: 4, \
                 sheet_rows: 2)))",
            ),
        )
        .unwrap();
        assert_eq!(examine(&d).findings.len(), 0, "a correct grid was reported anyway");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A scene with a camera and no Skybox is told what its sky will be.**
    /// The loader gives it the default mid-grey one, and a menu drawn over a
    /// flat grey read as a broken render. A camera-less scene is an additive
    /// layer, which keeps the base scene's sky and says nothing.
    #[test]
    fn a_scene_with_a_camera_and_no_skybox_is_warned_about_its_grey_sky() {
        let d = temp("nosky");
        let cam = "(name: \"Camera\", matter: Camera(fov_y: 1.0, active: true))";
        let sky = "(name: \"Sky\", matter: Skybox(color: (0.0, 0.0, 0.0), size: 500.0))";
        std::fs::write(d.join("scenes/first.ron"), scene(cam)).unwrap();
        std::fs::write(d.join("scenes/hud.ron"), scene("(name: \"Hud\", matter: Empty)")).unwrap();
        let r = examine(&d);
        let grey: Vec<_> = r.findings.iter().filter(|f| f.message.contains("no Skybox")).collect();
        assert_eq!(grey.len(), 1, "{:?}", r.findings);
        assert_eq!(grey[0].level, Level::Warning);
        assert!(grey[0].file.as_deref().is_some_and(|f| f.starts_with("scenes/first.ron")), "{:?}", grey[0].file);

        std::fs::write(d.join("scenes/first.ron"), scene(&format!("{cam}, {sky}"))).unwrap();
        assert!(examine(&d).findings.iter().all(|f| !f.message.contains("no Skybox")), "a scene with a sky was warned");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A shader is compiled, not just found.** A broken `.flsl` falls back to
    /// the plain look in the game and nothing else says so; here it is an
    /// error at its own line and column. A shader named by a UI element that
    /// is not there is an error (a game's panels drew as white slabs), and so is
    /// a ui shader worn as a mesh material.
    #[test]
    fn every_shader_is_compiled_and_every_named_one_is_the_right_stage() {
        let d = temp("flsl");
        std::fs::create_dir_all(d.join("shaders")).unwrap();
        std::fs::write(d.join("shaders/good.flsl"), "shader good {\n  stage fragment\n  output color = vec4(1, 0, 0, 1)\n}\n").unwrap();
        std::fs::write(d.join("shaders/face.flsl"), "shader face {\n  stage ui\n  output color = vec4(0, 0, 1, 1)\n}\n").unwrap();
        std::fs::write(d.join("shaders/broken.flsl"), "shader broken {\n  stage fragment\n  output color = vec4(nope, 0, 0, 1)\n}\n").unwrap();
        std::fs::write(
            d.join("scenes/first.ron"),
            scene(
                "(name: \"Wall\", matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), \
                 material: Some((shader: Some(\"shaders/good.flsl\")))), \
                 (name: \"Badge\", matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), \
                 material: Some((shader: Some(\"shaders/face.flsl\")))), \
                 (name: \"Gauge\", matter: Empty, ui: Some((shader: \"shaders/face.flsl\"))), \
                 (name: \"Scanlines\", matter: Empty, ui: Some((shader: \"shaders/ui/ui_scanline.flsl\")))",
            ),
        )
        .unwrap();
        let r = examine(&d);
        assert_eq!(r.shaders, 3, "every .flsl in the project is compiled");
        let msgs: Vec<String> =
            r.findings.iter().map(|f| format!("{}: {}", f.file.as_deref().unwrap_or(""), f.message)).collect();

        let broken: Vec<_> = r.findings.iter().filter(|f| f.file.as_deref().is_some_and(|p| p.starts_with("shaders/broken.flsl"))).collect();
        assert!(!broken.is_empty(), "the broken shader passed: {msgs:#?}");
        assert!(broken.iter().all(|f| f.level == Level::Error));
        assert!(broken[0].file.as_deref() == Some("shaders/broken.flsl:3:23"), "not placed at its line and column: {msgs:#?}");
        assert!(broken[0].message.contains("nope"), "{msgs:#?}");

        assert!(msgs.iter().any(|m| m.contains("Scanlines: no shader at shaders/ui/ui_scanline.flsl")), "{msgs:#?}");
        assert!(msgs.iter().any(|m| m.contains("Badge: shaders/face.flsl is a ui shader")), "{msgs:#?}");
        assert!(!msgs.iter().any(|m| m.contains("Wall") || m.contains("Gauge") || m.contains("good.flsl")), "{msgs:#?}");
        assert_eq!(r.errors(), broken.len() + 2, "{msgs:#?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A model is read, not just found.** A Draco-compressed `.glb` exists,
    /// so it passed, and drew nothing in the game.
    #[test]
    fn a_model_the_importer_refuses_is_an_error_naming_why() {
        let d = temp("draco");
        std::fs::create_dir_all(d.join("models")).unwrap();
        std::fs::write(
            d.join("models/arm.gltf"),
            "{\"asset\":{\"version\":\"2.0\"},\"extensionsUsed\":[\"KHR_draco_mesh_compression\"],\
             \"extensionsRequired\":[\"KHR_draco_mesh_compression\"]}",
        )
        .unwrap();
        std::fs::write(d.join("scenes/first.ron"), scene("(name: \"Arm\", matter: Mesh(asset_path: \"models/arm.gltf\"))")).unwrap();
        let r = examine(&d);
        let hit = r.findings.iter().find(|f| f.message.contains("models/arm.gltf")).expect("the model was not reported");
        assert_eq!(hit.level, Level::Error);
        assert!(hit.message.contains("KHR_draco_mesh_compression"), "{}", hit.message);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A file that does not parse is the whole point of the verb.**
    #[test]
    fn a_scene_that_does_not_parse_is_an_error() {
        let d = temp("broken");
        std::fs::write(d.join("scenes/first.ron"), "(name: \"s\", nodes: [ oh dear").unwrap();
        let r = examine(&d);
        assert_eq!(r.errors(), 1);
        assert_eq!(r.findings[0].file.as_deref(), Some("scenes/first.ron"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A reference to something that is not there.** The class of mistake a
    /// parser cannot catch and the editor only reveals by drawing nothing —
    /// which reads as a material problem, or a camera problem, or anything but
    /// a missing file.
    #[test]
    fn a_missing_texture_model_or_script_is_named_with_its_node() {
        let d = temp("missing");
        std::fs::write(
            d.join("scenes/first.ron"),
            scene(
                "(name: \"Hero\", material: Some((texture: Some(\"textures/gone.png\"))), \
                 scripts: [(kind: \"nowhere\")]),\
                 (name: \"Prop\", matter: Mesh(asset_path: \"models/gone.glb\"))",
            ),
        )
        .unwrap();
        let r = examine(&d);
        let msgs: Vec<&str> = r.findings.iter().map(|f| f.message.as_str()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("Hero") && m.contains("textures/gone.png")),
            "the missing texture was not reported against its node: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("Hero") && m.contains("scripts/nowhere.lua")),
            "the missing script was not reported: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("Prop") && m.contains("models/gone.glb")),
            "the missing model was not reported: {msgs:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// …and a reference that is there is left alone, through the editor's own
    /// resolver rather than a second guess at what a path means. A checker that
    /// cries wolf is one nobody runs twice.
    #[test]
    fn a_reference_that_resolves_is_not_reported() {
        let d = temp("present");
        std::fs::write(d.join("textures/here.png"), [0u8; 4]).unwrap();
        std::fs::write(d.join("scripts/here.lua"), "-- hi").unwrap();
        std::fs::write(
            d.join("scenes/first.ron"),
            scene(
                "(name: \"Hero\", material: Some((texture: Some(\"textures/here.png\"))), \
                 scripts: [(kind: \"here\")])",
            ),
        )
        .unwrap();
        let r = examine(&d);
        assert_eq!(
            r.errors(),
            0,
            "a file that is right there was reported missing: {:?}",
            r.findings.iter().map(|f| &f.message).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A package's scripts are addressed by the package's identity and live
    /// wherever it was installed or linked. Guessing a path for one would
    /// report every package-driven node as broken.
    #[test]
    fn a_package_reference_is_not_guessed_at() {
        let d = temp("pkg");
        std::fs::write(
            d.join("scenes/first.ron"),
            scene("(name: \"Hero\", scripts: [(kind: \"pkg://com.example.kit/thing\")])"),
        )
        .unwrap();
        assert_eq!(examine(&d).errors(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A directory with no project file in it is the mistake somebody makes
    /// first, and it has to say so rather than pass for having found nothing.
    #[test]
    fn a_directory_that_is_not_a_project_says_so() {
        let d = std::env::temp_dir().join(format!("flcheck-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let r = examine(&d);
        assert_eq!(r.errors(), 1);
        assert!(r.findings[0].message.contains("project.ron"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An installed package is somebody else's code. Its problems are not this
    /// project's problems and would be noise nobody running this can act on.
    #[test]
    fn an_installed_package_is_not_walked() {
        let d = temp("skip");
        std::fs::create_dir_all(d.join("packages/com.example.kit/scenes")).unwrap();
        std::fs::write(d.join("packages/com.example.kit/scenes/x.ron"), "not a scene at all")
            .unwrap();
        let r = examine(&d);
        assert_eq!(r.errors(), 0, "a package's file was checked as though it were ours");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **The same problem on forty nodes collapses to one line, and two
    /// different problems do not.**
    ///
    /// A real project walks into this at once — a panel authored hidden and
    /// shown by a script warns once per child — and thirty identical lines
    /// train whoever is reading to skip the section a real finding is in. The
    /// second half is the one that took a correction: blanking every quoted
    /// name merged four different hidden panels into one line that claimed to
    /// be one panel, which removes information rather than repetition.
    #[test]
    fn repeats_collapse_but_different_problems_do_not() {
        let shape = crate::console::repeat_shape;
        let same_shape = [
            r#"UI element "A" sits under "Panel", which is not visible"#,
            r#"UI element "B" sits under "Panel", which is not visible"#,
        ];
        assert_eq!(shape(same_shape[0]), shape(same_shape[1]), "these are one problem");

        let other_panel = r#"UI element "C" sits under "Other", which is not visible"#;
        assert_ne!(
            shape(same_shape[0]),
            shape(other_panel),
            "two different panels are two findings, not one with a count"
        );

        let unquoted = ["Hero: no texture at a.png", "Prop: no texture at a.png"];
        assert_ne!(
            shape(unquoted[0]),
            shape(unquoted[1]),
            "a message with nothing quoted has nothing to collapse on"
        );
    }

    /// **The wiring checks are the editor's own**, run at the levels the
    /// Console runs them at — an out-of-range parent is an error, and it is the
    /// one `report_scene_wiring` exists for.
    #[test]
    fn the_wiring_checks_are_the_ones_the_editor_runs() {
        let d = temp("wiring");
        std::fs::write(
            d.join("scenes/first.ron"),
            scene("(name: \"A\"), (name: \"B\", parent: Some(99))"),
        )
        .unwrap();
        let r = examine(&d);
        assert!(
            r.errors() > 0,
            "a parent index past the end of the list was not reported: {:?}",
            r.findings.iter().map(|f| &f.message).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
