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

#[test]
fn every_adr_is_linked() {
    assert_indexed("docs/decisions", "README.md");
}

/// Release notes are indexed by the folder listing and by `scope.json`, not by
/// prose — but a version with no notes at all would ship a blank page to every
/// player who clicks it in the Hub.
#[test]
fn every_release_has_notes_with_a_title() {
    let dir = repo().join("docs/releases");
    let mut bad = Vec::new();
    for name in pages(&dir) {
        if name == "STYLE.md" {
            continue;
        }
        let body = std::fs::read_to_string(dir.join(&name)).unwrap_or_default();
        if !body.trim_start().starts_with('#') {
            bad.push(name);
        }
    }
    assert!(bad.is_empty(), "release notes with no heading:\n  {}", bad.join("\n  "));
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
