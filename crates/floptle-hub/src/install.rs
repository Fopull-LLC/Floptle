//! Installing a version: download its artifact, verify the SHA-256, and unpack it into
//! `versions/<version>/`. Progress is streamed over a channel so the UI stays responsive
//! while a worker thread does the work.

use crate::config::Paths;
use crate::releases::Artifact;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// Progress events from an install job.
#[derive(Clone, Debug)]
pub enum Progress {
    Downloading { done: u64, total: u64 },
    Verifying,
    Unpacking,
    Done(PathBuf),
    Failed(String),
}

/// Download → verify → unpack, reporting [`Progress`]. Runs on the calling thread (the UI
/// spawns it on a worker and reads `tx`). `token` auths a private download.
pub fn install(version: &str, artifact: &Artifact, paths: &Paths, token: Option<&str>, tx: &Sender<Progress>) {
    match run(version, artifact, paths, token, tx) {
        Ok(dir) => {
            let _ = tx.send(Progress::Done(dir));
        }
        Err(e) => {
            let _ = tx.send(Progress::Failed(e));
        }
    }
}

fn run(version: &str, artifact: &Artifact, paths: &Paths, token: Option<&str>, tx: &Sender<Progress>) -> Result<PathBuf, String> {
    paths.ensure().map_err(|e| e.to_string())?;
    let fname = artifact.url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("bundle");
    let archive = paths.cache_dir().join(fname);
    download(&artifact.url, token, &archive, artifact.size, tx)?;

    let _ = tx.send(Progress::Verifying);
    verify_sha256(&archive, &artifact.sha256)?;

    let _ = tx.send(Progress::Unpacking);
    let dest = paths.version_dir(version);
    // Unpack into a STAGING dir and require the editor binary before committing, then
    // atomically rename into place. So a corrupt/partial bundle never leaves a half-
    // populated versions/<v>/ that reads as "installed", and a failed re-install/upgrade
    // never destroys the previously working copy.
    let staging = paths.versions_dir().join(format!(".staging-{version}"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let staged = (|| {
        unpack(&archive, &staging)?;
        if !staging.join(crate::registry::editor_bin_name()).is_file() {
            return Err("bundle contains no editor binary".to_string());
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&staging, &dest).map_err(|e| format!("commit install: {e}"))?;
    Ok(dest)
}

fn download(url: &str, token: Option<&str>, dest: &Path, expected_size: u64, tx: &Sender<Progress>) -> Result<(), String> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // A non-http URL is a local file path — the dev / LocalBuilds source ships bundles from
    // disk, so copy instead of fetch (also makes the whole flow testable offline).
    if !url.starts_with("http") {
        let src = Path::new(url);
        let total = std::fs::metadata(src).map(|m| m.len()).unwrap_or(expected_size);
        std::fs::copy(src, dest).map_err(|e| format!("copy {url}: {e}"))?;
        let _ = tx.send(Progress::Downloading { done: total, total });
        return Ok(());
    }
    let mut req = ureq::get(url).set("Accept", "application/octet-stream");
    // Only attach the token to GitHub hosts — never leak it to a manifest-supplied URL
    // that points elsewhere.
    if let Some(t) = token
        && crate::releases::is_github_host(url)
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
        let _ = tx.send(Progress::Downloading { done, total });
    }
    Ok(())
}

/// Stream the file through SHA-256 and compare (case-insensitive hex) to `expected`.
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
    // zip doesn't always preserve the unix exec bit; make sure the editor is runnable.
    #[cfg(unix)]
    set_executable(&dest.join(crate::registry::editor_bin_name()));
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
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
    fn unpack_targz_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("bundle.tar.gz");
        // Build a tar.gz with an editor binary + a data file.
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(gz);
            let mut append = |name: &str, data: &[u8]| {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, name, data).unwrap();
            };
            append(crate::registry::editor_bin_name(), b"#!/bin/sh\necho editor\n");
            append("version.json", br#"{"version":"0.1.0"}"#);
            tar.into_inner().unwrap().finish().unwrap();
        }
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        unpack(&archive, &dest).unwrap();
        assert!(dest.join(crate::registry::editor_bin_name()).is_file());
        assert!(dest.join("version.json").is_file());
        let inst = crate::registry::Install { version: "0.1.0".into(), path: dest };
        assert!(inst.is_valid());
    }

    #[test]
    fn unpack_zip_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("bundle.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
            let opts: zip::write::FileOptions<()> =
                zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(crate::registry::editor_bin_name(), opts).unwrap();
            zip.write_all(b"binary").unwrap();
            zip.start_file("version.json", opts).unwrap();
            zip.write_all(br#"{"version":"0.1.0"}"#).unwrap();
            zip.finish().unwrap();
        }
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        unpack(&archive, &dest).unwrap();
        assert!(dest.join(crate::registry::editor_bin_name()).is_file());
    }

    #[test]
    fn unpack_rejects_unknown_type() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("bundle.rar");
        std::fs::write(&a, b"x").unwrap();
        assert!(unpack(&a, tmp.path()).is_err());
    }

    /// The whole install flow against a LOCAL bundle (the LocalBuilds / dev path): a
    /// local-file artifact URL is copied, checksum-verified, and unpacked into
    /// versions/<v>/ — no network.
    #[test]
    fn install_from_local_bundle_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a tar.gz bundle with an editor binary.
        let bundle = tmp.path().join("floptle-0.1.0-test.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&bundle).unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(gz);
            let data = b"#!/bin/sh\necho editor\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, crate::registry::editor_bin_name(), &data[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let sha = sha256_hex(&std::fs::read(&bundle).unwrap());
        let artifact = crate::releases::Artifact {
            url: bundle.to_string_lossy().into_owned(), // local path, not http
            sha256: sha,
            size: 0,
        };
        let paths = crate::config::Paths::at(&tmp.path().join("hub"));
        let (tx, rx) = std::sync::mpsc::channel();
        install("0.1.0", &artifact, &paths, None, &tx);
        let done = rx.iter().any(|p| matches!(p, Progress::Done(_)));
        assert!(done, "install should report Done");
        let inst = crate::registry::Install {
            version: "0.1.0".into(),
            path: paths.version_dir("0.1.0"),
        };
        assert!(inst.is_valid(), "installed bundle should have the editor binary");
    }

    #[test]
    fn install_rejects_a_bad_checksum() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("b.tar.gz");
        std::fs::write(&bundle, b"not really a tar.gz").unwrap();
        let artifact = crate::releases::Artifact {
            url: bundle.to_string_lossy().into_owned(),
            sha256: "0000".into(),
            size: 0,
        };
        let paths = crate::config::Paths::at(&tmp.path().join("hub"));
        let (tx, rx) = std::sync::mpsc::channel();
        install("0.1.0", &artifact, &paths, None, &tx);
        assert!(rx.iter().any(|p| matches!(p, Progress::Failed(_))), "bad checksum must fail");
        assert!(!paths.version_dir("0.1.0").exists(), "nothing installed on failure");
    }
}
