//! **The certificate a managed relay presents, and how a renewal reaches it**
//! (`floptle/0227`).
//!
//! A relay reached by its region name is verified by every client: it has to
//! present a chain a public CA issued for `us-east.relay.fopull.com`, and
//! that chain expires. certbot renews it on the box on its own schedule —
//! roughly every sixty days, at an hour nobody chose. A relay that read its
//! certificate only at startup would need a restart to present the renewal,
//! and a relay restart ends every lobby on the box (`floptle/0210`). So the
//! files are WATCHED: every [`POLL_INTERVAL`] the relay stats both paths, and
//! when either has changed it loads them again and hands the result to
//! [`floptle_net::RelayServer::set_certificate`], which swaps the chain for
//! new handshakes only. Nobody is dropped; `touch fullchain.pem` forces one.
//!
//! ⚠ **A renewal that cannot be loaded changes nothing.** A torn file, a key
//! that belongs to a different certificate, a path that vanished mid-rotation:
//! each is said ONCE on the relay's log, and the relay goes on presenting the
//! certificate it had. It is retried the next time the files change, which
//! is what a completed write looks like. The alternative — presenting nothing,
//! or refusing every handshake until an operator notices — is the outage this
//! module exists to prevent.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use floptle_net::ServerCertificate;

/// How often the files are stat'd. Two `stat` calls; a renewal reaches new
/// handshakes within this of landing on disk.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// What a file looked like the last time it was read: modification time and
/// size, following symlinks — certbot's `live/` entries ARE symlinks, and a
/// renewal is the link moving to a new file in `archive/`. `None` is a path
/// that could not be stat'd.
type Stamp = Option<(SystemTime, u64)>;

/// The pair of stamps a certificate was loaded from.
type Stamps = (Stamp, Stamp);

/// The watch over a certificate file and its key.
#[derive(Debug)]
pub struct CertWatch {
    cert_path: PathBuf,
    key_path: PathBuf,
    /// The stamps of the files the relay is presenting now.
    presented: Stamps,
    /// The fingerprint of what is presented — for the log line that says a
    /// failed reload left it in place.
    fingerprint: String,
    /// Stamps that failed to load, so the failure is said once rather than
    /// every ten seconds until someone fixes the file.
    failed: Option<Stamps>,
    last_poll: Instant,
}

/// What a poll found.
#[derive(Debug)]
pub enum Reload {
    /// The files changed and loaded: present this from now on.
    Loaded(ServerCertificate),
    /// The files changed and did NOT load; the relay keeps what it has. Said
    /// once per failing state of the files.
    Failed(String),
}

impl CertWatch {
    /// Load the certificate at startup. A failure here is a relay that must
    /// not come up: it was told to present a certificate and cannot.
    pub fn load(cert_path: &Path, key_path: &Path) -> Result<(Self, ServerCertificate), String> {
        let cert = ServerCertificate::load_pem(cert_path, key_path)?;
        let watch = Self {
            cert_path: cert_path.to_path_buf(),
            key_path: key_path.to_path_buf(),
            presented: (stamp(cert_path), stamp(key_path)),
            fingerprint: cert.fingerprint(),
            failed: None,
            last_poll: Instant::now(),
        };
        Ok((watch, cert))
    }

    /// The leaf's fingerprint, as presented now.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Look at the files if it has been [`POLL_INTERVAL`] since the last look.
    pub fn poll(&mut self) -> Option<Reload> {
        if self.last_poll.elapsed() < POLL_INTERVAL {
            return None;
        }
        self.last_poll = Instant::now();
        self.check()
    }

