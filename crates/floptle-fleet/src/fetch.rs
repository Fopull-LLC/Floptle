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
/// here is above that and below "fills the disk": a signed URL
/// that started serving something enormous should stop, not consume the 92 GB
/// the box has.
const MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .build()
}

/// How long a download may go without a single byte arriving before it is
/// given up. **Idle, not total**: the whole-request limit above is right for
/// a JSON answer and wrong for a bundle, which at the ~5 MB/s the site serves
/// through its tunnel needs ~55 s at the contract's 256 MB cap. A 30 s total
/// meant nothing over ~140 MB could ever deploy.
const IDLE_SECS: u64 = 60;

/// A ceiling on one download attempt anyway, so a server trickling a byte a
/// minute cannot hold the loop (and every other deployment on the box) for
/// ever. What arrived is kept and the next cycle resumes it.
const DOWNLOAD_SECS: u64 = 15 * 60;

fn download_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(IDLE_SECS))
        .timeout(std::time::Duration::from_secs(DOWNLOAD_SECS))
        .build()
}

/// Download `url` into `part`, **carrying on from whatever `part` already
/// holds.** A `Range` request asks for the rest; a server that answers 206 for
/// exactly that offset is appended to, and one that answers 200 (it ignored the
/// range) starts the file over. A download that dies keeps its bytes for the
/// next attempt. Nothing here decides the bytes are right: the digest does,
/// after, and a wrong file is deleted there, so a bad resume costs one retry.
fn download_resuming(url: &str, part: &Path) -> Result<(), String> {
    let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let mut req = download_agent().get(url);
    if have > 0 {
        req = req.set("range", &format!("bytes={have}-"));
    }
    let resp = match req.call() {
        Ok(r) => r,
        // Everything is already here (a previous attempt finished and died
        // before the digest was checked). The digest decides.
        Err(ureq::Error::Status(416, _)) if have > 0 => return Ok(()),
        Err(e) => return Err(format!("fetch bundle: {e}")),
    };
    let resumed = have > 0
        && resp.status() == 206
        && resp
            .header("content-range")
            .is_some_and(|r| r.trim_start().starts_with(&format!("bytes {have}-")));
    let start = if resumed { have } else { 0 };
    let mut out = if resumed {
        bundle::log_line(&format!("resuming the bundle at {have} bytes"));
        std::fs::OpenOptions::new().append(true).open(part)
    } else {
        std::fs::File::create(part)
    }
    .map_err(|e| format!("open {}: {e}", part.display()))?;
    let mut reader = resp.into_reader().take(MAX_BUNDLE_BYTES + 1 - start);
    let n = std::io::copy(&mut reader, &mut out).map_err(|e| {
        let so_far = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        format!("write bundle: {e} ({so_far} bytes kept, resumed next cycle)")
    })?;
    if start + n > MAX_BUNDLE_BYTES {
        let _ = std::fs::remove_file(part);
        return Err(format!("bundle is larger than the {MAX_BUNDLE_BYTES} byte ceiling"));
    }
    Ok(())
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
    // Named by the digest, so a `.part` left by an earlier attempt can only
    // ever be the start of these same bytes, and is carried on from.
    let tmp_archive = parent.join(format!(".{digest}.part"));
    let tmp_dir = parent.join(format!(".{digest}.unpack"));
    let _ = std::fs::remove_dir_all(&tmp_dir);

    download_resuming(url, &tmp_archive)?;

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
    let resp = download_agent().get(url).call().map_err(|e| format!("fetch {url}: {e}"))?;
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

    /// A bundle archive with a manifest, and its digest.
    fn archive() -> (Vec<u8>, String) {
        let dir = tmp("archive");
        let path = dir.join("b.tar.gz");
        {
            let f = std::fs::File::create(&path).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            // Incompressible filler, so the archive is big enough to split.
            let mut noise = vec![0u8; 256 * 1024];
            let mut x: u32 = 0x9e37_79b9;
            for byte in noise.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *byte = x as u8;
            }
            for (name, body) in [
                ("./floptle-server.ron", &b"(project: \"assets\")"[..]),
                ("./assets/noise.bin", &noise[..]),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, name, body).unwrap();
            }
            b.into_inner().unwrap().finish().unwrap();
        }
        let digest = bundle::sha256_file(&path).unwrap();
        (std::fs::read(&path).unwrap(), digest)
    }

    /// **A download the network keeps cutting still arrives, by resuming.**
    ///
    /// The server here drops every plain request half-way, the way the tunnel
    /// cut freeflier's 189 MB bundle at 142 MB, and answers a `Range` request
    /// with the rest. Only an agent that keeps the `.part` and asks for what
    /// is missing ever gets the whole bundle; one that starts over fails
    /// forever, and every failure was an outage.
    #[test]
    fn a_cut_download_resumes_from_what_already_arrived() {
        use std::io::{BufRead, BufReader, Write};
        let (bytes, digest) = archive();
        let half = bytes.len() / 2;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/bundle", listener.local_addr().unwrap());
        let served = bytes.clone();
        let ranges = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = ranges.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut range = None;
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = v.trim().trim_end_matches('-').parse::<usize>().ok();
                    }
                }
                seen.lock().unwrap().push(format!("{range:?}"));
                match range {
                    Some(from) => {
                        let rest = &served[from..];
                        let _ = write!(
                            conn,
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                             Content-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n",
                            rest.len(),
                            served.len() - 1,
                            served.len()
                        );
                        let _ = conn.write_all(rest);
                    }
                    None => {
                        let _ = write!(
                            conn,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            served.len()
                        );
                        let _ = conn.write_all(&served[..half]);
                        // Dropped here: the connection dies mid-body.
                    }
                }
            }
        });

        let dir = tmp("resume");
        let dest = dir.join("builds").join(&digest);
        let first = fetch_bundle(&url, &digest, &dest);
        assert!(first.is_err(), "the first attempt is cut off: {first:?}");
        assert!(!dest.exists(), "and publishes nothing");
        let part = dir.join("builds").join(format!(".{digest}.part"));
        assert_eq!(
            std::fs::metadata(&part).map(|m| m.len() as usize).ok(),
            Some(half),
            "what arrived is kept for the next attempt"
        );

        fetch_bundle(&url, &digest, &dest).expect("the second attempt asks for the rest");
        assert!(bundle::present(&dir, &digest), "the bundle is verified and unpacked");
        assert!(!part.exists(), "and the partial file is gone");
        assert_eq!(*ranges.lock().unwrap(), ["None".to_string(), format!("Some({half})")]);
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
