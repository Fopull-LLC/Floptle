//! **The Floptle logo and app icons, embedded, and how a desktop is told about
//! them.**
//!
//! One source, `branding/floptle-logo.png`: the mark above the wordmark, white
//! line art with its shadow, on transparency. `scripts/brand/make-icons.py`
//! derives every icon from it — the mark alone, on a dark rounded tile — and
//! the results are committed beside it, so a binary embeds bytes that are
//! checked in rather than a build step nobody can run on a fresh machine.
//!
//! **Three places an icon has to be put, and they are not the same place.**
//!
//! - The **window**: [`Icon::at`] handed to winit (`with_window_icon`) or
//!   eframe (`with_icon`). That is the taskbar and the title bar on Windows
//!   and X11. **Wayland ignores it**: a Wayland compositor shows the icon of
//!   the `.desktop` entry whose name matches the window's `app_id`, and a
//!   window with no `app_id` gets the compositor's placeholder — which on
//!   GNOME is the yellow "W" Ty saw. So every window also names itself
//!   ([`APP_ID`], [`HUB_APP_ID`]) and the Hub installs the entries
//!   ([`linux::install`]) that those names resolve to.
//! - The **executable**: what Explorer and the Dock show for the file itself.
//!   On Windows that is a resource compiled into the `.exe` (each binary's
//!   `build.rs`, from `branding/floptle.ico`); on macOS it is an `.app`
//!   bundle's `.icns`, which the release does not build yet — a bare binary
//!   there shows the generic icon.
//! - The **logo itself**, [`LOGO_PNG`], for a page rather than a taskbar: the
//!   Hub's About, an editor splash, anything drawn on a dark ground.

use std::sync::Arc;

/// The window's name on Linux — the `app_id` under Wayland and the WM_CLASS
/// under X11 — and the stem of the `.desktop` entry that carries its icon.
pub const APP_ID: &str = "floptle";
/// The Hub's, likewise.
pub const HUB_APP_ID: &str = "floptle-hub";
/// The icon name both entries reference, as installed under `hicolor/`.
pub const ICON_NAME: &str = "floptle";

/// The full logo: mark and wordmark, white on transparency, 820 × 820. For a
/// dark ground; on a light one it is invisible, which is why the ICONS sit on
/// a tile.
pub const LOGO_PNG: &[u8] = include_bytes!("../../../branding/floptle-logo.png");

/// The app icon at each size the platforms ask for, PNG. The mark on a dark
/// rounded tile. `ICON_SIZES` lists them.
pub const ICON_SIZES: [u32; 9] = [16, 24, 32, 48, 64, 128, 256, 512, 1024];

pub fn icon_png(size: u32) -> Option<&'static [u8]> {
    Some(match size {
        16 => include_bytes!("../../../branding/icon-16.png"),
        24 => include_bytes!("../../../branding/icon-24.png"),
        32 => include_bytes!("../../../branding/icon-32.png"),
        48 => include_bytes!("../../../branding/icon-48.png"),
        64 => include_bytes!("../../../branding/icon-64.png"),
        128 => include_bytes!("../../../branding/icon-128.png"),
        256 => include_bytes!("../../../branding/icon-256.png"),
        512 => include_bytes!("../../../branding/icon-512.png"),
        1024 => include_bytes!("../../../branding/icon-1024.png"),
        _ => return None,
    })
}

/// Decoded pixels, straight RGBA, row-major — the shape every windowing API
/// takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Icon {
    /// The app icon at `size` (one of [`ICON_SIZES`]).
    pub fn at(size: u32) -> Option<Self> {
        decode(icon_png(size)?)
    }

    /// The full logo, decoded.
    pub fn logo() -> Self {
        decode(LOGO_PNG).expect("the committed logo decodes")
    }

    /// Shared, for an API that wants an `Arc`.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

/// Decode a PNG to straight 8-bit RGBA whatever it was stored as.
fn decode(png: &[u8]) -> Option<Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let bytes = &buf[..info.buffer_size()];
    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => bytes.to_vec(),
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
            bytes.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect()
        }
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => {
            bytes.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect()
        }
        (png::ColorType::Grayscale, png::BitDepth::Eight) => {
            bytes.iter().flat_map(|&g| [g, g, g, 255]).collect()
        }
        _ => return None,
    };
    Some(Icon { width: info.width, height: info.height, rgba })
}

/// **Telling a Linux desktop what these windows are.**
///
/// A Wayland compositor shows, for a window, the icon of the `.desktop` entry
/// whose file name matches the window's `app_id`; with no entry it shows a
/// placeholder. The Hub is the thing that is installed, so the Hub writes the
/// entries — for itself and for the editor it launches — and the icon at every
/// size, under the user's own XDG data directory. Idempotent: the same bytes
/// are written every time, and a file that already holds them is left alone.
#[cfg(target_os = "linux")]
pub mod linux {
    use std::path::{Path, PathBuf};

