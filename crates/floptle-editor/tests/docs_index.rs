//! Every document has to be reachable from an index.
//!
//! Documentation you cannot find is documentation you do not have. Thirty of the
//! forty-one pages under `docs/` were unreachable from `docs/README.md` when this
//! test was written — the UI guides, the animation guide, the web API, half the
//! proposals. None of them were missing; all of them were invisible.
//!
//! So: adding a page means linking it. The test says which one and where.
//!
//! It lives in this crate because the editor is what ships the docs surface
//! (the **§ Docs** tab, and the generated `docs/lua-api.md` beside it). It needs
//! nothing from the crate itself — only the repository on disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root")
}

/// The `.md` files directly inside `dir`, minus its own index.
fn pages(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".md") && n != "README.md")
        .collect()
}

/// A folder's index must link every page in that folder.
fn assert_indexed(rel: &str, index: &str) {
    let dir = repo().join(rel);
    let idx = std::fs::read_to_string(dir.join(index))
        .unwrap_or_else(|e| panic!("read {rel}/{index}: {e}"));
    let orphans: Vec<String> = pages(&dir).into_iter().filter(|p| !idx.contains(p)).collect();
    assert!(
        orphans.is_empty(),
        "{} page(s) under {rel}/ are linked from nowhere in {rel}/{index}, \
         so nobody browsing the docs will ever find them:\n  {}",
        orphans.len(),
        orphans.join("\n  ")
    );
}

#[test]
fn every_doc_is_linked_from_the_index() {
    assert_indexed("docs", "README.md");
}

/// The tutorials index is generated (`learn.rs`), but generated is not the same
/// as correct: a renamed tutorial that left its old page behind would still be
/// sitting in the folder, unreachable.
#[test]
fn every_tutorial_is_linked() {
    assert_indexed("docs/tutorials", "README.md");
}

#[test]
fn every_subsystem_doc_is_linked() {
    assert_indexed("docs/subsystems", "README.md");
}

/// Release notes are indexed by the folder listing and by `scope.json`, not by
/// prose — but a version with no notes at all would ship a blank page to every
/// player who clicks it in the Hub.
#[test]
fn every_release_has_notes_with_a_title() {
    let dir = repo().join("docs/releases");
    let mut bad = Vec::new();
    for name in pages(&dir) {
        let body = std::fs::read_to_string(dir.join(&name)).unwrap_or_default();
        if !body.trim_start().starts_with('#') {
            bad.push(name);
        }
    }
    assert!(bad.is_empty(), "release notes with no heading:\n  {}", bad.join("\n  "));
}

/// Every `.md` under `docs/` is either published to fopull.com or explicitly not.
///
/// `docs/site-map.json` decides which docs the website renders. A file in neither
/// list is the failure this exists to prevent, and it fails in the expensive
/// direction silently: a new guide simply never appears on the site, and nobody
/// finds out from the site, because a missing page looks exactly like a page
/// nobody wrote. Being told which file, at the moment it is added, is the
/// difference.
///
/// Note this asks for a DECISION, not for publication — "internal, because it is
/// a proposal" is a perfectly good answer and the map records the reason.
#[test]
fn every_doc_is_classified_for_the_website() {
    let root = repo().join("docs");
    let map = std::fs::read_to_string(root.join("site-map.json")).expect("docs/site-map.json");
    let mut unclassified: Vec<String> = Vec::new();
    for rel in all_docs(&root) {
        // A quoted whole-path match: bare `contains("physics.md")` would also
        // hit `"subsystems/physics.md"` and call the wrong file classified.
        if map.contains(&format!("\"{rel}\"")) {
            continue;
        }
        // A trailing-slash key covers a whole folder — `docs/releases/` reaches
        // the site through the release manifest rather than page by page.
        if rel
            .rmatch_indices('/')
            .any(|(i, _)| map.contains(&format!("\"{}/\"", &rel[..i])))
        {
            continue;
        }
        unclassified.push(rel);
    }
    unclassified.sort();
    assert!(
        unclassified.is_empty(),
        "{} doc(s) are in neither the published set nor the internal list in \
         docs/site-map.json, so the website silently does not have them:\n  {}",
        unclassified.len(),
        unclassified.join("\n  ")
    );
}

/// …and every page the site map promises has to exist.
///
/// The other half of the same guarantee: renaming a doc without touching the map
/// drops it from the website, and the website has no way to notice — it is handed
/// a list and renders what it gets.
#[test]
fn the_site_map_only_promises_pages_that_exist() {
    let root = repo().join("docs");
    let map = std::fs::read_to_string(root.join("site-map.json")).expect("docs/site-map.json");
    // Path-shaped strings only. The `internal` map's REASONS routinely end in a
    // filename ("a proposal; the shipped guide is animation.md"), and reading
    // those as promises makes this test fail on its own prose.
    let listed: Vec<String> = map
        .split('"')
        .filter(|s| s.ends_with(".md") && !s.contains(char::is_whitespace))
        .map(|s| s.to_owned())
        .collect();
    assert!(listed.len() > 50, "only {} pages parsed out of the site map", listed.len());
    let missing: Vec<&String> = listed.iter().filter(|p| !root.join(p).exists()).collect();
    assert!(
        missing.is_empty(),
        "docs/site-map.json lists {} page(s) that do not exist:\n  {}",
        missing.len(),
        missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n  ")
    );
}

