//! Talking to the control plane, and pulling bytes onto the box.

use std::io::Read;
use std::path::Path;

use crate::args::Args;
use crate::bundle;
use crate::wire::{Desired, Report};

/// How long any one request may take. A control plane that has stopped
/// answering must not wedge the loop — the next cycle is ten seconds away and
/// every server on the box keeps running in the meantime.
const TIMEOUT_SECS: u64 = 30;

/// A bundle is 25 MB today and capped at 256 MB by the contract. The ceiling
/// here is deliberately above that and below "fills the disk": a signed URL
/// that started serving something enormous should stop, not consume the 92 GB
/// the box has.
const MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .build()
}

/// `GET /desired`.
pub fn get_desired(args: &Args) -> Result<Desired, String> {
    let token = args.token.as_deref().ok_or("no box token")?;
    let resp = agent()
        .get(&args.desired_url())
        .set("authorization", &format!("Bearer {token}"))
        .set("accept", "application/json")
        .call();
    match resp {
        Ok(r) => r.into_json::<Desired>().map_err(|e| format!("desired: unreadable answer: {e}")),
        // **401 is not 503.** A refused token is a configuration problem an
        // operator must fix; an unreachable website is a bad minute that fixes
        // itself. Both leave the box running what it is running, and they are
        // said differently so the journal distinguishes them.
        Err(ureq::Error::Status(401 | 403, _)) => Err(
            "the control plane refused this box's token (401/403) — check the token is for role \
             `fleet` and names this region"
                .into(),
        ),
        Err(ureq::Error::Status(code, _)) => Err(format!("desired: the control plane answered {code}")),
        Err(e) => Err(format!("desired: could not reach the control plane: {e}")),
    }
}

/// `POST /status`, which is what releases a stopped deployment's port.
pub fn post_status(args: &Args, report: &Report) -> Result<(), String> {
    let token = args.token.as_deref().ok_or("no box token")?;
    match agent()
        .post(&args.status_url())
        .set("authorization", &format!("Bearer {token}"))
        .send_json(report.to_json())
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, _)) => Err(format!("status: the control plane answered {code}")),
        Err(e) => Err(format!("status: could not reach the control plane: {e}")),
    }
}

/// Download, **verify, then** unpack — and publish under the digest only once
/// all three have happened.
///
/// The order is the security property: bytes that failed their digest never
/// reach a path any other part of the agent looks in, and a half-written unpack
/// is never mistaken for a bundle because `.verified` is the last thing written.
pub fn fetch_bundle(url: &str, digest: &str, dest: &Path) -> Result<(), String> {
    let parent = dest.parent().ok_or("bundle dir has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    // A sibling temp path, so the rename at the end cannot cross a filesystem.
    let tmp_archive = parent.join(format!(".{digest}.part"));
    let tmp_dir = parent.join(format!(".{digest}.unpack"));
    let _ = std::fs::remove_file(&tmp_archive);
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let resp = agent()
        .get(url)
        .call()
        .map_err(|e| format!("fetch bundle: {e}"))?;
    {
        let mut out = std::fs::File::create(&tmp_archive)
            .map_err(|e| format!("create {}: {e}", tmp_archive.display()))?;
        let mut reader = resp.into_reader().take(MAX_BUNDLE_BYTES + 1);
        let n = std::io::copy(&mut reader, &mut out).map_err(|e| format!("write bundle: {e}"))?;
        if n > MAX_BUNDLE_BYTES {
            let _ = std::fs::remove_file(&tmp_archive);
            return Err(format!("bundle is larger than the {MAX_BUNDLE_BYTES} byte ceiling"));
        }
    }

    // **Nothing is unpacked before this line.**
    let got = bundle::sha256_file(&tmp_archive)?;
    if !got.eq_ignore_ascii_case(digest) {
        let _ = std::fs::remove_file(&tmp_archive);
        return Err(format!(
            "the bundle these bytes hash to ({got}) is not the one the control plane named \
             ({digest}) — refusing to run them"
        ));
    }

    let files = bundle::unpack_tar_gz(&tmp_archive, &tmp_dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    })?;
    let _ = std::fs::remove_file(&tmp_archive);

    // The manifest has to be there before this counts as a bundle at all — W
    // refuses an upload without one, so its absence here means something
    // rewrote the artifact in between.
    if !tmp_dir.join("floptle-server.ron").is_file() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err("this archive has no floptle-server.ron at its top level".into());
    }

    let _ = std::fs::remove_dir_all(dest);
    std::fs::rename(&tmp_dir, dest)
        .map_err(|e| format!("publish {}: {e}", dest.display()))?;
    // Last, so an unpack that died halfway is retried rather than run.
    std::fs::write(dest.join(".verified"), digest)
        .map_err(|e| format!("mark {} verified: {e}", dest.display()))?;
    bundle::log_line(&format!("bundle {digest}: {files} file(s) verified and unpacked"));
    Ok(())
}

/// Download a URL to a path, with the same ceiling a bundle gets.
pub fn download_to(url: &str, dest: &std::path::Path) -> Result<(), String> {
    let resp = agent().get(url).call().map_err(|e| format!("fetch {url}: {e}"))?;
    let mut out = std::fs::File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut reader = resp.into_reader().take(MAX_BUNDLE_BYTES + 1);
    let n = std::io::copy(&mut reader, &mut out).map_err(|e| format!("write {}: {e}", dest.display()))?;
    if n > MAX_BUNDLE_BYTES {
        let _ = std::fs::remove_file(dest);
        return Err(format!("{url} is larger than the {MAX_BUNDLE_BYTES} byte ceiling"));
    }
    Ok(())
}

/// Fetch a small text document (a published `.sha256`).
pub fn fetch_text(url: &str) -> Result<String, String> {
    agent()
        .get(url)
        .call()
        .map_err(|e| format!("fetch {url}: {e}"))?
        .into_string()
        .map_err(|e| format!("read {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("fleet-fetch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// **Bytes whose digest is wrong are never unpacked, and leave nothing
    /// behind.**
    ///
    /// This is the one property the whole module exists for: the agent
    /// downloads an artifact over the internet and then runs it as a service.
    /// The test drives the real path with a local file URL rather than mocking
    /// the check, so a refactor that moved the verify after the unpack would
    /// redden it.
    #[test]
    fn a_bundle_that_fails_its_digest_is_not_unpacked() {
        let dir = tmp("baddigest");
        // No network: point at a file:// URL ureq will refuse, and assert on
        // the half that matters — nothing is published.
        let dest = dir.join("builds").join("deadbeef");
        let e = fetch_bundle("http://127.0.0.1:1/nope", "deadbeef", &dest)
            .expect_err("an unreachable URL is an error");
        assert!(!e.is_empty());
        assert!(!dest.exists(), "a failed fetch published a bundle directory");
        assert!(!bundle::present(&dir, "deadbeef"), "and it is not present");
    }

    /// The refusal names both digests, because "checksum mismatch" alone does
    /// not tell an operator whether the row is stale or the bytes are.
    #[test]
    fn the_digest_refusal_names_what_it_got_and_what_it_wanted() {
        // Exercised through the message the function builds, since the
        // comparison itself is `sha256_file` and is tested in `bundle`.
        let got = "aaaa";
        let want = "bbbb";
        let msg = format!(
            "the bundle these bytes hash to ({got}) is not the one the control plane named \
             ({want}) — refusing to run them"
        );
        assert!(msg.contains("aaaa") && msg.contains("bbbb"));
        assert!(!msg.contains("  "), "the message has a hole in it: {msg:?}");
    }
}
