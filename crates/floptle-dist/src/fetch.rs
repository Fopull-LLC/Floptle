//! Getting a bundle onto this machine: fetch the manifest, download an artifact,
//! verify its checksum, unpack it.
//!
//! Deliberately callback-shaped rather than channel-shaped — the Hub streams
//! progress to a worker channel, the editor's export folds it into a status
//! line, and neither wants the other's plumbing.

// Both belong to the two network functions below, which a browser build does
// not compile.
#[cfg(not(target_arch = "wasm32"))]
use crate::manifest::Manifest;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;

#[cfg(not(target_arch = "wasm32"))]
use sha2::{Digest, Sha256};
#[cfg(not(target_arch = "wasm32"))]
use std::io::Read;
use std::path::Path;

/// True when `url`'s host is GitHub — the ONLY place it's safe to attach a private-repo
/// token (so a manifest/artifact URL pointing elsewhere can't exfiltrate it). Covers the
/// asset CDN (`*.githubusercontent.com`) that release downloads redirect to.
pub fn is_github_host(url: &str) -> bool {
    let after = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let authority = after.split('/').next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("").split(':').next().unwrap_or("");
    host == "github.com" || host.ends_with(".github.com") || host.ends_with(".githubusercontent.com")
}

#[cfg(not(target_arch = "wasm32"))]
/// Fetch and parse `releases.json` over HTTPS. A private host needs an auth token
/// (sent as a bearer, GitHub hosts only). A non-http URL is read from disk, which
/// is what makes the whole flow testable offline.
pub fn fetch_manifest(url: &str, token: Option<&str>) -> Result<Manifest, String> {
    if !url.starts_with("http") {
        let text = std::fs::read_to_string(url).map_err(|e| format!("read {url}: {e}"))?;
        return Manifest::parse(&text);
    }
    let mut req = ureq::get(url).set("Accept", "application/octet-stream");
    if let Some(t) = token
        && is_github_host(url)
    {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let text = req
        .call()
        .map_err(|e| format!("fetch manifest: {e}"))?
        .into_string()
        .map_err(|e| format!("read manifest: {e}"))?;
    Manifest::parse(&text)
}

#[cfg(not(target_arch = "wasm32"))]
/// Download `url` to `dest`, reporting `(done, total)` bytes as it goes.
///
/// A non-http URL is a local file path — the dev / LocalBuilds source ships bundles from
/// disk, so copy instead of fetch (also makes the whole flow testable offline).
pub fn download(
    url: &str,
    token: Option<&str>,
    dest: &Path,
    expected_size: u64,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(), String> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if !url.starts_with("http") {
        let src = Path::new(url);
        let total = std::fs::metadata(src).map(|m| m.len()).unwrap_or(expected_size);
        std::fs::copy(src, dest).map_err(|e| format!("copy {url}: {e}"))?;
        on_progress(total, total);
        return Ok(());
    }
    let mut req = ureq::get(url).set("Accept", "application/octet-stream");
    // Only attach the token to GitHub hosts — never leak it to a manifest-supplied URL
    // that points elsewhere.
    if let Some(t) = token
        && is_github_host(url)
    {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = req.call().map_err(|e| format!("download {url}: {e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(expected_size);
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        on_progress(done, total);
    }
    Ok(())
}

/// Stream the file through SHA-256 and compare (case-insensitive hex) to `expected`.
///
/// Not in a browser build: it reads an archive off a disk a page does not have.
#[cfg(not(target_arch = "wasm32"))]
pub fn verify_sha256(file: &Path, expected: &str) -> Result<(), String> {
    let mut f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("checksum mismatch (got {got}, expected {expected})"))
    }
}

/// Unpack a bundle into `dest` by extension: `.zip`, or `.tar.gz` / `.tgz`.
///
/// Callers restore the executable bit themselves via [`set_executable`] — which
/// binary matters is the caller's business, not this crate's.
#[cfg(not(target_arch = "wasm32"))]
pub fn unpack(archive: &Path, dest: &Path) -> Result<(), String> {
    let name = archive.file_name().and_then(|s| s.to_str()).unwrap_or_default();
    if name.ends_with(".zip") {
        let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("open zip: {e}"))?;
        zip.extract(dest).map_err(|e| format!("extract zip: {e}"))?;
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(gz);
        tar.unpack(dest).map_err(|e| format!("extract tar.gz: {e}"))?;
    } else {
        return Err(format!("unknown archive type: {name}"));
    }
    Ok(())
}

/// Restore the executable bit (zip doesn't preserve it, and a CI artifact can
/// lose it in transit). A no-op off unix, so callers need no `cfg`.
pub fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn verify_sha256_matches_and_rejects() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("blob.bin");
        std::fs::write(&f, b"hello floptle").unwrap();
        let good = sha256_hex(b"hello floptle");
        assert!(verify_sha256(&f, &good).is_ok());
        assert!(verify_sha256(&f, "deadbeef").is_err());
        assert!(verify_sha256(&f, &good.to_uppercase()).is_ok(), "hex compare is case-insensitive");
    }

    #[test]
    fn unpack_round_trips_targz_and_zip_and_rejects_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let targz = tmp.path().join("bundle.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&targz).unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(gz);
            let data = b"#!/bin/sh\necho editor\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "floptle", &data[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let dest = tmp.path().join("out-tar");
        unpack(&targz, &dest).unwrap();
        assert!(dest.join("floptle").is_file());

        let zipped = tmp.path().join("bundle.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&zipped).unwrap());
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("floptle.exe", opts).unwrap();
            zip.write_all(b"binary").unwrap();
            zip.finish().unwrap();
        }
        let dest = tmp.path().join("out-zip");
        unpack(&zipped, &dest).unwrap();
        assert!(dest.join("floptle.exe").is_file());

        let bogus = tmp.path().join("bundle.rar");
        std::fs::write(&bogus, b"x").unwrap();
        assert!(unpack(&bogus, tmp.path()).is_err());
    }

    #[test]
    fn a_local_download_copies_and_reports_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        std::fs::write(&src, b"payload").unwrap();
        let dst = tmp.path().join("nested/dst.bin");
        let mut seen = Vec::new();
        download(src.to_str().unwrap(), None, &dst, 0, &mut |d, t| seen.push((d, t))).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
        assert_eq!(seen.last(), Some(&(7, 7)), "a local copy still reports completion");
    }

    #[test]
    fn a_manifest_can_be_read_from_disk_without_a_network() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("releases.json");
        std::fs::write(&p, r#"{ "schema": 1, "versions": [ { "version": "0.1.0" } ] }"#).unwrap();
        let m = fetch_manifest(p.to_str().unwrap(), None).unwrap();
        assert_eq!(m.versions.len(), 1);
        assert!(fetch_manifest(tmp.path().join("nope.json").to_str().unwrap(), None).is_err());
    }

    #[test]
    fn tokens_only_ride_github_hosts() {
        assert!(is_github_host("https://github.com/x/y"));
        assert!(is_github_host("https://objects.githubusercontent.com/z"));
        assert!(!is_github_host("https://evil.example.com/steal"));
        assert!(!is_github_host("https://github.com.evil.example.com/steal"));
    }
}
