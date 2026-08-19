//! `floptle inspect` — **what is in this project?**
//!
//! Every answer the editor can compute in a panel was an answer a script could
//! not get. This is the read half of that: the project, its scenes, and any node
//! in them, as text or as JSON.
//!
//! ## It reads the files, not a running world
//!
//! `docs/cli-proposal.md` leaves this open, and this is the answer for the read
//! verbs: the caller is almost always about to *edit those files*, so the files
//! are the truth it needs. A verb that answered from a loaded world would need
//! the editor, and would report a hierarchy the caller cannot find anything to
//! change in. What a *running* world does differently belongs to `run`, when
//! that exists.
//!
//! ## Three things it does not do by hand
//!
//! **The hierarchy comes from `resolve_parent`.** A node's `parent_id` wins over
//! its positional `parent`, and a reader that took `parent` would print a
//! different tree from the one the engine builds — the exact failure stable ids
//! were added to prevent.
//!
//! **A node type is the name serde writes**, not a table kept here. That name is
//! what appears in the `.ron` the caller is about to edit, and taking it from
//! the serializer means the two cannot drift: a variant renamed in the format is
//! renamed here in the same commit, whether anybody remembered this file or not.
//!
//! **A selected node comes back as its whole document.** Summarising it would
//! decide for the caller which fields matter, and the caller is a program that
//! wants to patch one of them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use floptle_scene::{NodeDoc, SceneDoc};

/// How a `--select` query narrows the nodes.
enum Query {
    /// Case-insensitive substring of the node's name — the default, because it
    /// is what somebody types when they half-remember what a thing is called.
    Name(String),
    Id(u32),
    /// The type name as it is written in the scene file.
    Type(String),
    /// A script `kind` the node carries.
    Script(String),
}

impl Query {
    fn parse(q: &str) -> Query {
        match q.split_once(':') {
            Some(("id", v)) => match v.parse() {
                Ok(n) => Query::Id(n),
                // A malformed id is a name that happens to contain a colon
                // rather than an error — this is a search, and refusing it
                // would be the tool arguing with the question.
                Err(_) => Query::Name(q.to_ascii_lowercase()),
            },
            Some(("type", v)) => Query::Type(v.to_ascii_lowercase()),
            Some(("script", v)) => Query::Script(v.to_ascii_lowercase()),
            _ => Query::Name(q.to_ascii_lowercase()),
        }
    }

    fn matches(&self, n: &NodeDoc) -> bool {
        match self {
            Query::Name(s) => n.name.to_ascii_lowercase().contains(s.as_str()),
            Query::Id(id) => n.id == Some(*id),
            Query::Type(t) => matter_type(&n.matter).to_ascii_lowercase() == *t,
            Query::Script(k) => {
                n.scripts.iter().any(|s| s.kind.to_ascii_lowercase() == *k)
            }
        }
    }
}

