//! Project trust — the VS Code shape.
//!
//! A package under a project's `packages/` folder runs its `editor/*.lua`
//! the moment the project opens, with whatever permissions its own
//! `package.ron` declares. The catalogue's install path asks first; a project
//! that arrives as a folder or a zip with packages already inside it asked
//! nobody. "I opened a project and it posted my files somewhere" is the
//! complaint this exists to make impossible.
//!
//! So the **first open of a project whose permission-asking packages the user
//! has not seen** loads those packages with **no permissions**, and a banner
//! says which packages ask for what. Trusting records a fingerprint of the
//! package set — ids, permissions and the manifest text — against the project
//! path in the user's config directory; a changed manifest is a new
//! fingerprint and asks again. A project with no package asking for anything
//! has nothing to trust and shows nothing.
//!
//! `floptle exec` is exempt by design (a script the user named on their own
//! command line is the user's own code), and the Hub's "new project from a
//! template" writes projects that carry no packages.

use std::path::{Path, PathBuf};

use floptle_package::{Loaded, Permission};

/// One package that asks: its id and what it asks for.
pub(crate) type Ask = (String, Vec<Permission>);

/// Where a project stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Trust {
    /// No package asks for a permission: nothing to decide.
    #[default]
    NothingToAsk,
    /// The fingerprint is in the store.
    Trusted,
    /// Packages loaded with their permissions withheld, and the banner is up.
    Untrusted { asks: Vec<Ask>, fingerprint: String },
    /// The user chose to keep the packages restricted this session; the banner
    /// is down, the permissions stay withheld.
    Restricted { fingerprint: String },
}

/// The permission-asking packages of a project, fingerprinted.
///
/// `None` when nothing asks for anything. Otherwise the hex fingerprint plus
/// the list the banner shows. The fingerprint covers each asking package's id,
/// its permissions and the bytes of its manifest, so editing a manifest —
/// adding `Network` to one that had only `Files` — asks again.
pub(crate) fn fingerprint(loaded: &[Loaded]) -> Option<(String, Vec<Ask>)> {
    let mut asks: Vec<Ask> = loaded
        .iter()
        .filter(|p| !p.manifest.permissions.is_empty())
        .map(|p| {
            let mut perms = p.manifest.permissions.clone();
            perms.sort();
            perms.dedup();
            (p.id().to_string(), perms)
        })
        .collect();
    if asks.is_empty() {
        return None;
    }
    asks.sort();
    let mut h = Fnv::new();
    for p in loaded {
        if p.manifest.permissions.is_empty() {
            continue;
        }
        h.eat(p.id().as_bytes());
        h.eat(b"\0");
        let mut perms = p.manifest.permissions.clone();
        perms.sort();
        for perm in perms {
            h.eat(perm.name().as_bytes());
            h.eat(b",");
        }
        h.eat(b"\0");
        let manifest = floptle_vfs::read(p.root.join(floptle_package::MANIFEST_FILE)).unwrap_or_default();
        h.eat(&manifest);
        h.eat(b"\0");
    }
    Some((format!("{:016x}", h.0), asks))
}

/// FNV-1a. Not a security hash and not meant as one: the store is the user's
/// own file, and the fingerprint's job is to notice a manifest that changed.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn eat(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// The user's list of trusted projects: one line per project, the project's
/// path and the fingerprint it was trusted at, tab-separated.
#[derive(Clone, Debug)]
pub(crate) struct TrustStore {
    /// `None` when there is nowhere to keep it (no config dir): nothing is
    /// trusted and trusting persists nothing, which is the safe way round.
    path: Option<PathBuf>,
}

impl Default for TrustStore {
    /// The user's store — except under test, where a test that opens a
    /// project must neither read the developer's real list nor write to it.
    fn default() -> Self {
        if cfg!(test) {
            Self::at(std::env::temp_dir().join(format!("floptle-trust-test-{}", std::process::id())))
        } else {
            Self::user()
        }
    }
}

impl TrustStore {
    pub(crate) fn user() -> Self {
        Self { path: crate::prefs::floptle_config_dir().map(|d| d.join("trusted_projects.txt")) }
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn key(root: &Path) -> String {
        floptle_vfs::normalize(root).to_string_lossy().into_owned()
    }

    fn lines(&self) -> Vec<(String, String)> {
        let Some(p) = &self.path else { return Vec::new() };
        floptle_vfs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('\t').map(|(a, b)| (a.to_string(), b.trim().to_string())))
            .collect()
    }

    pub(crate) fn is_trusted(&self, root: &Path, fingerprint: &str) -> bool {
        let key = Self::key(root);
        self.lines().iter().any(|(k, fp)| *k == key && fp == fingerprint)
    }

