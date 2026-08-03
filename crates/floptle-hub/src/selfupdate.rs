//! The Hub updating **itself**: download its own bundle, verify it, and put the new
//! binary where the running one is.
//!
//! Every other update in this app writes to a directory. This one has to replace a file
//! the OS currently has open and executing, which is the only genuinely awkward part.
//!
//! **The trick is that a running executable can be RENAMED even where it cannot be
//! deleted.** Windows holds an image lock that refuses `DeleteFile` on a running binary
//! but permits `MoveFile`; on Unix a rename just repoints the directory entry and the
//! running process keeps its open inode. So the swap is two renames — current → `.old`,
//! new → current — and both platforms take the same path. The leftover `.old` is deleted
//! on the next launch, when nothing has it open.
//!
//! **Same filesystem or nothing.** `rename` cannot cross devices, so the new binary is
//! staged in the directory it will live in, not in the download cache — which on Linux
//! is routinely a different mount from `/opt` or `~/.local/bin`.
//!
//! **It can legitimately fail, and it must say so rather than half-succeed.** A Hub in
//! `/usr/local/bin`, or inside a read-only mount, or on macOS under a quarantine flag,
//! cannot rewrite itself without privileges this app deliberately never asks for. So
//! writability is checked *before* the button is offered, and a swap that fails puts the
//! old binary back.

use crate::config::Paths;
use crate::releases::Artifact;
use floptle_dist::{download, set_executable, unpack, verify_sha256};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

/// The Hub's own binary name inside its bundle.
pub fn hub_bin_name() -> &'static str {
    if cfg!(windows) { "floptle-hub.exe" } else { "floptle-hub" }
}

/// Progress from a self-update job. Mirrors [`crate::install::Progress`] so the UI can
/// render both the same way.
#[derive(Clone, Debug)]
pub enum Progress {
    Downloading { done: u64, total: u64 },
    Verifying,
    Swapping,
    /// The new binary is in place; the caller relaunches and exits.
    Done(PathBuf),
    Failed(String),
}

/// Why a self-update can't be offered here.
#[derive(Clone, Debug, PartialEq)]
pub enum Blocked {
    /// A `cargo run` build has no release binary to replace.
    DevBuild,
    /// The directory holding the binary isn't writable by this user.
    NotWritable(PathBuf),
    /// `current_exe()` failed — nothing sensible to do.
    Unknown(String),
}

impl Blocked {
    /// What to tell the user, in the place they were about to click a button.
    pub fn message(&self) -> String {
        match self {
            Blocked::DevBuild => "this is a dev build — cargo owns the binary".into(),
            Blocked::NotWritable(dir) => format!(
                "can't write to {} — download the new Hub and replace it yourself, \
                 or move the Hub somewhere you own",
                dir.display()
            ),
            Blocked::Unknown(e) => format!("can't locate this binary ({e})"),
        }
    }
}

/// Can this install replace itself? Checked before offering the button, so the offer is
/// never a lie — a failure discovered *after* a 6 MB download is a worse way to learn it.
pub fn can_self_update() -> Result<PathBuf, Blocked> {
    let exe = std::env::current_exe().map_err(|e| Blocked::Unknown(e.to_string()))?;
    if in_a_cargo_target_dir(&exe) {
        return Err(Blocked::DevBuild);
    }
    let dir = exe.parent().ok_or_else(|| Blocked::Unknown("no parent dir".into()))?.to_path_buf();
    // Probe by actually creating a file. A permissions bit says less than the filesystem
    // does — read-only mounts, immutable flags and container overlays all pass a mode
    // check and then fail the write.
    let probe = dir.join(".floptle-hub-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(exe)
        }
        Err(_) => Err(Blocked::NotWritable(dir)),
    }
}

/// Delete the previous binary left behind by an update. Called once at startup, where
/// nothing has it open — the one moment Windows will let it go.
pub fn clean_leftovers() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
}

