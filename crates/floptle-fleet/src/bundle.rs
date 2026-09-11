//! Getting a server bundle onto the box, safely.
//!
//! ## The order is the security property
//!
//! Download to a temp file ⇒ **verify the digest** ⇒ unpack ⇒ atomically publish
//! under the digest. Nothing is unpacked before it is verified and nothing is
//! run before it is unpacked, so bytes that failed their digest never reach a
//! path any other part of the agent looks in.
//!
//! ## Content-addressed, so a redeploy is free
//!
//! A bundle lives at `<root>/builds/<sha256>/`. The same build deployed again —
//! restarted, moved to another port, rolled back to — is already there and
//! nothing is fetched. It also means the signed `build_url`, which expires in
//! sixty minutes, is never something the agent has to store or reason about: if
//! the digest is present the URL is irrelevant, and if it is not, the URL was
//! just fetched.
//!
//! ## The archive is an upload from a developer
//!
//! It is not hostile by assumption, but it is not ours, and it is about to be
//! executed. `tar` will refuse most escapes on its own; this module refuses them
//! itself and says which entry, because "the fleet agent cannot write outside
//! its build directory" is a property this crate should be able to point at
//! rather than inherit from a dependency's defaults.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// Where a verified bundle for `digest` lives.
pub fn dir_for(root: &Path, digest: &str) -> PathBuf {
    root.join("builds").join(digest.to_ascii_lowercase())
}

/// Is this bundle already on the box, verified?
///
/// The marker file rather than the directory: an unpack that died halfway
/// leaves a directory full of a partial project, and treating that as "present"
/// is how a box runs half a game forever. The marker is written last.
pub fn present(root: &Path, digest: &str) -> bool {
    dir_for(root, digest).join(".verified").is_file()
}

/// Hex SHA-256 of a file, streamed.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Where one tar entry may be written, relative to the destination — or why it
/// may not be written at all.
///
/// **Both real bundles carry a `./` prefix on every entry** (`floptle/0199`
/// §2), which is normal tar and which broke W's PHP reader outright. Rust's tar
/// handles it; it is normalised here anyway so that the traversal check below
/// is comparing what it thinks it is comparing.
///
/// Refused: an absolute path, any `..`, and a root or prefix component. Each of
/// those is a way for an archive to name a file outside the directory it is
/// being unpacked into, which for this agent means writing into another
/// deployment's bundle, into the engine store, or into the unit directory it
/// then asks systemd to run.
pub fn safe_entry_path(raw: &Path) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            // `./x` — the prefix both real bundles are written with.
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                return Err(format!("{} climbs out of the bundle with `..`", raw.display()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("{} is an absolute path", raw.display()));
            }
        }
    }
    if out.as_os_str().is_empty() {
        // `./` itself — the directory entry, not a file. Skipped, not refused.
        return Ok(PathBuf::new());
    }
    Ok(out)
}

