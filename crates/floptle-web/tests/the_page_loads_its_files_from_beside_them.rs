//! The web page finds its files when it is served from somewhere else.
//!
//! A site can serve a game's page at one address (`/games/freeflier/play`)
//! and its files at another, with a `<base href>` naming where the files are.
//! Safari, and every browser on an iPhone, ignores `<base>` for a relative
//! `import()` and for an inline script's `import.meta.url`. It asked for the
//! engine beside the page, got a 404, and the game never started: "Importing
//! a module script failed". Desktop browsers honour `<base>`, so it worked
//! everywhere anybody tested.
//!
//! The page now resolves every file against `document.baseURI`, which
//! honours `<base>` in every browser. These tests read the page's source and
//! hold it to that.

const PAGES: &[(&str, &str)] = &[
    ("index.html", include_str!("../web/index.html")),
    ("probe.html", include_str!("../web/probe.html")),
];

/// The script part of a page, comments removed, so a comment that quotes
/// the old pattern does not trip the check.
fn code(page: &str) -> String {
    page.lines()
        .map(|l| l.split("// ").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn no_file_is_loaded_relative_to_the_page_address() {
    for (name, page) in PAGES {
        let code = code(page);
        for bad in ["import('", "import(\"", "import.meta.url", "new Worker('", "importScripts("] {
            assert!(
                !code.contains(bad),
                "{name} loads with {bad}…, which Safari resolves against the page's own address \
                 and not its <base>; resolve it against document.baseURI"
            );
        }
        for file in ["game.flpk", "floptle_web_bg.wasm"] {
            assert!(
                !code.contains(&format!("fetch('{file}'")) && !code.contains(&format!("fetch('./{file}'")),
                "{name} fetches {file} by a bare relative path; send it through here()"
            );
        }
    }
}

/// `here` is a `const`, and the page calls `run()` near the top. Declared
/// below that call, the first use throws before anything loads, and the page
/// shows an error instead of the game. That happened once while making this
/// fix.
#[test]
fn the_address_helper_exists_before_the_page_first_uses_it() {
    let (_, page) = PAGES[0];
    let declared = page.find("const here = ").expect("index.html declares here()");
    let started = page.find("run().catch(").expect("index.html starts run()");
    assert!(declared < started, "here() is declared after run() is called, so run() throws on its first line");
    assert!(page.contains("document.baseURI"), "here() resolves against document.baseURI");
    for f in ["here('pkg/floptle_web.js')", "here('pkg/floptle_web_bg.wasm')", "here('game.flpk')"] {
        assert!(page.contains(f), "index.html loads {f}");
    }
}
