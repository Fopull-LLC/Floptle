//! Per-user editor preferences — one small plain-text file per setting under
//! the platform config dir ([`floptle_config_dir`]) — plus the "Open in IDE"
//! launcher (ADR-0011) those preferences configure, and the viewport grid
//! settings they persist.

use std::path::{Path, PathBuf};

/// Editor reference-grid display + snapping settings.
#[derive(Clone, Copy)]
pub(crate) struct GridConfig {
    pub(crate) show: bool,
    /// Spacing between grid lines (world units) — also the snap increment.
    pub(crate) size: f32,
    /// Cells out from the center the grid extends.
    pub(crate) extent: i32,
    pub(crate) color: [f32; 3],
    pub(crate) alpha: f32,
    /// Snap moved/created objects to the grid.
    pub(crate) snap: bool,
    /// How far BELOW the camera the grid plane sits (world units, snapped to `size`).
    pub(crate) y_offset: f32,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            show: true,
            size: 1.0,
            extent: 24,
            color: [0.45, 0.45, 0.58],
            alpha: 0.32,
            snap: false,
            y_offset: DEFAULT_GRID_Y_OFFSET,
        }
    }
}

// ---- "Open in IDE" (ADR-0011): launch the user's external editor ------------

/// Is `cmd` (a binary name) resolvable on PATH?
pub(crate) fn on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        dir.join(cmd).is_file()
            || (cfg!(windows)
                && ["exe", "cmd", "bat"].iter().any(|e| dir.join(format!("{cmd}.{e}")).is_file()))
    })
}

/// Pick a sensible default external editor by probing PATH (VSCode first).
pub(crate) fn auto_detect_editor() -> String {
    for c in ["code", "codium", "code-insiders", "zed", "subl", "nvim", "vim", "nano"] {
        if on_path(c) {
            return c.to_string();
        }
    }
    "code".to_string()
}

/// The per-user config directory for Floptle (platform-appropriate).
pub(crate) fn floptle_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("floptle"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/floptle"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("floptle"))
    }
}

pub(crate) fn editor_pref_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("external_editor"))
}

/// The configured external editor command, or an auto-detected default if unset.
pub(crate) fn load_external_editor() -> String {
    editor_pref_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(auto_detect_editor)
}

pub(crate) fn save_external_editor(cmd: &str) {
    if let Some(p) = editor_pref_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, cmd.trim());
    }
}

pub(crate) fn prefer_pref_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("prefer_external_editor"))
}

/// Whether the user prefers their external editor over the in-engine IDE.
pub(crate) fn load_prefer_external() -> bool {
    prefer_pref_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

pub(crate) fn save_prefer_external(v: bool) {
    if let Some(p) = prefer_pref_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, if v { "1" } else { "0" });
    }
}

/// The default play-mode chrome tint: a small, even additive RGB nudge (brighten).
pub(crate) const DEFAULT_PLAY_TINT: [u8; 3] = [9, 9, 9];

pub(crate) fn play_tint_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("play_tint"))
}

/// The play-mode editor tint preference: `(enabled, additive RGB offset)`.
/// File format is one line: `enabled r g b` (e.g. `1 10 18 30`).
pub(crate) fn load_play_tint() -> (bool, [u8; 3]) {
    let parsed = play_tint_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| {
            let nums: Vec<&str> = s.split_whitespace().collect();
            if nums.len() == 4 {
                Some((
                    nums[0] == "1",
                    [
                        nums[1].parse().ok()?,
                        nums[2].parse().ok()?,
                        nums[3].parse().ok()?,
                    ],
                ))
            } else {
                None
            }
        });
    parsed.unwrap_or((true, DEFAULT_PLAY_TINT))
}

pub(crate) fn save_play_tint(enabled: bool, tint: [u8; 3]) {
    if let Some(p) = play_tint_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let on = if enabled { 1 } else { 0 };
        let _ = std::fs::write(p, format!("{on} {} {} {}", tint[0], tint[1], tint[2]));
    }
}