/// Unpack a verified `.tar.gz` into `dest`, refusing any entry that would land
/// outside it.
///
/// Symlinks and hard links are **skipped**, not followed and not recreated. A
/// server bundle is scripts, scenes and models; it has no reason to contain a
/// link, and a link is the other half of the escape that `safe_entry_path`
/// closes — a symlink `assets/x -> /etc` is a legal relative entry whose
/// *contents*, written afterwards, are not.
pub fn unpack_tar_gz(archive: &Path, dest: &Path) -> Result<u64, String> {
    let f = std::fs::File::open(archive).map_err(|e| format!("open bundle: {e}"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    std::fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut files = 0u64;
    for entry in tar.entries().map_err(|e| format!("read bundle: {e}"))? {
        let mut entry = entry.map_err(|e| format!("read bundle entry: {e}"))?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            // Named rather than silently dropped: a bundle that contains one
            // was built by something we do not understand, and the operator
            // should see that in the journal.
            let p = entry.path().map(|p| p.display().to_string()).unwrap_or_default();
            log_line(&format!("bundle: skipping link entry {p}"));
            continue;
        }
        let raw = entry.path().map_err(|e| format!("bundle entry path: {e}"))?.into_owned();
        let rel = safe_entry_path(&raw)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if kind.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        entry.unpack(&out).map_err(|e| format!("write {}: {e}", out.display()))?;
        files += 1;
    }
    normalise_modes(dest)?;
    std::fs::write(dest.join(READABLE_MARKER), "").map_err(|e| format!("mark {}: {e}", dest.display()))?;
    Ok(files)
}

/// Written beside `.verified` once a bundle's modes have been normalised.
const READABLE_MARKER: &str = ".readable";

/// Normalise a bundle that is already on the box, once.
///
/// A present bundle is never re-unpacked — it is content-addressed and the
/// unpack is what verified it — so a fix that lived only in [`unpack_tar_gz`]
/// would leave every bundle unpacked by an older agent exactly as broken as it
/// was, until somebody redeployed with a new digest. The live Forgery server
/// on `us-east-1` was one of those. So the agent does it here on the first
/// cycle after an upgrade, and the marker keeps it from walking 500 files
/// every ten seconds forever.
pub fn ensure_readable(dir: &Path) -> Result<bool, String> {
    if dir.join(READABLE_MARKER).is_file() {
        return Ok(false);
    }
    normalise_modes(dir)?;
    std::fs::write(dir.join(READABLE_MARKER), "").map_err(|e| format!("mark {}: {e}", dir.display()))?;
    Ok(true)
}

/// Make everything under `dir` readable by a user other than the one that
/// unpacked it: files `0644` (`0755` where an exec bit was set), directories
/// `0755`.
///
/// **The archive's own mode bits are an accident of the machine it was made
/// on**, not a statement about the box. `tar` carries a developer's umask
/// faithfully, and a `0600` file that was fine on their laptop is unreadable
/// to the server, which runs as a different user. `floptle/0200`, defect two,
/// and it was live: the Forgery server on `us-east-1` ran without its input
/// bindings and four of its scripts, half-working, and nothing reported it.
/// Equivalent to `chmod -R a+rX`, which is also what the upload endpoint tells
/// a developer to run — the agent does it here so nobody has to.
fn normalise_modes(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let rd = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for e in rd {
            let e = e.map_err(|e| format!("read {}: {e}", dir.display()))?;
            let p = e.path();
            let meta = std::fs::symlink_metadata(&p).map_err(|e| format!("stat {}: {e}", p.display()))?;
            if meta.file_type().is_symlink() {
                continue;
            }
            // A directory needs its search bit; a file keeps an exec bit it had.
            let executable = meta.is_dir() || meta.permissions().mode() & 0o111 != 0;
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode))
                .map_err(|e| format!("chmod {}: {e}", p.display()))?;
            if meta.is_dir() {
                normalise_modes(&p)?;
            }
        }
        // The root itself: a bundle whose top-level directory entry was `0700`
        // would hide the manifest.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", dir.display()))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Print one line to stdout for the journal.
///
/// The fleet agent logs to stdout and nowhere else — `floptle/0197` asks for no
/// file logging on the box, because the journal is what gets shipped to the
/// control plane and a second copy would be a second thing to rotate.
pub fn log_line(msg: &str) {
    floptle_say::say!("{msg}");
}

/// The bundle's own manifest, as much of it as the agent needs.
///
/// It is read back **after unpacking** rather than trusted from `/desired`,
/// because the manifest inside the artifact is what the developer actually
/// shipped. Where they disagree the bundle wins for `project` and `scene`: the
/// control plane read those out of the same file at upload time, so a
/// disagreement means the row is stale, and running the row would start a
/// server against a scene the build does not contain.
#[derive(Debug, Default, PartialEq)]
pub struct Manifest {
    pub project: Option<String>,
    pub scene: Option<String>,
    pub engine_version: Option<String>,
}