    /// Record the project at this fingerprint, replacing any earlier line.
    pub(crate) fn trust(&self, root: &Path, fingerprint: &str) {
        let Some(p) = &self.path else { return };
        let key = Self::key(root);
        let mut lines: Vec<(String, String)> =
            self.lines().into_iter().filter(|(k, _)| *k != key).collect();
        lines.push((key, fingerprint.to_string()));
        let text: String = lines.iter().map(|(k, fp)| format!("{k}\t{fp}\n")).collect();
        if let Some(dir) = p.parent() {
            let _ = floptle_vfs::create_dir_all(dir);
        }
        let _ = floptle_vfs::write(p, text);
    }
}

/// The sentence a withheld permission raises with, so a package author and a
/// user reading the Console both know what to do.
pub(crate) fn withheld_message(pkg: &str, perm: Permission) -> String {
    format!(
        "package {pkg} asked for the `{}` permission, and this project is not trusted yet — \
         its packages run with no permissions until you choose Trust in the banner at the top \
         of the editor",
        perm.name()
    )
}

/// What the banner's buttons say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Trust,
    KeepRestricted,
}

/// Draw the banner for an untrusted project, across the top of `ui`. Returns
/// whether anything was drawn and the button pressed, if any. Nothing is
/// drawn for any other state.
pub(crate) fn banner(ui: &mut egui::Ui, trust: &Trust) -> (bool, Option<Answer>) {
    let Trust::Untrusted { asks, .. } = trust else { return (false, None) };
    let mut answer = None;
    egui::Panel::top("project_trust_banner")
        .frame(egui::Frame::NONE.fill(egui::Color32::from_rgb(64, 52, 20)).inner_margin(8.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let n = asks.len();
                let what: Vec<String> = asks
                    .iter()
                    .map(|(id, perms)| {
                        let names: Vec<&str> = perms.iter().map(|p| p.name()).collect();
                        format!("{id} ({})", names.join(", "))
                    })
                    .collect();
                ui.label(
                    egui::RichText::new(format!(
                        "⚠ This project carries {n} package{} that ask{} for permissions: {}. \
                         They are loaded with none until you trust this project.",
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "s" } else { "" },
                        what.join("; ")
                    ))
                    .color(egui::Color32::from_rgb(255, 230, 160)),
                );
                if ui.button("Trust this project").on_hover_text(
                    "grants each package what its manifest asks for, and remembers this set \
                     of packages for this project — a changed manifest asks again",
                ).clicked() {
                    answer = Some(Answer::Trust);
                }
                if ui.button("Keep them restricted").on_hover_text(
                    "the packages stay loaded with no permissions for this session; the banner \
                     comes back the next time the project opens",
                ).clicked() {
                    answer = Some(Answer::KeepRestricted);
                }
            });
        });
    (true, answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store remembers a project at a fingerprint, and only that one: a
    /// different fingerprint for the same project is not trusted.
    #[test]
    fn the_store_trusts_a_project_at_one_fingerprint_only() {
        let dir = std::env::temp_dir().join(format!("floptle-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = TrustStore::at(dir.join("trusted_projects.txt"));
        let a = dir.join("A");
        let b = dir.join("B");
        assert!(!store.is_trusted(&a, "aaaa"));
        store.trust(&a, "aaaa");
        assert!(store.is_trusted(&a, "aaaa"));
        assert!(!store.is_trusted(&a, "bbbb"), "a changed manifest must ask again");
        assert!(!store.is_trusted(&b, "aaaa"), "another project is another line");
        // Re-trusting at a new fingerprint replaces the line rather than growing the file.
        store.trust(&a, "bbbb");
        assert!(store.is_trusted(&a, "bbbb") && !store.is_trusted(&a, "aaaa"));
        assert_eq!(store.lines().len(), 1);
        // Nowhere to keep it: nothing is trusted, and trusting is a no-op.
        let nowhere = TrustStore { path: None };
        nowhere.trust(&a, "aaaa");
        assert!(!nowhere.is_trusted(&a, "aaaa"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The banner exists for an untrusted project and for nothing else.
    #[test]
    fn the_banner_shows_for_an_untrusted_project_only() {
        let ctx = crate::icons::test_context();
        let untrusted = Trust::Untrusted {
            asks: vec![("grass".into(), vec![Permission::Network, Permission::Files])],
            fingerprint: "f".into(),
        };
        let mut drawn = (false, None);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| drawn = banner(ui, &untrusted));
        assert_eq!(drawn, (true, None));
        for quiet in [Trust::NothingToAsk, Trust::Trusted, Trust::Restricted { fingerprint: "f".into() }] {
            let mut drawn = (true, None);
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| drawn = banner(ui, &quiet));
            assert_eq!(drawn, (false, None), "a banner was drawn for {quiet:?}");
        }
    }
}