    /// `$XDG_DATA_HOME`, else `~/.local/share`.
    pub fn data_home() -> Option<PathBuf> {
        if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(x));
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
    }

    /// The `.desktop` text for one of the two apps. `exec` is the binary's
    /// absolute path; `%F` lets a project directory be dropped on the entry.
    pub fn desktop_entry(app_id: &str, name: &str, comment: &str, exec: &Path) -> String {
        // A path with a space is quoted; Exec= takes shell-like quoting.
        let exec = exec.display().to_string();
        let exec = if exec.contains(' ') { format!("\"{exec}\"") } else { exec };
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={name}\n\
             Comment={comment}\n\
             Exec={exec} %F\n\
             Icon={icon}\n\
             Terminal=false\n\
             Categories=Development;Game;\n\
             StartupWMClass={app_id}\n\
             StartupNotify=true\n",
            icon = super::ICON_NAME,
        )
    }

    /// Write the icon set and the two entries under `data_home`. Returns the
    /// paths written or confirmed. `editor` is the currently installed
    /// editor, if there is one; without it only the Hub's entry is written.
    pub fn install(data_home: &Path, hub: &Path, editor: Option<&Path>) -> Result<Vec<PathBuf>, String> {
        let mut done = Vec::new();
        for size in super::ICON_SIZES {
            let Some(png) = super::icon_png(size) else { continue };
            let dir = data_home.join(format!("icons/hicolor/{size}x{size}/apps"));
            let path = dir.join(format!("{}.png", super::ICON_NAME));
            write_if_changed(&path, png)?;
            done.push(path);
        }
        let apps = data_home.join("applications");
        let hub_entry = apps.join(format!("{}.desktop", super::HUB_APP_ID));
        write_if_changed(
            &hub_entry,
            desktop_entry(super::HUB_APP_ID, "Floptle Hub", "Install and open Floptle projects", hub).as_bytes(),
        )?;
        done.push(hub_entry);
        if let Some(editor) = editor {
            let entry = apps.join(format!("{}.desktop", super::APP_ID));
            write_if_changed(
                &entry,
                desktop_entry(super::APP_ID, "Floptle", "The Floptle game engine editor", editor).as_bytes(),
            )?;
            done.push(entry);
        }
        Ok(done)
    }

    fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
        if std::fs::read(path).is_ok_and(|old| old == bytes) {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every size the platforms ask for is embedded, decodes, and is what it
    /// says it is — a build that shipped a 0-byte icon would show nothing
    /// and say nothing.
    #[test]
    fn every_icon_size_is_embedded_and_decodes_to_its_size() {
        for s in ICON_SIZES {
            let i = Icon::at(s).unwrap_or_else(|| panic!("icon-{s}.png does not decode"));
            assert_eq!((i.width, i.height), (s, s), "icon-{s}.png");
            assert_eq!(i.rgba.len(), (s * s * 4) as usize);
            // The tile is opaque in the middle and the corners are rounded off.
            let px = |x: u32, y: u32| i.rgba[((y * s + x) * 4) as usize..][..4].to_vec();
            assert_eq!(px(s / 2, s / 2)[3], 255, "icon-{s}: the tile is not opaque");
            assert!(px(0, 0)[3] < 24, "icon-{s}: the corner is not rounded ({})", px(0, 0)[3]);
        }
        assert!(Icon::at(17).is_none());
    }

    /// The logo is the source everything else was cut from: white on
    /// transparency, and square.
    #[test]
    fn the_logo_is_white_line_art_on_transparency() {
        let l = Icon::logo();
        assert_eq!((l.width, l.height), (820, 820));
        let opaque = l.rgba.chunks(4).filter(|p| p[3] == 255).count();
        let white_opaque = l.rgba.chunks(4).filter(|p| p[3] == 255 && p[0] == 255 && p[1] == 255 && p[2] == 255).count();
        assert!(opaque > 10_000, "{opaque}");
        // The rest of the opaque pixels are the baked-in shadow, grey.
        assert!(white_opaque * 4 > opaque * 3, "the strokes are white: {white_opaque} of {opaque}");
        assert_eq!(l.rgba[3], 0, "the corner is transparent");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_desktop_entries_name_the_windows_and_the_icon() {
        use std::path::Path;
        let e = linux::desktop_entry(APP_ID, "Floptle", "The editor", Path::new("/opt/floptle/bin/floptle"));
        assert!(e.contains("Exec=/opt/floptle/bin/floptle %F\n"), "{e}");
        assert!(e.contains("Icon=floptle\n"), "{e}");
        assert!(e.contains("StartupWMClass=floptle\n"), "{e}");
        let spaced = linux::desktop_entry(HUB_APP_ID, "Floptle Hub", "", Path::new("/home/t/My Apps/floptle-hub"));
        assert!(spaced.contains("Exec=\"/home/t/My Apps/floptle-hub\" %F\n"), "{spaced}");

        let home = std::env::temp_dir().join(format!("brand-xdg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let written = linux::install(&home, Path::new("/x/floptle-hub"), Some(Path::new("/x/floptle"))).unwrap();
        assert!(home.join("applications/floptle-hub.desktop").is_file());
        assert!(home.join("applications/floptle.desktop").is_file());
        assert!(home.join("icons/hicolor/256x256/apps/floptle.png").is_file());
        assert_eq!(written.len(), ICON_SIZES.len() + 2);
        // Idempotent: the second run rewrites nothing (mtimes untouched).
        let before = std::fs::metadata(home.join("applications/floptle.desktop")).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        linux::install(&home, Path::new("/x/floptle-hub"), Some(Path::new("/x/floptle"))).unwrap();
        let after = std::fs::metadata(home.join("applications/floptle.desktop")).unwrap().modified().unwrap();
        assert_eq!(before, after, "an unchanged entry was rewritten");
        let _ = std::fs::remove_dir_all(&home);
    }
}