/// Is this binary one cargo built and owns? Replacing it would be silently undone by the
/// next `cargo build`, and under `cargo run` it is the file currently executing.
///
/// Detected by **cargo's own lock file**, not by directory names. The obvious check —
/// a path component called `target` next to one called `debug` — is wrong the moment
/// `CARGO_TARGET_DIR` points somewhere else, which is exactly how this repo is set up
/// (`~/.cache/floptle-target`), so the guard read "not a dev build" on the one machine
/// most likely to run one. `.cargo-lock` sits in every profile directory cargo writes,
/// and it is there whatever the tree is called.
fn in_a_cargo_target_dir(exe: &Path) -> bool {
    let Some(dir) = exe.parent() else { return false };
    // `<target>/<profile>/exe`, and `<target>/<profile>/deps/exe` for a test binary.
    dir.join(".cargo-lock").exists()
        || dir.parent().is_some_and(|p| p.join(".cargo-lock").exists())
}

fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Download → verify → unpack → swap. Runs on the calling thread; the UI spawns it on a
/// worker and reads `tx`.
pub fn update(artifact: &Artifact, paths: &Paths, token: Option<&str>, tx: &Sender<Progress>) {
    match run(artifact, paths, token, tx) {
        Ok(exe) => {
            let _ = tx.send(Progress::Done(exe));
        }
        Err(e) => {
            let _ = tx.send(Progress::Failed(e));
        }
    }
}

fn run(artifact: &Artifact, paths: &Paths, token: Option<&str>, tx: &Sender<Progress>) -> Result<PathBuf, String> {
    let exe = can_self_update().map_err(|b| b.message())?;
    swap(&exe, artifact, paths, token, tx)
}

