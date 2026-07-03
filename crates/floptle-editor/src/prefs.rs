//! Per-user editor preferences — one small plain-text file per setting under
//! the platform config dir ([`floptle_config_dir`]) — plus the "Open in IDE"
//! launcher (ADR-0011) those preferences configure, and the viewport grid
//! settings they persist.

use std::path::{Path, PathBuf};

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
