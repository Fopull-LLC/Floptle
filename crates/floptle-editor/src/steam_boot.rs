//! Deciding whether a session activates Steam, and doing so.
//!
//! Kept as one seam so `main.rs`/`run.rs` call the same two functions
//! **unconditionally**, whatever the `steam` cargo feature is — every
//! `#[cfg(feature = "steam")]` in this file lives here and nowhere else, so
//! the process-entry-point and main-loop code that calls into it never
//! branches on the feature itself and can't accidentally break the default
//! (steam-off) build.
//!
//! **Never called from the editor's own docked Play-mode viewport** — only
//! from `floptle run --steam` (headless) and a player_mode session boot
//! (`floptle play` / an exported or served build), per
//! the Steam integration plan's "Where Steam activates".

use std::rc::Rc;

/// The Steam App ID a session should init with, or `None` to leave Steam
/// untouched entirely (`NullPlatform` stays the default).
///
/// `steam` is the project's configured `SteamProjectSettings`, if any.
/// `force` is true only for `floptle run --steam` — the explicit opt-in that
/// lets dev-time testing work even on a project with no `steam` settings at
/// all, via the Spacewar (480) fallback. A player_mode boot never forces:
/// an exported build with no `steam` block configured must not silently try
/// to talk to Steam.
pub(crate) fn resolve_app_id(
    steam: Option<floptle_scene::SteamProjectSettings>,
    force: bool,
) -> Option<u32> {
    match steam {
        Some(s) if s.app_id != 0 => Some(s.app_id),
        // A `steam` block exists but no id is set yet — Spacewar lets
        // dev-time testing work before a partner account exists.
        Some(_) => Some(480),
        None if force => Some(480),
        None => None,
    }
}

/// Initializes Steam for `app_id` — `restart_app_if_necessary` first (exits
/// the process immediately if it says so), then `SteamAPI_Init`. `None` on
/// any failure (no Steam client running is the ordinary case in dev/CI);
/// the caller falls back to `NullPlatform` and keeps going.
#[cfg(feature = "steam")]
pub(crate) fn boot(app_id: u32) -> Option<Rc<dyn floptle_services::Platform>> {
    if floptle_steam::restart_app_if_necessary(app_id) {
        std::process::exit(0);
    }
    match floptle_steam::SteamPlatform::init(app_id) {
        Ok(p) => {
            println!("steam: initialized (app {app_id})");
            Some(Rc::new(p))
        }
        Err(e) => {
            eprintln!("steam: could not initialize ({e}) — continuing without it");
            None
        }
    }
}

/// This build was compiled without the `steam` cargo feature — matches
/// [`boot`]'s signature so call sites never branch on the feature.
#[cfg(not(feature = "steam"))]
pub(crate) fn boot(app_id: u32) -> Option<Rc<dyn floptle_services::Platform>> {
    eprintln!(
        "steam: this build has no `steam` feature compiled in — app {app_id} requested, \
         continuing without it"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_steam_settings_and_not_forced_stays_untouched() {
        assert_eq!(resolve_app_id(None, false), None);
    }

    #[test]
    fn no_steam_settings_but_forced_falls_back_to_spacewar() {
        assert_eq!(resolve_app_id(None, true), Some(480));
    }

    #[test]
    fn steam_settings_with_no_id_falls_back_to_spacewar() {
        let s = floptle_scene::SteamProjectSettings { app_id: 0 };
        assert_eq!(resolve_app_id(Some(s), false), Some(480));
    }

    #[test]
    fn steam_settings_with_an_id_use_it_exactly() {
        let s = floptle_scene::SteamProjectSettings { app_id: 12345 };
        assert_eq!(resolve_app_id(Some(s), false), Some(12345));
    }
}