/// Whether Lua `gizmo.*` shapes also draw over the GAME view. Off by default, and
/// persisted, because a project that wants hit/hurtboxes while playing wants them every
/// session, not once.
pub(crate) fn game_gizmos_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("game_gizmos"))
}

pub(crate) fn load_game_gizmos() -> bool {
    game_gizmos_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

pub(crate) fn save_game_gizmos(on: bool) {
    if let Some(p) = game_gizmos_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, if on { "1" } else { "0" });
    }
}

/// The default grid `y_offset` — the grid sits this far below the camera by default (a
/// little lower than eye level, nearer the floor). Persisted, so a user's value sticks.
pub(crate) const DEFAULT_GRID_Y_OFFSET: f32 = 2.0;

pub(crate) fn grid_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("grid"))
}

/// Load the persisted grid settings (all fields), falling back to defaults per-field so a
/// short/old file still loads. Format is one whitespace-separated line:
/// `show size extent r g b alpha snap y_offset`.
pub(crate) fn load_grid() -> GridConfig {
    let mut g = GridConfig::default();
    if let Some(s) = grid_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() >= 9 {
            g.show = f[0] == "1";
            if let Ok(v) = f[1].parse() { g.size = v; }
            if let Ok(v) = f[2].parse() { g.extent = v; }
            if let (Ok(r), Ok(gc), Ok(b)) = (f[3].parse(), f[4].parse(), f[5].parse()) {
                g.color = [r, gc, b];
            }
            if let Ok(v) = f[6].parse() { g.alpha = v; }
            g.snap = f[7] == "1";
            if let Ok(v) = f[8].parse() { g.y_offset = v; }
        }
    }
    g
}

pub(crate) fn save_grid(g: &GridConfig) {
    if let Some(p) = grid_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            p,
            format!(
                "{} {} {} {} {} {} {} {} {}",
                if g.show { 1 } else { 0 },
                g.size,
                g.extent,
                g.color[0],
                g.color[1],
                g.color[2],
                g.alpha,
                if g.snap { 1 } else { 0 },
                g.y_offset,
            ),
        );
    }
}

pub(crate) fn engine_theme_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("engine_theme"))
}
pub(crate) fn code_theme_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("code_theme"))
}

/// A persisted theme index, clamped to a valid entry (0 if unset/out of range).
pub(crate) fn load_theme_index(path: Option<PathBuf>, count: usize) -> usize {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&i| i < count)
        .unwrap_or(0)
}

pub(crate) fn save_theme_index(path: Option<PathBuf>, idx: usize) {
    if let Some(p) = path {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, idx.to_string());
    }
}

/// Launch the external editor on `file`. VSCode-family editors open the project as
/// the workspace root and jump to `file:line` (ADR-0011); others just open the file.
/// `cmd` may include leading args (e.g. "code -n").
pub(crate) fn open_external_editor(cmd: &str, project_root: &Path, file: &str, line: usize) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let Some((prog, pre)) = parts.split_first() else { return };
    let mut command = std::process::Command::new(prog);
    command.args(pre);
    if prog.contains("code") {
        command.arg(project_root).arg("--goto").arg(format!("{file}:{line}"));
    } else {
        command.arg(file);
    }
    if let Err(e) = command.spawn() {
        eprintln!("  Open in IDE ({prog}) failed: {e}");
    }
}

// --- the Scene view's floating panels ---------------------------------------

pub(crate) fn viewport_panels_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("viewport_panels.ron"))
}