/// The name this node's type is written under in the scene file.
///
/// Read out of the serializer rather than matched here, so it is by
/// construction the same word the `.ron` uses — see the module header.
fn matter_type(m: &floptle_scene::MatterDoc) -> String {
    match serde_json::to_value(m) {
        // Struct and tuple variants serialize externally tagged: one key, and
        // the key is the variant.
        Ok(serde_json::Value::Object(o)) => o.keys().next().cloned().unwrap_or_default(),
        // A unit variant is just its name.
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// Which components a node actually carries, in the words the Inspector uses.
fn components(n: &NodeDoc) -> Vec<&'static str> {
    let mut out = Vec::new();
    if n.material.is_some() {
        out.push("material");
    }
    if !n.object_materials.is_empty() {
        out.push("object materials");
    }
    if n.rigidbody.is_some() {
        out.push("rigidbody");
    }
    if n.celestial.is_some() {
        out.push("celestial");
    }
    if n.mesh_collider {
        out.push("mesh collider");
    }
    if !n.scripts.is_empty() {
        out.push("scripts");
    }
    out
}

/// Every scene file under `<root>/scenes`, sorted so two runs agree.
/// **`--scene` means the same thing to every verb that takes it.**
///
/// A path as typed, a path relative to the project, or the stem of a scene
/// anywhere under `scenes/` — case-insensitively, because `--scene Arena` and
/// `--scene arena` are the same request. Shared, because `inspect` and `run`
/// having their own resolvers meant `--scene arena` worked in one and exited 1
/// in the other while both helps said "by path or by name".
pub(crate) fn resolve_scene(root: &Path, s: &str) -> Option<PathBuf> {
    for candidate in [PathBuf::from(s), root.join(s)] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    scene_files(root).into_iter().find(|f| f.file_stem().is_some_and(|st| st.eq_ignore_ascii_case(s)))
}

fn scene_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "ron")
                && !p.to_string_lossy().ends_with(".prefab.ron")
            {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join("scenes"), &mut out);
    out
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Run the verb. Returns the process exit code.
pub(crate) fn run(root: &Path, scene: Option<&str>, select: Option<&str>, json: bool) -> i32 {
    // The same refusal, in the same words, with the same code as every other
    // verb that takes a PROJECT. This used to print "no readable project.ron"
    // as though it were an answer and exit 0 — so a caller pointed at the wrong
    // directory was told the run had succeeded.
    if !root.join("project.ron").is_file() {
        eprintln!("{} is not a project directory (no project.ron)", root.display());
        return 2;
    }

    // Which scenes are in scope: one named, or all of them.
    let files: Vec<PathBuf> = match scene {
        Some(s) => match resolve_scene(root, s) {
            Some(p) => vec![p],
            None => {
                eprintln!("no scene called {s} under {}", root.join("scenes").display());
                return 1;
            }
        },
        None => scene_files(root),
    };

    match select {
        Some(q) => report_selection(root, &files, &Query::parse(q), q, json),
        None if scene.is_some() => report_scenes(root, &files, json),
        None => report_project(root, &files, json),
    }
}

/// One loaded scene, with the parent table it needs.
struct Loaded {
    file: String,
    doc: SceneDoc,
    by_id: HashMap<u32, usize>,
}

fn load_all(root: &Path, files: &[PathBuf]) -> (Vec<Loaded>, Vec<String>) {
    let mut out = Vec::new();
    let mut failed = Vec::new();
    for f in files {
        match floptle_scene::load(f) {
            Ok(doc) => {
                let by_id = floptle_scene::node_id_positions(&doc.nodes);
                out.push(Loaded { file: rel(root, f), doc, by_id });
            }
            // A scene that will not load is `check`'s business; here it is only
            // a reason this listing is incomplete, and saying so beats leaving
            // a caller to wonder why a node it can see in the file is missing.
            Err(e) => failed.push(format!("{}: {e}", rel(root, f))),
        }
    }
    (out, failed)
}

