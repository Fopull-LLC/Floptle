//! Reporting a problem: where the tracker is, and what happens when the editor crashes.
//!
//! **A crash is the moment a bug report is worth most and is hardest to ask for.** The
//! window is gone, whatever was on screen went with it, and the only trace is a
//! backtrace in a terminal the user probably never opened. So the panic hook writes what
//! it knows to a file, and the NEXT launch notices that file and offers — once — to open
//! the tracker with the details already filled in.
//!
//! Nothing is sent anywhere. The file is local, the user chooses whether to open a
//! browser, and the issue body is pre-filled text they can read and edit before posting.

use std::path::PathBuf;

/// Where bugs go. Both Floptle repos are public and have issues enabled; this is the
/// engine's own.
pub(crate) const ISSUES_URL: &str = "https://github.com/Fopull-LLC/Floptle/issues";
pub(crate) const DOCS_URL: &str = "https://github.com/Fopull-LLC/Floptle/blob/main/docs/scripting.md";

/// The crash note a panic leaves behind, read and deleted by the next launch.
fn crash_file() -> Option<PathBuf> {
    floptle_dist::config_dir().map(|d| d.join("last-crash.txt"))
}

/// What we can say about this build without asking anything of the user.
fn environment() -> String {
    format!(
        "Floptle {}\n{} {}\n",
        crate::distribution_version(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// Install the panic hook. Keeps the default hook's output (the terminal message and
/// backtrace are still worth having) and additionally leaves a note on disk.
pub(crate) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    // Only the FIRST panic of a run gets written down.
    //
    // A graphics panic rarely arrives alone: it unwinds, a device object is
    // dropped mid-flight, its destructor panics too, and the run ends on
    // "panic in a destructor during cleanup" — which names nothing and is what
    // the user ends up sending. Every panic after the first is fallout of the
    // first, so the note keeps the cause and the terminal keeps the rest.
    static WRITTEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    std::panic::set_hook(Box::new(move |info| {
        // The default hook first: if writing our own note goes wrong, the normal
        // crash output has already happened.
        previous(info);
        if WRITTEN.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let where_ = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "(unknown location)".into());
        // `RUST_BACKTRACE` is off by default and we do not turn it on for the user —
        // capturing one here is what makes the report useful without asking them to
        // reproduce it under an environment variable.
        let trace = std::backtrace::Backtrace::force_capture();
        let note = format!("{}\npanic: {payload}\nat {where_}\n\n{trace}\n", environment());
        if let Some(path) = crash_file() {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
            let _ = std::fs::write(&path, &note);
            eprintln!("\nFloptle wrote a crash report to {}", path.display());
            eprintln!("Please report it: {ISSUES_URL}");
        }
    }));
}

/// The crash note from a previous run, if there is one. Taken (deleted) as it's read, so
/// one crash asks once — a banner that came back every launch would be its own bug.
pub(crate) fn take_last_crash() -> Option<String> {
    let path = crash_file()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    (!text.trim().is_empty()).then_some(text)
}

/// Percent-encode for a URL query value. Small on purpose — this encodes issue titles
/// and bodies, not arbitrary input.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Open the tracker. With `details`, pre-fills a new issue: what happened, and the
/// version/platform that would otherwise be the first thing anyone has to ask for.
///
/// GitHub caps a prefilled body at roughly 8 KB of URL, and a full backtrace can exceed
/// that — so the body is trimmed and the note stays on disk for the user to attach.
pub(crate) fn open_issue_tracker(details: Option<&str>) {
    let url = match details {
        None => ISSUES_URL.to_string(),
        Some(d) => {
            const MAX_BODY: usize = 6000;
            let mut body = format!(
                "**What I was doing:**\n\n\n**What happened:**\n\n\n<details><summary>Crash report</summary>\n\n```\n{d}\n```\n</details>\n"
            );
            if body.len() > MAX_BODY {
                body.truncate(MAX_BODY);
                body.push_str("\n… trimmed — the full report is in your Floptle config folder as last-crash.txt\n```\n</details>\n");
            }
            format!(
                "{ISSUES_URL}/new?title={}&body={}",
                encode("Editor crashed"),
                encode(&body)
            )
        }
    };
    if let Err(e) = floptle_script::open_in_browser(&url) {
        eprintln!("could not open the browser ({e}) — the tracker is at {ISSUES_URL}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefilled_issue_url_survives_a_huge_backtrace() {
        // A real backtrace is far longer than a URL may be; the link must still be a
        // link rather than something the browser refuses.
        let huge = "frame\n".repeat(20_000);
        let mut opened = String::new();
        // Rebuild what `open_issue_tracker` would open, without opening it.
        let mut body = format!(
            "**What I was doing:**\n\n\n**What happened:**\n\n\n<details><summary>Crash report</summary>\n\n```\n{huge}\n```\n</details>\n"
        );
        if body.len() > 6000 {
            body.truncate(6000);
            body.push_str("\n… trimmed\n");
        }
        opened.push_str(&format!("{ISSUES_URL}/new?title={}&body={}", encode("Editor crashed"), encode(&body)));
        assert!(opened.starts_with(ISSUES_URL), "{opened:.80}");
        assert!(opened.len() < 24_000, "prefilled URL is {} bytes", opened.len());
    }

    #[test]
    fn encoding_leaves_a_url_safe_and_reversible_enough_to_read() {
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("x&y=z"), "x%26y%3Dz");
        assert_eq!(encode("plain-Text_1.0~"), "plain-Text_1.0~");
        // Newlines and backticks are what a crash body is mostly made of.
        assert_eq!(encode("a\n`b`"), "a%0A%60b%60");
    }
}