/// Everything after "which file am I replacing". Split from [`run`] so a test can point
/// it at a fake binary — `current_exe()` under `cargo test` is the test harness, and a
/// self-update test that swapped *that* would be replacing the thing running it.
fn swap(
    exe: &Path,
    artifact: &Artifact,
    paths: &Paths,
    token: Option<&str>,
    tx: &Sender<Progress>,
) -> Result<PathBuf, String> {
    let dir = exe.parent().ok_or("no parent dir")?.to_path_buf();
    paths.ensure().map_err(|e| e.to_string())?;

    let fname = artifact.url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("hub");
    let archive = paths.cache_dir().join(fname);
    download(&artifact.url, token, &archive, artifact.size, &mut |done, total| {
        let _ = tx.send(Progress::Downloading { done, total });
    })?;

    let _ = tx.send(Progress::Verifying);
    verify_sha256(&archive, &artifact.sha256)?;

    // Staged BESIDE the binary, because `rename` can't cross filesystems and the cache
    // routinely is one.
    let staging = dir.join(".floptle-hub-staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("stage next to the Hub: {e}"))?;
    let staged = (|| {
        unpack(&archive, &staging)?;
        let bin = staging.join(hub_bin_name());
        if !bin.is_file() {
            return Err(format!("the bundle contains no {}", hub_bin_name()));
        }
        set_executable(&bin);
        Ok(bin)
    })();
    let new_bin = match staged {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    let _ = tx.send(Progress::Swapping);
    let old = old_path(exe);
    // A leftover from a previous update would make the first rename fail on Windows.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("move the running Hub aside: {e}"))?;
    if let Err(e) = std::fs::rename(&new_bin, exe) {
        // Put it back. A Hub that deleted itself and failed to land the replacement is
        // the one outcome nobody can recover from inside the app.
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("put the new Hub in place: {e}"));
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(exe.to_path_buf())
}

/// Start the freshly-installed Hub. The caller exits immediately after; the two overlap
/// for a moment, which is fine — the Hub holds no lock on anything.
pub fn relaunch(exe: &Path) -> Result<(), String> {
    std::process::Command::new(exe)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("start the new Hub: {e}"))
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

    /// A tar.gz holding just the Hub binary, like the release pipeline emits.
    fn hub_bundle(at: &Path, body: &[u8]) {
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(at).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, hub_bin_name(), body).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn the_old_binary_is_named_beside_the_new_one() {
        let p = old_path(Path::new("/opt/floptle/floptle-hub"));
        assert_eq!(p, Path::new("/opt/floptle/floptle-hub.old"));
        // Windows: the extension is part of the name, not replaced by it — `with_extension`
        // would have produced `floptle-hub.old` from `floptle-hub.exe` and orphaned the
        // leftover under a name `clean_leftovers` never looks for.
        assert_eq!(
            old_path(Path::new(r"C:\Floptle\floptle-hub.exe")),
            Path::new(r"C:\Floptle\floptle-hub.exe.old")
        );
    }

    /// The swap itself, against a fake "running binary" in a temp dir: the new bytes land
    /// at the same path, and the previous binary survives under `.old` (which is what
    /// makes the failure path recoverable and what `clean_leftovers` sweeps).
    #[test]
    fn a_swap_replaces_the_binary_and_keeps_the_previous_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(hub_bin_name());
        std::fs::write(&exe, b"OLD HUB").unwrap();

        let archive = tmp.path().join("hub.tar.gz");
        hub_bundle(&archive, b"NEW HUB");
        let artifact = crate::releases::Artifact {
            url: archive.to_string_lossy().into_owned(), // local path, no network
            sha256: sha256_hex(&std::fs::read(&archive).unwrap()),
            size: 0,
        };

        let paths = Paths::at(&tmp.path().join("hubdata"));
        let (tx, rx) = std::sync::mpsc::channel();
        // `run` resolves the binary through current_exe(), which in a test is the test
        // harness — so the swap is exercised directly against our fake instead.
        swap(&exe, &artifact, &paths, None, &tx).unwrap();
        drop(tx);
        assert!(rx.iter().any(|p| matches!(p, Progress::Swapping)));

        assert_eq!(std::fs::read(&exe).unwrap(), b"NEW HUB");
        assert_eq!(std::fs::read(old_path(&exe)).unwrap(), b"OLD HUB");
        assert!(!dir.join(".floptle-hub-staging").exists(), "staging is cleaned up");
    }

    /// A bundle that unpacks to the wrong thing must leave the working Hub in place. The
    /// alternative — a Hub that moved itself aside and then failed — is unrecoverable
    /// from inside the app, so this is the single most important case here.
    #[test]
    fn a_bad_bundle_leaves_the_working_hub_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(hub_bin_name());
        std::fs::write(&exe, b"OLD HUB").unwrap();

        // A valid archive whose contents are wrong: the checksum PASSES and the unpack
        // succeeds — it just doesn't contain a Hub.
        let archive = tmp.path().join("hub.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, "README", &b"hi\n"[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let artifact = crate::releases::Artifact {
            url: archive.to_string_lossy().into_owned(),
            sha256: sha256_hex(&std::fs::read(&archive).unwrap()),
            size: 0,
        };
        let paths = Paths::at(&tmp.path().join("hubdata"));
        let (tx, _rx) = std::sync::mpsc::channel();
        let err = swap(&exe, &artifact, &paths, None, &tx).unwrap_err();
        assert!(err.contains(hub_bin_name()), "{err}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD HUB", "the working Hub survived");
        assert!(!old_path(&exe).exists(), "nothing was moved aside");
    }

    #[test]
    fn a_dev_build_is_refused_rather_than_replacing_cargos_binary() {
        // `can_self_update` reads current_exe(), and under `cargo test` that IS a cargo
        // target binary — so this asserts the guard fires exactly where it must.
        //
        // It did not, at first: the guard looked for a path component named `target`, and
        // this workspace sets CARGO_TARGET_DIR to `~/.cache/floptle-target`. The check is
        // cargo's `.cargo-lock` file now, which is there whatever the tree is called.
        assert_eq!(can_self_update(), Err(Blocked::DevBuild));
        assert!(Blocked::DevBuild.message().contains("dev build"));
    }

    #[test]
    fn an_installed_hub_is_not_mistaken_for_a_dev_build() {
        let tmp = tempfile::tempdir().unwrap();
        // An unpacked release bundle: a binary in a plain directory, no cargo lock.
        let installed = tmp.path().join("floptle-hub");
        std::fs::write(&installed, b"").unwrap();
        assert!(!in_a_cargo_target_dir(&installed));

        // A directory named `target` is not by itself cargo's — somebody's install path
        // may well contain one, and refusing to update there would be a bug they could
        // never diagnose.
        let odd = tmp.path().join("target/release");
        std::fs::create_dir_all(&odd).unwrap();
        let there = odd.join("floptle-hub");
        std::fs::write(&there, b"").unwrap();
        assert!(!in_a_cargo_target_dir(&there));

        // With cargo's lock file beside it, it IS cargo's — whatever the tree is called.
        std::fs::write(odd.join(".cargo-lock"), b"").unwrap();
        assert!(in_a_cargo_target_dir(&there));
        // …and one level down too, which is where a test binary lands.
        let deps = odd.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        assert!(in_a_cargo_target_dir(&deps.join("floptle-hub-abc123")));
    }

}