/// Read `floptle-server.ron` out of an unpacked bundle.
///
/// A deliberately small reader rather than a RON dependency: this file has five
/// string fields written by one function in `export.rs`, and the agent needs
/// three of them. Pulling in a parser to read it would be the larger risk, not
/// the smaller one.
///
/// **It must be at the top level** — W keys the whole "is this a server bundle"
/// decision on that, and refuses an upload without it, so a bundle that reaches
/// this box has one.
pub fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join("floptle-server.ron");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("this bundle has no floptle-server.ron at its top level: {e}"))?;
    let field = |name: &str| -> Option<String> {
        let at = text.find(&format!("{name}:"))?;
        let rest = &text[at + name.len() + 1..];
        let open = rest.find('"')?;
        let close = rest[open + 1..].find('"')?;
        Some(rest[open + 1..open + 1 + close].to_string())
    };
    Ok(Manifest {
        project: field("project"),
        scene: field("scene"),
        engine_version: field("engine_version"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("floptle-fleet-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// One raw 512-byte tar header plus its data, written by hand.
    ///
    /// `tar::Builder` refuses to WRITE a `..` path — which is a good default and
    /// exactly why it cannot be used to build the fixture: a hostile archive is
    /// not produced by a well-behaved writer, it is produced by whatever the
    /// attacker likes, so the bytes are laid out here directly.
    fn raw_entry(name: &str, body: &[u8]) -> Vec<u8> {
        let mut h = [0u8; 512];
        let n = name.as_bytes();
        h[..n.len()].copy_from_slice(n);
        h[100..107].copy_from_slice(b"0000644"); // mode
        h[108..115].copy_from_slice(b"0000000"); // uid
        h[116..123].copy_from_slice(b"0000000"); // gid
        let size = format!("{:011o}", body.len());
        h[124..135].copy_from_slice(size.as_bytes());
        h[136..147].copy_from_slice(b"00000000000"); // mtime
        h[156] = b'0'; // regular file
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        // The checksum is computed with its own field read as spaces.
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| *b as u32).sum();
        let chk = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(chk.as_bytes());

        let mut out = h.to_vec();
        out.extend_from_slice(body);
        // Data is padded to a 512-byte boundary.
        let pad = (512 - body.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        out
    }

    /// **The `./` prefix every real bundle carries is normal, and everything
    /// that climbs out is refused.**
    ///
    /// The prefix is the half that must be ACCEPTED — it is what both artifacts
    /// Ty produced actually look like, and a guard that rejected it would
    /// refuse every real bundle. The escapes are the half that must be
    /// REFUSED, and they are refused by name so the journal says which entry.
    #[test]
    fn a_dot_slash_prefix_is_fine_and_everything_that_escapes_is_not() {
        // What a real bundle is written with.
        assert_eq!(safe_entry_path(Path::new("./assets/scenes/lobby.ron")).unwrap(),
                   PathBuf::from("assets/scenes/lobby.ron"));
        assert_eq!(safe_entry_path(Path::new("./floptle-server.ron")).unwrap(),
                   PathBuf::from("floptle-server.ron"));
        assert_eq!(safe_entry_path(Path::new("assets/x.lua")).unwrap(),
                   PathBuf::from("assets/x.lua"));
        // The bare directory entry is a skip, not an error.
        assert_eq!(safe_entry_path(Path::new("./")).unwrap(), PathBuf::new());

        // Every shape of escape.
        for bad in [
            "../etc/passwd",
            "./../../etc/passwd",
            "assets/../../../root/.ssh/authorized_keys",
            "/etc/systemd/system/evil.service",
            "./assets/../..",
        ] {
            let e = safe_entry_path(Path::new(bad)).expect_err("{bad} must be refused");
            assert!(e.contains(bad) || e.contains("absolute"), "the entry is named: {e}");
        }
    }

    /// **A bundle cannot write outside its own directory**, end to end through
    /// the real tar reader rather than through the path helper alone.
    ///
    /// The helper could be perfect and the unpack could still call it on the
    /// wrong string, so this builds an archive containing a traversal entry and
    /// asserts the file does not appear where it aimed.
    #[test]
    fn an_archive_that_aims_outside_the_destination_is_refused() {
        let root = tmp("escape");
        let outside = root.join("OUTSIDE");
        std::fs::create_dir_all(&outside).unwrap();
        let archive = root.join("evil.tar.gz");
        {
            let mut raw = Vec::new();
            raw.extend(raw_entry("./assets/fine.txt", b"ok"));
            raw.extend(raw_entry("../OUTSIDE/pwned.txt", b"pwned"));
            raw.extend(raw_entry("/etc/floptle/pwned.txt", b"pwned"));
            raw.extend(std::iter::repeat_n(0u8, 1024)); // the two end-of-archive blocks
            let f = std::fs::File::create(&archive).unwrap();
            let mut gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            std::io::Write::write_all(&mut gz, &raw).unwrap();
            gz.finish().unwrap();
        }
        let dest = root.join("dest");
        let err = unpack_tar_gz(&archive, &dest).expect_err("a traversal entry stops the unpack");
        assert!(err.contains(".."), "the refusal says which entry and why: {err}");
        assert!(
            !outside.join("pwned.txt").exists(),
            "a bundle wrote outside the directory it was unpacked into"
        );
        assert!(!Path::new("/etc/floptle/pwned.txt").exists(), "or at an absolute path");
    }

    /// A plain bundle unpacks, and its manifest reads back.
    #[test]
    fn a_real_shaped_bundle_unpacks_and_its_manifest_reads() {
        let root = tmp("ok");
        let archive = root.join("b.tar.gz");
        let manifest = br#"(
    title: "Forgery",
    project: "assets",
    scene: "scenes/lobby.ron",
    engine_version: "0.85.0-rc6",
    game: "forgery",
)
"#;
        {
            let f = std::fs::File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            for (name, body) in [
                ("./floptle-server.ron", &manifest[..]),
                ("./assets/scenes/lobby.ron", b"(nodes: [])"),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, name, body).unwrap();
            }
            b.into_inner().unwrap().finish().unwrap();
        }
        let dest = root.join("dest");
        let n = unpack_tar_gz(&archive, &dest).expect("unpacks");
        assert_eq!(n, 2);
        assert!(dest.join("assets/scenes/lobby.ron").is_file());

        let m = read_manifest(&dest).expect("the manifest reads");
        assert_eq!(m.project.as_deref(), Some("assets"));
        assert_eq!(m.scene.as_deref(), Some("scenes/lobby.ron"));
        assert_eq!(m.engine_version.as_deref(), Some("0.85.0-rc6"));
    }

    /// **The bundle's own mode bits are not trusted.**
    ///
    /// `floptle/0200`, defect two, and it was live: the Forgery server on
    /// `us-east-1` ran without its input bindings and four of its Lua scripts,
    /// because those files were `0600` on the developer's laptop, `tar` carried
    /// the bit, and the server — a different user — could not read them. It
    /// did not fail; it half-worked, and nothing reported it. The modes in an
    /// archive are an accident of the machine it was made on, so the unpack
    /// normalises them: every file readable, every directory listable, an
    /// exec bit kept where one was set.
    #[test]
    fn a_bundle_written_with_a_tight_umask_is_readable_by_the_server() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("modes");
        let archive = root.join("b.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            let mut dir = tar::Header::new_gnu();
            dir.set_entry_type(tar::EntryType::Directory);
            dir.set_size(0);
            dir.set_mode(0o700);
            dir.set_cksum();
            b.append_data(&mut dir, "./assets/scripts", &[][..]).unwrap();
            for (name, body, mode) in [
                ("./floptle-server.ron", &b"(project: \"assets\")"[..], 0o600),
                ("./assets/input.ron", &b"()"[..], 0o600),
                ("./assets/scripts/gun.lua", &b"return 1"[..], 0o600),
                ("./assets/tool.sh", &b"#!/bin/sh"[..], 0o700),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(mode);
                h.set_cksum();
                b.append_data(&mut h, name, body).unwrap();
            }
            b.into_inner().unwrap().finish().unwrap();
        }
        let dest = root.join("dest");
        unpack_tar_gz(&archive, &dest).expect("unpacks");
        let mode = |rel: &str| std::fs::metadata(dest.join(rel)).unwrap().permissions().mode() & 0o777;
        for f in ["floptle-server.ron", "assets/input.ron", "assets/scripts/gun.lua"] {
            assert_eq!(mode(f), 0o644, "{f} must be readable by a user that is not the unpacker");
        }
        assert_eq!(mode("assets/tool.sh"), 0o755, "an exec bit that was set is kept");
        assert_eq!(mode("assets/scripts"), 0o755, "a 0700 directory hides everything in it");
        assert_eq!(mode("assets"), 0o755);
    }

    /// **A bundle unpacked by an older agent is fixed on the next cycle, once.**
    ///
    /// The live case: the bundle is present and verified, so it will never be
    /// unpacked again, and its `0600` files are exactly as unreadable as the
    /// day they arrived. The first cycle after the upgrade walks it; the
    /// second does not.
    #[test]
    fn a_bundle_already_on_the_box_is_made_readable_once() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("present-modes");
        let dir = root.join("aa");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/input.ron"), "()").unwrap();
        std::fs::set_permissions(dir.join("assets/input.ron"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(dir.join("assets"), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.join(".verified"), "").unwrap();

        assert!(ensure_readable(&dir).unwrap(), "the first cycle after the upgrade does the work");
        let mode = |rel: &str| std::fs::metadata(dir.join(rel)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("assets/input.ron"), 0o644);
        assert_eq!(mode("assets"), 0o755);
        assert!(!ensure_readable(&dir).unwrap(), "and never again");

        // …and a freshly unpacked bundle is already marked, so it is not
        // walked a second time either.
        let archive = root.join("b.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.set_size(2);
            h.set_mode(0o600);
            h.set_cksum();
            b.append_data(&mut h, "./floptle-server.ron", &b"()"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let fresh = root.join("fresh");
        unpack_tar_gz(&archive, &fresh).unwrap();
        assert!(!ensure_readable(&fresh).unwrap(), "unpacking normalised it already");
    }

    /// A half-unpacked bundle is not "present". The marker is written last, so
    /// an unpack killed halfway is retried rather than run.
    #[test]
    fn a_bundle_is_only_present_once_it_is_marked() {
        let root = tmp("marker");
        let digest = "abc123";
        assert!(!present(&root, digest));
        let d = dir_for(&root, digest);
        std::fs::create_dir_all(d.join("assets")).unwrap();
        std::fs::write(d.join("assets/half.lua"), "x").unwrap();
        assert!(!present(&root, digest), "a directory full of a partial project is not a bundle");
        std::fs::write(d.join(".verified"), "").unwrap();
        assert!(present(&root, digest));
    }

    /// The digest is a real SHA-256 of the bytes, and the comparison is
    /// case-insensitive because the control plane's hex is lower and nothing
    /// should turn on that.
    #[test]
    fn the_digest_is_of_the_bytes() {
        let root = tmp("digest");
        let f = root.join("x.bin");
        std::fs::write(&f, b"hello").unwrap();
        // The known SHA-256 of "hello".
        let want = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(sha256_file(&f).unwrap(), want);
        assert!(sha256_file(&f).unwrap().eq_ignore_ascii_case(&want.to_uppercase()));
    }
}
