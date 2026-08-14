//! Where you left the editor is where you find it.
//!
//! Two things are persisted, per user, beside the other preferences in
//! [`crate::prefs::floptle_config_dir`]:
//!
//! - **the dock layout** — which tabs exist, where they are docked, how the
//!   splits are divided, which tab is front in each leaf.
//! - **the window** — its size, its position, and whether it was maximised.
//!
//! Neither used to be. Every session started at [`default_dock`] on a 1280×720
//! window, so anybody who works with the Inspector wider, or the Console pulled
//! out beside the viewport, rebuilt that arrangement every single time they
//! opened the editor. Arranging a workspace is work, and work the tool throws
//! away is work you stop doing — you learn to live with the default rather than
//! set it up again daily.
//!
//! # Restoring is best-effort, on purpose
//!
//! A layout file is a cache of a preference, not a document. Anything wrong with
//! it — written by an older build, hand-edited into nonsense, naming a tab this
//! build no longer has — falls back to the default **silently**. There is no
//! version of "your window arrangement would not load" that a person wants to
//! read at startup, and there is no version of it that should stop the editor
//! opening.
//!
//! What is *not* silent is the escape hatch: **Window ▸ Reset layout** puts the
//! default back, and it is the answer to every question that starts "my panels
//! have gone weird". Restoring a saved layout without one of those is a trap.
//!
//! # Why the window is clamped to a monitor
//!
//! A position saved on a second display is a window nobody can see once that
//! display is unplugged — the single most common way "remember my window" turns
//! into "the app will not start". [`WindowPlace::sane_on`] drops a position that
//! no longer lands on anything.

use std::path::PathBuf;

use crate::dock::{default_dock, EditorTab};
use crate::prefs::floptle_config_dir;

pub(crate) fn layout_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("layout.ron"))
}

pub(crate) fn window_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("window"))
}

/// Read the saved dock layout, or the default when there is not a usable one.
///
/// The saved layout is **repaired rather than trusted**: a tab that has arrived
/// since it was written is missing from it, and a person who upgrades should get
/// the new tab rather than have to know to reset. See [`repair`].
pub(crate) fn load_dock() -> egui_dock::DockState<EditorTab> {
    let Some(text) = layout_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return default_dock();
    };
    match ron::from_str::<egui_dock::DockState<EditorTab>>(&text) {
        Ok(mut dock) if !is_empty(&dock) => {
            repair(&mut dock);
            dock
        }
        // Unreadable, or readable and empty — a layout with no tabs in it is a
        // window with nothing in it, which is worse than the default.
        _ => default_dock(),
    }
}

pub(crate) fn save_dock(dock: &egui_dock::DockState<EditorTab>) {
    let Some(p) = layout_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = ron::to_string(dock) {
        let _ = std::fs::write(p, text);
    }
}

/// Throw the saved layout away, so the next start is a clean default even if
/// this session never gets to save (a crash, a kill).
pub(crate) fn forget_dock() {
    if let Some(p) = layout_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// Nothing to show. **`iter_all_tabs` rather than `main_surface`**: a document
/// with `"surfaces": []` is a perfectly good parse and indexing the main surface
/// of one panics. That shape is one hand-edit or one truncated write away.
fn is_empty(dock: &egui_dock::DockState<EditorTab>) -> bool {
    dock.iter_all_tabs().next().is_none()
}

/// The two tabs the editor cannot sensibly run without.
///
/// Everything else is closable and stays closed if that is how somebody left it
/// — the point of a saved layout is that it saves *your* arrangement. But a
/// window with no viewport in it has no way back to one except the Window menu,
/// and somebody whose layout file has lost the Scene tab has an editor that
/// looks broken rather than tidy.
const ESSENTIAL: &[EditorTab] = &[EditorTab::Scene, EditorTab::Game];

/// Make a loaded layout usable in *this* build.
///
/// A saved layout is a list of tab names, so it drifts from the code in one
/// direction as tabs are added. Anything essential that is missing comes back;
/// anything unknown was already dropped by the deserializer.
fn repair(dock: &mut egui_dock::DockState<EditorTab>) {
    for tab in ESSENTIAL {
        if dock.find_tab(tab).is_none() {
            dock.push_to_focused_leaf(*tab);
        }
    }
}

/// Where the window was and how big, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WindowPlace {
    pub(crate) width: f64,
    pub(crate) height: f64,
    /// Absent when the window has never been placed, or when the position it was
    /// saved at is no longer on any monitor.
    pub(crate) x: Option<f64>,
    pub(crate) y: Option<f64>,
    pub(crate) maximized: bool,
}

