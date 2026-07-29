//! Installing a version: download its artifact, verify the SHA-256, and unpack it into
//! `versions/<version>/`. Progress is streamed over a channel so the UI stays responsive
//! while a worker thread does the work.
//!
//! The fetching itself lives in `floptle-dist`, shared with the editor's export templates.

use crate::config::Paths;
use crate::releases::Artifact;
use floptle_dist::{download, set_executable, unpack, verify_sha256};
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
    download(&artifact.url, token, &archive, artifact.size, &mut |done, total| {
        let _ = tx.send(Progress::Downloading { done, total });
    })?;

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
        let bin = staging.join(crate::registry::editor_bin_name());
        if !bin.is_file() {
            return Err("bundle contains no editor binary".to_string());
        }
        // zip doesn't always preserve the unix exec bit; make sure the editor is runnable.
        set_executable(&bin);
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

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A tar.gz bundle holding just the editor binary, like the release pipeline emits.
    fn bundle(at: &std::path::Path) {
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(at).unwrap(),
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

    /// The whole install flow against a LOCAL bundle (the LocalBuilds / dev path): a
    /// local-file artifact URL is copied, checksum-verified, and unpacked into
    /// versions/<v>/ — no network. (Download/verify/unpack themselves are tested in
    /// `floptle-dist`; what is tested HERE is the staging-and-commit around them.)
    #[test]
    fn install_from_local_bundle_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let b = tmp.path().join("floptle-0.1.0-test.tar.gz");
        bundle(&b);
        let artifact = crate::releases::Artifact {
            url: b.to_string_lossy().into_owned(), // local path, not http
            sha256: sha256_hex(&std::fs::read(&b).unwrap()),
            size: 0,
        };
        let paths = crate::config::Paths::at(&tmp.path().join("hub"));
        let (tx, rx) = std::sync::mpsc::channel();
        install("0.1.0", &artifact, &paths, None, &tx);
        assert!(rx.iter().any(|p| matches!(p, Progress::Done(_))), "install should report Done");
        let inst = crate::registry::Install {
            version: "0.1.0".into(),
            path: paths.version_dir("0.1.0"),
        };
        assert!(inst.is_valid(), "installed bundle should have the editor binary");
    }

    #[test]
    fn install_rejects_a_bad_checksum() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("b.tar.gz");
        std::fs::write(&bad, b"not really a tar.gz").unwrap();
        let artifact = crate::releases::Artifact {
            url: bad.to_string_lossy().into_owned(),
            sha256: "0000".into(),
            size: 0,
        };
        let paths = crate::config::Paths::at(&tmp.path().join("hub"));
        let (tx, rx) = std::sync::mpsc::channel();
        install("0.1.0", &artifact, &paths, None, &tx);
        assert!(rx.iter().any(|p| matches!(p, Progress::Failed(_))), "bad checksum must fail");
        assert!(!paths.version_dir("0.1.0").exists(), "nothing installed on failure");
    }

    /// A failed re-install must not destroy the copy that already works — that is
    /// the whole reason unpacking goes through a staging dir.
    #[test]
    fn a_failed_reinstall_leaves_the_working_copy_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::at(&tmp.path().join("hub"));
        let good = tmp.path().join("good.tar.gz");
        bundle(&good);
        let ok = crate::releases::Artifact {
            url: good.to_string_lossy().into_owned(),
            sha256: sha256_hex(&std::fs::read(&good).unwrap()),
            size: 0,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        install("0.1.0", &ok, &paths, None, &tx);
        drop(rx);
        let installed = paths.version_dir("0.1.0").join(crate::registry::editor_bin_name());
        assert!(installed.is_file());

        let junk = tmp.path().join("junk.tar.gz");
        std::fs::write(&junk, b"garbage").unwrap();
        let bad = crate::releases::Artifact {
            url: junk.to_string_lossy().into_owned(),
            sha256: sha256_hex(b"garbage"), // checksum PASSES; the unpack is what fails
            size: 0,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        install("0.1.0", &bad, &paths, None, &tx);
        assert!(rx.iter().any(|p| matches!(p, Progress::Failed(_))));
        assert!(installed.is_file(), "the previously working install survived a failed one");
    }
}