/// Every `.md` under `docs/`, relative to it, with `/` separators.
fn all_docs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "md") {
                out.push(
                    p.strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out
}

/// Relative links between docs must actually resolve.
///
/// A dead link in an index is worse than no index: it reads as "this exists"
/// and then wastes the reader's time proving it doesn't.
#[test]
fn doc_links_resolve() {
    let root = repo().join("docs");
    let mut broken: Vec<String> = Vec::new();
    let mut checked = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "md") {
                continue;
            }
            let body = std::fs::read_to_string(&p).unwrap_or_default();
            for target in markdown_links(&body) {
                // Only local paths: external URLs and in-page anchors aren't
                // this test's business.
                if target.starts_with("http") || target.starts_with('#') || target.is_empty() {
                    continue;
                }
                let target = target.split('#').next().unwrap_or_default();
                if target.is_empty() {
                    continue;
                }
                checked += 1;
                let resolved = p.parent().unwrap_or(&root).join(target);
                if !resolved.exists() {
                    let from = p.strip_prefix(repo()).unwrap_or(&p).display();
                    broken.push(format!("{from} → {target}"));
                }
            }
        }
    }
    // Guard against passing by finding nothing: a link parser that silently
    // matches zero links would report a clean bill of health forever.
    assert!(checked > 100, "only {checked} local links found — the parser is broken");
    broken.sort();
    assert!(
        broken.is_empty(),
        "{} link(s) point at files that don't exist:\n  {}",
        broken.len(),
        broken.join("\n  ")
    );
}

/// A **published** page may only link to another published page.
///
/// `floptle/0102`: 33 links across the docs site pointed at documents the site
/// map holds back. The website renders those as plain text — dropping the link
/// and keeping the words, which is the right call, because a 404 and a bounce
/// into the repo mid-sentence are both worse. What it leaves is a reader being
/// told to "see the roadmap" with no roadmap to see.
///
/// That is a bug in the docs, and it is one that regrows every time somebody
/// links a proposal from a guide — which is a natural thing to do, because the
/// proposal is usually where the reasoning is. So it gets a gate rather than a
/// one-time sweep.
///
/// `doc_links_resolve` above is the other half: that one asks whether the file
/// EXISTS, this one asks whether the reader can reach it.
#[test]
fn published_pages_only_link_to_published_pages() {
    let root = repo().join("docs");
    let map = std::fs::read_to_string(root.join("site-map.json")).expect("docs/site-map.json");
    // The published set is every path-shaped string BEFORE the `internal`
    // object; everything after it is held back. Path-shaped means no
    // whitespace, which is what keeps the internal map's REASONS out — they
    // routinely end in a filename ("a proposal; the shipped guide is
    // animation.md") and reading those as pages would publish them by accident.
    let split = map.find("\"internal\"").expect("site-map.json has an `internal` block");
    let published: Vec<String> = map[..split]
        .split('"')
        .filter(|s| s.ends_with(".md") && !s.contains(char::is_whitespace))
        .map(|s| s.to_owned())
        .collect();
    assert!(published.len() > 50, "only {} published pages parsed", published.len());

    let mut unreachable: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for page in &published {
        let body = std::fs::read_to_string(root.join(page)).unwrap_or_default();
        let dir = std::path::Path::new(page)
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .to_string_lossy()
            .into_owned();
        for target in markdown_links(&body) {
            if target.starts_with("http") || target.starts_with('#') || target.is_empty() {
                continue;
            }
            let target = target.split('#').next().unwrap_or_default();
            if target.is_empty() {
                continue;
            }
            checked += 1;
            // Normalise `../x` by hand: `Path::join` keeps the `..` component,
            // and the published set is spelled without one.
            let mut parts: Vec<&str> = Vec::new();
            for c in dir.split('/').chain(target.split('/')) {
                match c {
                    "" | "." => {}
                    ".." => {
                        parts.pop();
                    }
                    other => parts.push(other),
                }
            }
            let rel = parts.join("/");
            if !published.contains(&rel) {
                unreachable.push(format!("{page} → {target}"));
            }
        }
    }
    assert!(checked > 100, "only {checked} local links found — the parser is broken");
    unreachable.sort();
    assert!(
        unreachable.is_empty(),
        "{} link(s) on PUBLISHED pages point at documents the site map holds back, so \
         the reader is promised something they cannot reach:\n  {}\n\nEither publish the \
         target in docs/site-map.json, point at the shipped guide instead (the `internal` \
         block names one for most of them), or rewrite the sentence so it does not \
         promise a document.",
        unreachable.len(),
        unreachable.join("\n  ")
    );
}

/// The `(target)` of every `[text](target)` in a Markdown body.
///
/// Deliberately skips fenced code blocks — a snippet demonstrating link syntax
/// is not a link, and treating it as one makes the test lie.
fn markdown_links(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let cs: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            if cs[i] == ']' && cs.get(i + 1) == Some(&'(') {
                let mut j = i + 2;
                let mut depth = 1;
                let mut target = String::new();
                while j < cs.len() {
                    match cs[j] {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    target.push(cs[j]);
                    j += 1;
                }
                out.push(target.trim().to_owned());
                i = j;
            }
            i += 1;
        }
    }
    out
}