/// Where the Scene view's overlay panels sit. Per-user, because it is a fact
/// about how somebody likes to work, and persisted because moving your tool
/// palette every session is worse than it being in the wrong place once.
pub(crate) fn load_viewport_panels() -> crate::viewport_panel::ViewportPanels {
    viewport_panels_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| ron::from_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn save_viewport_panels(p: &crate::viewport_panel::ViewportPanels) {
    let Some(path) = viewport_panels_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = ron::ser::to_string_pretty(p, ron::ser::PrettyConfig::default()) {
        let _ = std::fs::write(path, s);
    }
}

// --- the 🖼 image canvas's overlays ----------------------------------------

/// How the image canvas draws everything that is **not** your art: the
/// transparency checker, the pixel grid, the sheet cell grid.
///
/// Every one of these used to be a literal in the draw code, which is fine right
/// up until the art is the colour of the overlay — and which art that is, is not
/// knowable in advance. A 28-alpha white grid over pale pixel art is invisible at
/// exactly the zoom you need it at, and a grey checker under grey art says
/// nothing at all (`floptle/0097`).
///
/// Per-user and not per-document: this is how somebody likes to LOOK at images,
/// not a fact about one image. (The cell grid's size is the fact — that lives in
/// the document.)
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct CanvasLook {
    pub(crate) checker: bool,
    pub(crate) checker_a: [u8; 3],
    pub(crate) checker_b: [u8; 3],
    /// Checker square edge in SCREEN pixels, so it neither moirés at high zoom
    /// nor vanishes at low zoom.
    pub(crate) checker_px: f32,

    pub(crate) pixel_grid: bool,
    pub(crate) pixel_grid_color: [u8; 3],
    pub(crate) pixel_grid_alpha: u8,
    /// Screen pixels per texel below which the pixel grid is more noise than
    /// grid. Was a hard-coded 6, which suits one art scale.
    pub(crate) pixel_grid_zoom: f32,
    /// Draw the pixel grid as a light line under dark dashes, so it survives a
    /// background of any colour without being configured for it. The setting is
    /// the escape hatch; this is the design.
    pub(crate) pixel_grid_two_tone: bool,

    pub(crate) cell_grid: bool,
    pub(crate) cell_grid_color: [u8; 3],
    pub(crate) cell_grid_alpha: u8,
}

impl Default for CanvasLook {
    fn default() -> Self {
        CanvasLook {
            checker: true,
            checker_a: [58, 58, 58],
            checker_b: [48, 48, 48],
            checker_px: 8.0,
            pixel_grid: true,
            pixel_grid_color: [255, 255, 255],
            // 28/255 was 11% — present in a screenshot, absent over real art.
            pixel_grid_alpha: 46,
            pixel_grid_zoom: 6.0,
            pixel_grid_two_tone: true,
            cell_grid: true,
            // Deliberately not white: the cell grid must not read as a heavier
            // pixel grid, it is a different thing about a different unit.
            cell_grid_color: [120, 190, 255],
            cell_grid_alpha: 170,
        }
    }
}

pub(crate) fn canvas_look_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("canvas_look.ron"))
}

/// Load the canvas overlay settings. Anything missing or unreadable falls back
/// per-field (`#[serde(default)]`), so a file written by an older build — or a
/// half-edited one — still opens rather than resetting the lot.
pub(crate) fn load_canvas_look() -> CanvasLook {
    canvas_look_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| ron::from_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn save_canvas_look(look: &CanvasLook) {
    let Some(p) = canvas_look_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = ron::ser::to_string_pretty(look, ron::ser::PrettyConfig::default()) {
        let _ = std::fs::write(p, s);
    }
}

#[cfg(test)]
mod canvas_look_tests {
    use super::*;

    #[test]
    fn the_canvas_look_round_trips_through_its_file_format() {
        let look = CanvasLook {
            pixel_grid_alpha: 200,
            checker_a: [10, 20, 30],
            cell_grid: false,
            ..Default::default()
        };
        let s = ron::ser::to_string_pretty(&look, ron::ser::PrettyConfig::default()).unwrap();
        assert_eq!(ron::from_str::<CanvasLook>(&s).unwrap(), look);
    }

    /// A settings file from a build that had fewer fields must not reset the
    /// ones it did have — which is what a non-`default` deserialize would do.
    #[test]
    fn an_older_settings_file_keeps_what_it_says_and_defaults_the_rest() {
        let look: CanvasLook = ron::from_str("(pixel_grid_alpha: 90)").unwrap();
        assert_eq!(look.pixel_grid_alpha, 90);
        assert_eq!(look.checker_a, CanvasLook::default().checker_a);
        assert!(look.pixel_grid_two_tone);
    }
}