impl Default for WindowPlace {
    fn default() -> Self {
        WindowPlace { width: 1280.0, height: 720.0, x: None, y: None, maximized: false }
    }
}

impl WindowPlace {
    /// This place, with a position that is not on any of `monitors` dropped.
    ///
    /// `monitors` is a list of `(x, y, width, height)` in the same logical
    /// pixels. A window restored onto a display that has since been unplugged is
    /// a window nobody can find, and "the app opens somewhere I cannot see it"
    /// is indistinguishable from "the app does not open".
    ///
    /// The test is that the window's **top-left area** lands on something — not
    /// that the whole window fits. A window hanging off the right edge of a
    /// screen is normal and is how somebody left it; a window entirely past the
    /// end of every screen is not.
    pub(crate) fn sane_on(mut self, monitors: &[(f64, f64, f64, f64)]) -> WindowPlace {
        let (Some(x), Some(y)) = (self.x, self.y) else { return self };
        if monitors.is_empty() {
            return self;
        }
        // A margin of title bar, so a window whose corner is a pixel off screen
        // is still counted as visible.
        const GRAB: f64 = 64.0;
        let visible = monitors.iter().any(|&(mx, my, mw, mh)| {
            x + GRAB > mx && x < mx + mw && y + GRAB > my && y < my + mh
        });
        if !visible {
            self.x = None;
            self.y = None;
        }
        self
    }
}

/// Read the saved window place. Anything malformed is no saved place at all.
///
/// One whitespace-separated line: `width height x y maximized`, with `x`/`y`
/// written as `-` when unknown.
pub(crate) fn load_window() -> WindowPlace {
    let Some(text) = window_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return WindowPlace::default();
    };
    let f: Vec<&str> = text.split_whitespace().collect();
    if f.len() < 5 {
        return WindowPlace::default();
    }
    let num = |s: &str| s.parse::<f64>().ok();
    let (Some(width), Some(height)) = (num(f[0]), num(f[1])) else {
        return WindowPlace::default();
    };
    // A window of no size is a window that does not appear.
    if !(width.is_finite() && height.is_finite()) || width < 320.0 || height < 240.0 {
        return WindowPlace::default();
    }
    WindowPlace {
        width,
        height,
        x: num(f[2]).filter(|v| v.is_finite()),
        y: num(f[3]).filter(|v| v.is_finite()),
        maximized: f[4] == "1",
    }
}

pub(crate) fn save_window(p: &WindowPlace) {
    let Some(path) = window_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let one = |v: Option<f64>| v.map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into());
    let _ = std::fs::write(
        path,
        format!(
            "{:.0} {:.0} {} {} {}",
            p.width,
            p.height,
            one(p.x),
            one(p.y),
            if p.maximized { 1 } else { 0 }
        ),
    );
}