/// `floptle inspect` — the project, at a glance.
fn report_project(root: &Path, files: &[PathBuf], json: bool) -> i32 {
    let cfg = floptle_scene::try_load_project(&root.join("project.ron"));
    let (loaded, failed) = load_all(root, files);

    if json {
        let cfg_json = match &cfg {
            Ok(Some(c)) => serde_json::json!({
                "title": c.title,
                "entryScene": c.entry_scene,
                "engineVersion": c.engine_version,
                "layers": c.layers,
            }),
            _ => serde_json::Value::Null,
        };
        let doc = serde_json::json!({
            "root": root.to_string_lossy(),
            "project": cfg_json,
            "scenes": loaded.iter().map(|l| serde_json::json!({
                "file": l.file,
                "name": l.doc.name,
                "nodes": l.doc.nodes.len(),
            })).collect::<Vec<_>>(),
            "unreadable": failed,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return 0;
    }

    match &cfg {
        Ok(Some(c)) => {
            println!(
                "{} — \"{}\", stamped {}",
                root.display(),
                c.title.as_deref().unwrap_or("(untitled)"),
                c.engine_version.as_deref().unwrap_or("(no version)")
            );
            if let Some(e) = &c.entry_scene {
                println!("  entry scene   {e}");
            }
            if !c.layers.is_empty() {
                println!("  layers        {}", c.layers.join(", "));
            }
        }
        _ => println!("{} — no readable project.ron", root.display()),
    }
    println!("  {} scene(s)", loaded.len());
    for l in &loaded {
        println!("    {:<28}  \"{}\", {} node(s)", l.file, l.doc.name, l.doc.nodes.len());
    }
    for f in &failed {
        println!("  unreadable: {f}");
    }
    0
}

/// `floptle inspect --scene X` — the nodes, as a tree.
fn report_scenes(root: &Path, files: &[PathBuf], json: bool) -> i32 {
    let (loaded, failed) = load_all(root, files);
    if json {
        let scenes: Vec<serde_json::Value> = loaded
            .iter()
            .map(|l| {
                serde_json::json!({
                    "file": l.file,
                    "name": l.doc.name,
                    "nodes": tree_order(l).into_iter()
                        .map(|i| node_summary(l, i, &l.doc.nodes[i]))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let doc = serde_json::json!({ "scenes": scenes, "unreadable": failed });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return i32::from(!failed.is_empty());
    }
    for l in &loaded {
        println!("{} — \"{}\", {} node(s)", l.file, l.doc.name, l.doc.nodes.len());
        for i in tree_order(l) {
            println!("{}", node_line(l, i, &l.doc.nodes[i]));
        }
    }
    for f in &failed {
        eprintln!("unreadable: {f}");
    }
    i32::from(!failed.is_empty())
}

/// `floptle inspect --select Q` — the nodes that match, in full.
fn report_selection(
    root: &Path,
    files: &[PathBuf],
    q: &Query,
    typed: &str,
    json: bool,
) -> i32 {
    let (loaded, _) = load_all(root, files);
    let mut hits: Vec<(&Loaded, usize, &NodeDoc)> = Vec::new();
    for l in &loaded {
        for (i, n) in l.doc.nodes.iter().enumerate() {
            if q.matches(n) {
                hits.push((l, i, n));
            }
        }
    }

    if json {
        // The WHOLE document per hit. Summarising here would decide for the
        // caller which fields matter, and the caller is usually a program about
        // to patch one of them.
        let nodes: Vec<serde_json::Value> = hits
            .iter()
            .map(|(l, i, n)| {
                let mut o = node_summary(l, *i, n);
                o["file"] = serde_json::json!(l.file);
                o["document"] = serde_json::to_value(n).unwrap_or(serde_json::Value::Null);
                o
            })
            .collect();
        let doc = serde_json::json!({ "query": typed, "matched": nodes.len(), "nodes": nodes });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        // Nothing found exits 1, the way `grep` answers — "is there a node
        // called X" is worth being able to ask from a script, and `matched`
        // still says so for a caller reading the document.
        return i32::from(nodes.is_empty());
    }

    if hits.is_empty() {
        println!("nothing matched {typed}");
        return 1;
    }
    for (l, i, n) in &hits {
        println!("{}:{}", l.file, node_line(l, *i, n).trim_start());
    }
    println!("{} node(s) matched {typed}", hits.len());
    0
}

/// The nodes in **tree order**: every node immediately followed by its
/// children, roots in file order.
///
/// Printing in file order and leaning on indentation to convey the hierarchy
/// does not work — a scene that lists five planets and then their five cores
/// shows each core indented under the wrong planet, and the reader has no way
/// to tell. Indentation only means something when a child is adjacent to its
/// parent.
///
/// Every node comes out exactly once, including ones whose parent link is
/// broken or circular: those are emitted at the end rather than dropped,
/// because a listing that silently omits a node is worse than one that shows it
/// in an odd place — the node somebody is hunting for is precisely the one with
/// the broken link.
fn tree_order(l: &Loaded) -> Vec<usize> {
    let n = l.doc.nodes.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for i in 0..n {
        match floptle_scene::resolve_parent(&l.doc.nodes[i], &l.by_id) {
            Some(p) if p < n && p != i => children[p].push(i),
            _ => roots.push(i),
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(i) = stack.pop() {
        if seen[i] {
            continue;
        }
        seen[i] = true;
        out.push(i);
        for &c in children[i].iter().rev() {
            stack.push(c);
        }
    }
    // Anything a cycle kept out of the walk still gets listed.
    out.extend((0..n).filter(|&i| !seen[i]));
    out
}

/// How deep this node sits, walking `resolve_parent` up to a root.
fn depth(l: &Loaded, mut i: usize) -> usize {
    let mut d = 0;
    // Bounded by the node count: a parent cycle in a hand-edited file must not
    // hang the tool that was reached for to find it.
    for _ in 0..l.doc.nodes.len() {
        match floptle_scene::resolve_parent(&l.doc.nodes[i], &l.by_id) {
            Some(p) if p < l.doc.nodes.len() && p != i => {
                d += 1;
                i = p;
            }
            _ => break,
        }
    }
    d
}

fn node_line(l: &Loaded, i: usize, n: &NodeDoc) -> String {
    let indent = "  ".repeat(depth(l, i) + 1);
    let id = n.id.map(|id| format!("#{id}")).unwrap_or_else(|| format!("[{i}]"));
    let extra = components(n);
    let tail = if extra.is_empty() { String::new() } else { format!("  ({})", extra.join(", ")) };
    let name = if n.name.is_empty() { "(unnamed)" } else { &n.name };
    format!("{indent}{id:<6} {name:<28} {}{tail}", matter_type(&n.matter))
}

fn node_summary(l: &Loaded, i: usize, n: &NodeDoc) -> serde_json::Value {
    serde_json::json!({
        "id": n.id,
        "index": i,
        "name": n.name,
        "type": matter_type(&n.matter),
        "parent": floptle_scene::resolve_parent(n, &l.by_id),
        "depth": depth(l, i),
        "components": components(n),
        "scripts": n.scripts.iter().map(|s| s.kind.clone()).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str, scene: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "flinspect-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("scenes")).unwrap();
        std::fs::write(d.join("project.ron"), "(title: Some(\"t\"))").unwrap();
        std::fs::write(d.join("scenes/first.ron"), scene).unwrap();
        d
    }

    fn load(root: &Path) -> Loaded {
        let (mut l, failed) = load_all(root, &scene_files(root));
        assert!(failed.is_empty(), "{failed:?}");
        l.remove(0)
    }

    /// **A type name is the word the scene file uses**, because the caller is
    /// about to edit that file. Taken from the serializer rather than a table
    /// here, so a variant renamed in the format is renamed here too.
    #[test]
    fn a_node_type_is_the_name_the_file_writes() {
        let d = project(
            "types",
            "(name: \"s\", nodes: [(name: \"A\", matter: Sprite(ppu: 32.0)), \
             (name: \"B\", matter: PointLight(color: (1,1,1)))])",
        );
        let l = load(&d);
        assert_eq!(matter_type(&l.doc.nodes[0].matter), "Sprite");
        assert_eq!(matter_type(&l.doc.nodes[1].matter), "PointLight");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **`parent_id` wins over the positional `parent`.**
    ///
    /// This is the whole reason the hierarchy goes through `resolve_parent`: a
    /// reader that took `parent` would print a different tree from the one the
    /// engine builds, and a positional link is exactly what stable ids exist to
    /// stop anybody trusting.
    #[test]
    fn the_tree_is_the_one_the_engine_builds() {
        let d = project(
            "tree",
            "(name: \"s\", nodes: [\
               (name: \"Root\", id: Some(1)), \
               (name: \"Child\", id: Some(2), parent_id: Some(1), parent: Some(0)), \
               (name: \"Grandchild\", id: Some(3), parent_id: Some(2), parent: Some(0))])",
        );
        let l = load(&d);
        assert_eq!(depth(&l, 0), 0);
        assert_eq!(depth(&l, 1), 1);
        assert_eq!(depth(&l, 2), 2, "the positional parent was followed instead of the id");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A child is printed under its own parent**, not merely at the right
    /// depth. A scene that lists five planets and then their five cores put
    /// every core under the wrong planet, and indentation gave no way to tell:
    /// depth without adjacency is a picture of a hierarchy that is not this
    /// one.
    #[test]
    fn the_listing_puts_a_child_under_its_parent() {
        let d = project(
            "order",
            "(name: \"s\", nodes: [\
               (name: \"P1\", id: Some(1)), \
               (name: \"P2\", id: Some(2)), \
               (name: \"C1\", id: Some(3), parent_id: Some(1)), \
               (name: \"C2\", id: Some(4), parent_id: Some(2))])",
        );
        let l = load(&d);
        let order: Vec<&str> =
            tree_order(&l).into_iter().map(|i| l.doc.nodes[i].name.as_str()).collect();
        assert_eq!(order, ["P1", "C1", "P2", "C2"], "file order was printed as though it were a tree");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **Every node is listed exactly once, however broken its link.** The node
    /// somebody is hunting for is the one with the bad parent, so a walk that
    /// quietly dropped it would fail at the one job it was reached for.
    #[test]
    fn a_broken_link_still_lists_every_node() {
        let d = project(
            "orphan",
            "(name: \"s\", nodes: [\
               (name: \"A\", id: Some(1), parent_id: Some(2)), \
               (name: \"B\", id: Some(2), parent_id: Some(1)), \
               (name: \"C\", id: Some(3), parent: Some(99))])",
        );
        let l = load(&d);
        let mut order = tree_order(&l);
        order.sort();
        assert_eq!(order, [0, 1, 2], "a node vanished from the listing");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A hand-edited file can say a node is its own ancestor, and the tool
    /// somebody reached for to FIND that must not hang on it.
    #[test]
    fn a_parent_cycle_does_not_hang() {
        let d = project(
            "cycle",
            "(name: \"s\", nodes: [\
               (name: \"A\", id: Some(1), parent_id: Some(2)), \
               (name: \"B\", id: Some(2), parent_id: Some(1))])",
        );
        let l = load(&d);
        // Any finite answer will do; the assertion is that we get one.
        assert!(depth(&l, 0) <= l.doc.nodes.len());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The query forms, including the one that decides a malformed `id:` is a
    /// name rather than an error — this is a search, and refusing the question
    /// would be worse than answering it literally.
    #[test]
    fn the_query_forms_select_what_they_say() {
        let d = project(
            "select",
            "(name: \"s\", nodes: [\
               (name: \"Hero\", id: Some(7), matter: Sprite(ppu: 32.0), \
                scripts: [(kind: \"player\")]), \
               (name: \"Heroic Statue\", id: Some(8)), \
               (name: \"Lamp\", id: Some(9), matter: PointLight(color: (1,1,1)))])",
        );
        let l = load(&d);
        let matching = |q: &str| -> Vec<&str> {
            let query = Query::parse(q);
            l.doc
                .nodes
                .iter()
                .filter(|n| query.matches(n))
                .map(|n| n.name.as_str())
                .collect()
        };
        assert_eq!(matching("hero"), ["Hero", "Heroic Statue"], "a name is a substring");
        assert_eq!(matching("id:7"), ["Hero"]);
        assert_eq!(matching("type:pointlight"), ["Lamp"], "a type matches whatever the case");
        assert_eq!(matching("script:player"), ["Hero"]);
        assert!(matching("id:not-a-number").is_empty(), "…and it fell back to a name search");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **A selected node comes back whole.** The caller is a program about to
    /// patch one of its fields, and a summary would decide for it which fields
    /// were worth keeping.
    #[test]
    fn a_selection_carries_the_entire_node_document() {
        let d = project(
            "doc",
            "(name: \"s\", nodes: [(name: \"Hero\", id: Some(7), \
              matter: Sprite(ppu: 32.0, size: 2.0, cell: 3))])",
        );
        let l = load(&d);
        let whole = serde_json::to_value(&l.doc.nodes[0]).expect("a node serializes");
        assert_eq!(whole["matter"]["Sprite"]["cell"], 3, "the authored cell survived the round trip");
        assert_eq!(whole["name"], "Hero");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Components are reported from what the node actually carries, so "what is
    /// on this node" does not require reading the file that was just read.
    #[test]
    fn components_are_the_ones_the_node_carries() {
        let d = project(
            "comp",
            "(name: \"s\", nodes: [\
               (name: \"Plain\"), \
               (name: \"Dressed\", material: Some((color: (1,1,1))), scripts: [(kind: \"k\")])])",
        );
        let l = load(&d);
        assert!(components(&l.doc.nodes[0]).is_empty());
        assert_eq!(components(&l.doc.nodes[1]), ["material", "scripts"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