    /// Look at the files now.
    pub fn check(&mut self) -> Option<Reload> {
        let now = (stamp(&self.cert_path), stamp(&self.key_path));
        if now == self.presented || self.failed == Some(now) {
            return None;
        }
        match ServerCertificate::load_pem(&self.cert_path, &self.key_path) {
            Ok(cert) => {
                self.presented = now;
                self.failed = None;
                self.fingerprint = cert.fingerprint();
                Some(Reload::Loaded(cert))
            }
            Err(e) => {
                self.failed = Some(now);
                Some(Reload::Failed(e))
            }
        }
    }
}

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory holding `fullchain.pem` + `privkey.pem` for `name`.
    struct OnDisk {
        dir: PathBuf,
    }

    impl OnDisk {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("relay-tls-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn cert(&self) -> PathBuf {
            self.dir.join("fullchain.pem")
        }

        fn key(&self) -> PathBuf {
            self.dir.join("privkey.pem")
        }

        /// Write a freshly minted pair and return its fingerprint. The
        /// modification time is pushed forward explicitly, so two writes
        /// inside one filesystem timestamp tick still read as a change — the
        /// way a real renewal, days apart, trivially does.
        fn mint(&self, bump_secs: u64) -> String {
            let cert = rcgen::generate_simple_self_signed(vec!["us-east.relay.fopull.com".into()])
                .unwrap();
            std::fs::write(self.cert(), cert.cert.pem()).unwrap();
            std::fs::write(self.key(), cert.key_pair.serialize_pem()).unwrap();
            self.bump(bump_secs);
            ServerCertificate::load_pem(&self.cert(), &self.key()).unwrap().fingerprint()
        }

        fn bump(&self, secs: u64) {
            let t = SystemTime::now() + Duration::from_secs(secs);
            for p in [self.cert(), self.key()] {
                std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
            }
        }
    }

    impl Drop for OnDisk {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// **A rewritten file is loaded once; a quiet one is not read again.**
    /// The relay stats the files every ten seconds for as long as it runs,
    /// so a check that loaded on every poll would re-parse a key eight
    /// thousand times a day for nothing — and a check that never noticed a
    /// change is the restart-only relay this module replaces.
    #[test]
    fn a_renewal_on_disk_is_picked_up_once_and_an_unchanged_file_is_left_alone() {
        let disk = OnDisk::new("renew");
        let first = disk.mint(0);
        let (mut watch, cert) = CertWatch::load(&disk.cert(), &disk.key()).unwrap();
        assert_eq!(cert.fingerprint(), first);
        assert!(watch.check().is_none(), "nothing changed");
        assert!(watch.check().is_none());

        let second = disk.mint(5);
        assert_ne!(first, second);
        match watch.check() {
            Some(Reload::Loaded(c)) => assert_eq!(c.fingerprint(), second, "the OLD one was loaded"),
            other => panic!("the renewal was not noticed: {other:?}"),
        }
        assert_eq!(watch.fingerprint(), second);
        assert!(watch.check().is_none(), "loaded twice for one change");
    }

    /// **A renewal that cannot be read is said once, and the old certificate
    /// stays.** Then, when the files change again — the write completing —
    /// it is tried again. Loudly refusing every handshake, or going quiet
    /// about a file that will now fail forever, are both worse than this.
    #[test]
    fn a_torn_renewal_is_reported_once_and_the_presented_certificate_is_kept() {
        let disk = OnDisk::new("torn");
        let first = disk.mint(0);
        let (mut watch, _) = CertWatch::load(&disk.cert(), &disk.key()).unwrap();

        // Half a renewal: the key was replaced, the chain was not yet.
        let stranger = rcgen::generate_simple_self_signed(vec!["x".into()]).unwrap();
        std::fs::write(disk.key(), stranger.key_pair.serialize_pem()).unwrap();
        disk.bump(5);
        match watch.check() {
            Some(Reload::Failed(e)) => assert!(e.contains("server tls"), "{e}"),
            other => panic!("a stranger's key was accepted: {other:?}"),
        }
        assert_eq!(watch.fingerprint(), first, "the presented certificate must not change");
        assert!(watch.check().is_none(), "the same failure was said twice");

        // The key file vanishes entirely for a moment.
        std::fs::remove_file(disk.key()).unwrap();
        match watch.check() {
            Some(Reload::Failed(e)) => assert!(e.contains("privkey.pem"), "{e}"),
            other => panic!("a missing key went unremarked: {other:?}"),
        }
        assert!(watch.check().is_none());

        // The write completes.
        let second = disk.mint(10);
        match watch.check() {
            Some(Reload::Loaded(c)) => assert_eq!(c.fingerprint(), second),
            other => panic!("the completed renewal was not retried: {other:?}"),
        }
    }

    /// A relay told to present a certificate it cannot read must not come up
    /// presenting a self-signed one instead.
    #[test]
    fn a_certificate_that_cannot_be_loaded_at_startup_is_an_error_not_a_default() {
        let disk = OnDisk::new("startup");
        let e = CertWatch::load(&disk.cert(), &disk.key()).expect_err("no files");
        assert!(e.contains("fullchain.pem"), "{e}");
    }
}