pub(crate) fn forget_window() {
    if let Some(p) = window_path() {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_round_trips_through_its_file_format() {
        let mut dock = default_dock();
        crate::dock::focus(&mut dock, EditorTab::Packages);
        let text = ron::to_string(&dock).unwrap();
        let back: egui_dock::DockState<EditorTab> = ron::from_str(&text).unwrap();
        assert!(back.find_tab(&EditorTab::Packages).is_some());
        assert!(back.find_tab(&EditorTab::Hierarchy).is_some());
    }

    /// A layout file is a cache of a preference. Nothing wrong with it may cost
    /// somebody an editor that will not open.
    #[test]
    fn nonsense_in_the_layout_file_is_the_default_layout() {
        for bad in ["", "()", "not a layout at all", "(surfaces: [])", "None", "[]"] {
            let dock: egui_dock::DockState<EditorTab> = ron::from_str(bad)
                .ok()
                .filter(|d| !is_empty(d))
                .unwrap_or_else(default_dock);
            assert!(dock.find_tab(&EditorTab::Scene).is_some(), "{bad:?}");
        }
    }

    /// A layout written before a tab existed must not leave somebody without a
    /// viewport — they would read that as the editor being broken.
    #[test]
    fn a_layout_missing_a_viewport_gets_one_back() {
        let mut dock = egui_dock::DockState::new(vec![EditorTab::Console]);
        repair(&mut dock);
        assert!(dock.find_tab(&EditorTab::Scene).is_some());
        assert!(dock.find_tab(&EditorTab::Game).is_some());
        // …and it does not force back the tabs somebody deliberately closed.
        assert!(dock.find_tab(&EditorTab::Mixer).is_none());
    }

    /// Repair must be idempotent, or every launch adds another Scene tab.
    #[test]
    fn repairing_a_healthy_layout_changes_nothing() {
        let mut dock = default_dock();
        let before = ron::to_string(&dock).unwrap();
        repair(&mut dock);
        repair(&mut dock);
        assert_eq!(ron::to_string(&dock).unwrap(), before);
    }

    #[test]
    fn a_window_place_round_trips_and_survives_a_broken_file() {
        let p = WindowPlace { width: 2560.0, height: 1400.0, x: Some(-8.0), y: Some(31.0), maximized: true };
        let one = |v: Option<f64>| v.map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into());
        let text = format!("{:.0} {:.0} {} {} 1", p.width, p.height, one(p.x), one(p.y));
        let f: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(f.len(), 5);
        assert_eq!(f[2], "-8");
        assert_eq!(f[3], "31");
    }

    /// The failure that turns "remember my window" into "the app will not
    /// start": a position on a monitor that is no longer there.
    #[test]
    fn a_window_saved_on_a_display_that_is_gone_opens_where_it_can_be_seen() {
        let laptop = [(0.0, 0.0, 1920.0, 1080.0)];
        let with_second = [(0.0, 0.0, 1920.0, 1080.0), (1920.0, 0.0, 2560.0, 1440.0)];

        let on_second =
            WindowPlace { x: Some(2400.0), y: Some(200.0), ..Default::default() };
        assert_eq!(on_second.sane_on(&with_second).x, Some(2400.0), "still plugged in");
        assert_eq!(on_second.sane_on(&laptop).x, None, "unplugged — do not open off-screen");

        // A window hanging slightly off an edge is how somebody left it, not a
        // problem to correct.
        let hanging = WindowPlace { x: Some(1800.0), y: Some(900.0), ..Default::default() };
        assert_eq!(hanging.sane_on(&laptop).x, Some(1800.0));
        // Negative coordinates are ordinary on a multi-monitor desktop.
        let left_of_zero = WindowPlace { x: Some(-1000.0), y: Some(100.0), ..Default::default() };
        assert_eq!(left_of_zero.sane_on(&[(-1920.0, 0.0, 1920.0, 1080.0)]).x, Some(-1000.0));

        // Knowing about no monitors is not evidence the position is bad.
        assert_eq!(on_second.sane_on(&[]).x, Some(2400.0));
    }

    /// A saved size of zero — which is what a minimised window reports on some
    /// platforms — must not be restored, or the editor opens invisible.
    #[test]
    fn a_degenerate_size_is_refused_in_favour_of_the_default() {
        let d = WindowPlace::default();
        for bad in ["0 0 - - 0", "-5 900 - - 0", "nan 900 - - 0", "100 80 - - 0", "1280"] {
            let f: Vec<&str> = bad.split_whitespace().collect();
            let ok = f.len() >= 5
                && f[0].parse::<f64>().is_ok_and(|w| w.is_finite() && w >= 320.0)
                && f[1].parse::<f64>().is_ok_and(|h| h.is_finite() && h >= 240.0);
            assert!(!ok, "{bad:?} should be refused");
        }
        assert_eq!(d.width, 1280.0);
    }
}
