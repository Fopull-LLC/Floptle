//! `floptle check` — **does this project still load?**
//!
//! The question anything editing a project by hand or by script needs answered
//! after every edit, and until now the only thing that could answer it was
//! opening the editor. A `.ron` file that parses is not a scene that works: a
//! parent index can point past the end of the list, a material can name a
//! texture that is not there, a node can carry a script with no file behind it.
//! Each of those reaches you later as a symptom somewhere else.
//!
//! **It runs the engine's own checks, not a second opinion.** Scenes load
//! through `floptle_scene::load`, the wiring goes through `validate_parents`
//! and `validate_ui_visibility` at the same two levels
//! `Project::report_scene_wiring` pushes them to the Console at, and assets
//! resolve through `project::resolve_asset_path` — the same rescue chain the
//! editor resolves them with, so a reference that works in the editor is not
//! reported as broken here.
//!
//! **No GPU and no window.** Nothing in this file draws anything; that is the
//! line ADR-0027 asks to be drawn deliberately, and a verb that wanted to
//! render a thumbnail would be on the other side of it.
//!
//! **What it looks at**, since a checker's silence is read as a pass: every
//! scene, prefab, effect and standalone material; every node's material maps
//! (colour, normal, roughness, metallic, ambient occlusion), its surface shader
//! and that shader's own textures, its model and its scripts; every effect
//! track's texture, mesh and trail; and the entry scene `project.ron` names.
//!
//! **What it does not**, said as plainly: animation controllers, audio
//! references, and anything inside an installed package. The first list was
//! shorter than the code's actual coverage in one direction and longer in the
//! other — a material's surface maps went unchecked while the prose implied
//! textures were done — so treat both lists as part of the behaviour.

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
        self.scenes + self.prefabs + self.effects + self.materials
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
/// **Those last two are worth keeping apart**, and they used to share an exit
/// code here while `run`, `shot` and `exec` all answered 2. "your path is wrong"
/// and "your project is broken" are the exact distinction this verb exists to
/// draw, so a caller must not have to read the prose to tell them apart.
pub(crate) fn run(root: &Path, json: bool) -> i32 {
    if !root.join("project.ron").is_file() {
        eprintln!("{} is not a project directory (no project.ron)", root.display());
        return 2;
    }
    let report = examine(root);
    if json {
        print_json(&report);
    } else {
        print_text(&report, root);
    }
    i32::from(report.errors() > 0)
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

    let mut files = Vec::new();
    walk(root, &mut files);

    for path in &files {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let where_ = rel(root, path);
        if name.ends_with(".vfx.ron") {
            r.effects += 1;
            match floptle_scene::load_vfx_effect(path) {
                Ok(doc) => check_effect(root, &doc, &where_, &mut r),
                Err(e) => r.error(Some(where_), format!("{e}")),
            }
        } else if name.ends_with(".prefab.ron") {
            r.prefabs += 1;
            check_prefab(root, path, &where_, &mut r);
        } else if name.ends_with(".ron") && in_dir(root, path, "scenes") {
            r.scenes += 1;
            check_scene(root, path, &where_, &mut r);
        } else if name.ends_with(".ron") && in_dir(root, path, "materials") {
            r.materials += 1;
            check_material_file(root, path, &where_, &mut r);
        }
    }
    r
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
fn check_prefab(root: &Path, path: &Path, where_: &str, r: &mut Report) {
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
                check_node_refs(root, node, where_, r);
            }

        }
        Err(e) => r.error(Some(where_.into()), format!("not a prefab: {e}")),
    }
}

/// One scene: does it parse, is its wiring sound, and is everything it names
/// actually there.
fn check_scene(root: &Path, path: &Path, where_: &str, r: &mut Report) {
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
        check_node_refs(root, node, where_, r);
    }
}

/// Everything one node names: its materials, its model, its scripts.
///
/// Shared with the prefab pass. A prefab is nodes — the same nodes, written by
/// the same clipboard — so a prefab whose material points at a deleted texture
/// is the same defect as a scene's, and it was going unreported because only
/// the parent indices were being read.
fn check_node_refs(root: &Path, node: &floptle_scene::NodeDoc, where_: &str, r: &mut Report) {
    let who = if node.name.is_empty() { "an unnamed node" } else { &node.name };
    if let Some(m) = &node.material {
        check_texture(root, m, who, where_, r);
    }
    for m in node.object_materials.values() {
        check_texture(root, m, who, where_, r);
    }
    if let floptle_scene::MatterDoc::Mesh { asset_path } = &node.matter
        && !asset_path.is_empty()
        && !exists(root, asset_path)
    {
        r.error(Some(where_.into()), format!("{who}: no model at {asset_path}"));
    }
    for s in &node.scripts {
        check_script(root, &s.kind, who, where_, r);
    }
}

/// A material file on its own — the ones under `materials/`, which nodes share.
fn check_material_file(root: &Path, path: &Path, where_: &str, r: &mut Report) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return r.error(Some(where_.into()), format!("{e}")),
    };
    match ron::from_str::<floptle_scene::MaterialDoc>(&text) {
        Ok(m) => check_texture(root, &m, "this material", where_, r),
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
            Some(file) => println!("{}: {file}: {first}{more}", level.as_str()),
            None => println!("{}: {first}{more}", level.as_str()),
        }
    }
    let counted = format!(
        "{} scene(s), {} prefab(s), {} effect(s), {} material(s)",
        r.scenes, r.prefabs, r.effects, r.materials
    );
    if r.examined() == 0 {
        // Said plainly: a checker that looked at nothing and printed nothing is
        // indistinguishable from a clean project, and that is the one way this
        // verb can lie.
        println!("checked nothing in {} — no scenes, prefabs, effects or materials", root.display());
        return;
    }
    match (r.errors(), r.warnings()) {
        (0, 0) => println!("{counted} — all good"),
        (0, w) => println!("{counted} — {w} warning(s)"),
        (e, 0) => println!("{counted} — {e} error(s)"),
        (e, w) => println!("{counted} — {e} error(s), {w} warning(s)"),
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
        },
        "errors": r.errors(),
        "warnings": r.warnings(),
        "findings": findings,
    });
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
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

        // …and naming the scene that IS there passes, by stem as well as by path.
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

    /// …and a reference that IS there is left alone, through the editor's own
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
