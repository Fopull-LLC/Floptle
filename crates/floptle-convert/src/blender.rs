//! **Formats only Blender reads well: through Blender, when it is installed.**
//!
//! A `.blend` is Blender's own memory written to disk, and the only program
//! that reads it correctly is Blender. Collada and USD have Rust readers of a
//! sort, but none that keeps materials per face, applies the file's up axis
//! and transforms normals with their nodes; Blender's importers do all three.
//! So these go through the Blender the person already has: run in the
//! background, it opens or imports the file and writes it out with its own
//! glTF exporter — modifiers applied, textures embedded, armatures and
//! actions kept — and the result is read back like any other `.glb`.
//!
//! When Blender is not installed the conversion says so, names the formats
//! that need it, and says where it looked. It never pretends.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::{ConvertError, Report};

/// The extensions that convert through Blender.
pub const FORMATS: &[&str] = &["blend", "dae", "usd", "usda", "usdc", "usdz"];

/// The longest a conversion may run. A scene with a heavy modifier stack can
/// take a while to apply; one that has not finished in this long is stuck.
const TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The script Blender runs: open or import, then export a `.glb`.
const SCRIPT: &str = r#"
import bpy, sys
a = sys.argv[sys.argv.index("--") + 1:]
src, out = a[0], a[1]
ext = src.rsplit(".", 1)[-1].lower()
try:
    if ext == "blend":
        bpy.ops.wm.open_mainfile(filepath=src)
    else:
        bpy.ops.wm.read_factory_settings(use_empty=True)
        if ext == "dae":
            if "collada_import" not in dir(bpy.ops.wm):
                print("FLOPTLE_NO_IMPORTER collada")
                sys.exit(3)
            bpy.ops.wm.collada_import(filepath=src)
        else:
            bpy.ops.wm.usd_import(filepath=src)
    bpy.ops.export_scene.gltf(filepath=out, export_format="GLB", export_apply=True,
                              export_animations=True, export_skins=True, export_yup=True)
except Exception as e:
    print("FLOPTLE_ERROR", e)
    sys.exit(2)
"#;

/// Where Blender is, or `None`. `FLOPTLE_BLENDER` names it outright; then the
/// `PATH`; then where each platform's installer puts it; then every Steam
/// library on the machine, since Blender is on Steam and a Steam library can
/// be on any disk.
pub fn find() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLOPTLE_BLENDER") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    candidates().into_iter().find(|p| p.is_file())
}

/// The user's home folder.
fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Every place [`find`] looks, in order. Said in the message when nothing is
/// found, so a Blender in an odd place can be pointed at.
fn candidates() -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "blender.exe" } else { "blender" };
    let mut out: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(exe)).collect())
        .unwrap_or_default();
    let home = home();
    if cfg!(target_os = "macos") {
        out.push(PathBuf::from("/Applications/Blender.app/Contents/MacOS/Blender"));
        if let Some(h) = &home {
            out.push(h.join("Applications/Blender.app/Contents/MacOS/Blender"));
        }
    } else if cfg!(windows) {
        for root in ["C:\\Program Files\\Blender Foundation", "C:\\Program Files (x86)\\Blender Foundation"] {
            // One folder per installed version; the newest sorts last.
            let mut versions: Vec<PathBuf> = std::fs::read_dir(root)
                .map(|rd| rd.flatten().map(|e| e.path().join(exe)).collect())
                .unwrap_or_default();
            versions.sort();
            out.extend(versions.into_iter().rev());
        }
    } else {
        for p in ["/usr/bin/blender", "/usr/local/bin/blender", "/snap/bin/blender"] {
            out.push(PathBuf::from(p));
        }
        if let Some(h) = &home {
            out.push(h.join(".local/bin/blender"));
        }
    }
    out.extend(steam_blenders(&steam_roots(home.as_deref())));
    // A Flatpak Blender last: it runs in a sandbox that sees only what its
    // permissions grant, so a native one, from Steam or anywhere else, is the
    // better choice whenever there is one.
    if !cfg!(windows) && !cfg!(target_os = "macos") {
        out.push(PathBuf::from("/var/lib/flatpak/exports/bin/org.blender.Blender"));
        if let Some(h) = &home {
            out.push(h.join(".local/share/flatpak/exports/bin/org.blender.Blender"));
        }
    }
    out
}

/// Where Steam itself is installed, on this platform.
fn steam_roots(home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(windows) {
        // Steam records where it is in the registry; `reg` is on every
        // Windows, which spares a registry crate for one value.
        if let Ok(o) = Command::new("reg").args(["query", "HKCU\\Software\\Valve\\Steam", "/v", "SteamPath"]).output() {
            let text = String::from_utf8_lossy(&o.stdout);
            if let Some(v) = text.lines().find_map(|l| l.split_once("REG_SZ").map(|(_, v)| v.trim().to_string())) {
                out.push(PathBuf::from(v));
            }
        }
        out.push(PathBuf::from("C:\\Program Files (x86)\\Steam"));
        out.push(PathBuf::from("C:\\Program Files\\Steam"));
    } else if let Some(h) = home {
        if cfg!(target_os = "macos") {
            out.push(h.join("Library/Application Support/Steam"));
        } else {
            out.push(h.join(".local/share/Steam"));
            out.push(h.join(".steam/steam"));
            out.push(h.join(".steam/root"));
            // Steam as a Flatpak keeps its own home under ~/.var.
            out.push(h.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"));
            out.push(h.join(".var/app/com.valvesoftware.Steam/data/Steam"));
        }
    }
    out
}

/// Blender in every Steam library the Steam installs at `roots` know of: each
/// root itself, and every library its `libraryfolders.vdf` lists, which is
/// how a library on a second disk is found.
fn steam_blenders(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut libraries: Vec<PathBuf> = Vec::new();
    for root in roots {
        libraries.push(root.clone());
        for vdf in [root.join("steamapps/libraryfolders.vdf"), root.join("config/libraryfolders.vdf")] {
            if let Ok(text) = std::fs::read_to_string(&vdf) {
                libraries.extend(vdf_paths(&text).into_iter().map(PathBuf::from));
            }
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for lib in libraries {
        let app = lib.join("steamapps/common/Blender");
        let exe = if cfg!(target_os = "macos") {
            app.join("Blender.app/Contents/MacOS/Blender")
        } else if cfg!(windows) {
            app.join("blender.exe")
        } else {
            app.join("blender")
        };
        if !out.contains(&exe) {
            out.push(exe);
        }
    }
    out
}

/// Every `"path"` value in a Steam `.vdf` (Valve's KeyValues text), at any
/// depth. Tolerant on purpose: quoted strings with `\\` escapes are read,
/// braces, comments and anything unexpected are stepped over, and a file cut
/// short gives the paths up to the cut rather than none.
fn vdf_paths(text: &str) -> Vec<String> {
    let mut strings: Vec<String> = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            if let Some(e) = chars.next() {
                                s.push(match e {
                                    'n' => '\n',
                                    't' => '\t',
                                    other => other,
                                });
                            }
                        }
                        '"' => break,
                        other => s.push(other),
                    }
                }
                strings.push(s);
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            // A brace ends a key with no value; it must not pair the key
            // before it with the string after it.
            '{' | '}' => strings.push(String::from("\u{0}")),
            _ => {}
        }
    }
    strings
        .windows(2)
        .filter(|w| w[0].eq_ignore_ascii_case("path") && w[1] != "\u{0}")
        .map(|w| w[1].clone())
        .collect()
}

/// Convert `src` by running `blender` on it. The bytes of the `.glb` it
/// wrote, described.
pub fn convert(src: &Path) -> Result<(Vec<u8>, Report), ConvertError> {
    match find() {
        Some(b) => convert_with(src, &b),
        None => {
            let ext = src.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            Err(ConvertError::NeedsBlender(format!(
                "A .{ext} file converts through Blender, and Blender was not found. Install it from \
                 blender.org (it is free), or set FLOPTLE_BLENDER to where it is — or export the \
                 model from its program as .glb, .fbx or .obj."
            )))
        }
    }
}

/// [`convert`] with Blender at `blender`.
pub(crate) fn convert_with(src: &Path, blender: &Path) -> Result<(Vec<u8>, Report), ConvertError> {
    // Written beside the source, not in the temp folder: a sandboxed Blender
    // (Flatpak, Snap) has a private temp folder of its own, and a file it
    // writes there never arrives. The source's folder it can read, so it can
    // write there too.
    let tmp = src.with_file_name(format!(
        ".{}.floptle-blender-{}.glb",
        src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let src_abs = std::fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let mut child = Command::new(blender)
        .args(["--background", "--factory-startup", "--python-exit-code", "2", "--python-expr", SCRIPT, "--"])
        .arg(&src_abs)
        .arg(&tmp)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ConvertError::Io(format!("Blender at {} could not be started: {e}", blender.display())))?;

    // The pipes are drained on threads so a chatty Blender cannot fill one
    // and stall waiting for this side to read it.
    let drain = |r: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut r) = r {
                let _ = r.read_to_string(&mut s);
            }
            s
        })
    };
    let out_t = drain(child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>));
    let err_t = drain(child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ConvertError::Io("Blender did not finish converting in ten minutes and was stopped.".into()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(ConvertError::Io(e.to_string())),
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();

    if stdout.contains("FLOPTLE_NO_IMPORTER") {
        return Err(ConvertError::NeedsBlender(format!(
            "The Blender found ({}) no longer imports Collada — Blender 5 removed it. Export the \
             model as .fbx, .obj or .glb instead, or point FLOPTLE_BLENDER at a Blender 4.",
            blender.display()
        )));
    }
    let bytes = std::fs::read(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);
    let Some(bytes) = bytes.filter(|_| status.success()) else {
        let said = stdout
            .lines()
            .chain(stderr.lines())
            .find_map(|l| l.strip_prefix("FLOPTLE_ERROR ").map(str::to_string))
            .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()).map(str::to_string))
            .unwrap_or_else(|| "it wrote no model".into());
        return Err(ConvertError::Malformed(format!("Blender could not convert it: {said}")));
    };

    let (json, bin) = crate::stamp::split(&bytes)
        .ok_or_else(|| ConvertError::Malformed("Blender wrote something that is not a .glb".into()))?;
    let mut root: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| ConvertError::Malformed(e.to_string()))?;
    let names: Vec<String> = root["images"]
        .as_array()
        .map(|a| a.iter().map(|i| i["name"].as_str().unwrap_or("embedded").to_string()).collect())
        .unwrap_or_default();
    let mut report = Report::default();
    crate::gltf_pack::describe(&mut root, &names, &[], &mut report);
    if report.meshes == 0 {
        return Err(ConvertError::NoGeometry);
    }
    report.warnings.push(format!("Converted by Blender ({}).", blender.display()));
    Ok((crate::stamp::join(&root, bin), report))
}

#[cfg(test)]
mod vdf_tests {
    use super::*;

    /// Steam's own file, a second library on another disk, a Windows path
    /// with its escaped backslashes, and a file cut off mid-way.
    #[test]
    fn steam_library_paths_are_read_from_libraryfolders() {
        let text = r#""libraryfolders"
{
	"0"
	{
		"path"		"/home/me/.local/share/Steam"
		"label"		""
		"apps"
		{
			"365670"		"1234"
		}
	}
	// a comment
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
	}
	"2"
	{
		"PATH"		"D:\\Games\\Steam"
	}
	"3"
	{
		"path"		"/cut/off"#;
        // The value the cut fell in is kept as far as it got: it is only ever
        // a place to look, and a wrong one is simply not there.
        assert_eq!(vdf_paths(text), ["/home/me/.local/share/Steam", "/mnt/games/SteamLibrary", "D:\\Games\\Steam", "/cut/off"]);
        assert!(vdf_paths("not a vdf at all {{{").is_empty());
    }

    /// **Blender in a second Steam library is found** through the first
    /// library's `libraryfolders.vdf`.
    #[test]
    fn blender_in_a_second_steam_library_is_found() {
        let dir = std::env::temp_dir().join(format!("floptle-steam-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (steam, games) = (dir.join("Steam"), dir.join("Games"));
        std::fs::create_dir_all(steam.join("steamapps")).unwrap();
        std::fs::write(
            steam.join("steamapps/libraryfolders.vdf"),
            format!("\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} }}",
                steam.display(), games.display()).replace('\\', "\\\\"),
        )
        .unwrap();
        let exe = if cfg!(target_os = "macos") {
            "steamapps/common/Blender/Blender.app/Contents/MacOS/Blender"
        } else if cfg!(windows) {
            "steamapps/common/Blender/blender.exe"
        } else {
            "steamapps/common/Blender/blender"
        };
        std::fs::create_dir_all(games.join(exe).parent().unwrap()).unwrap();
        std::fs::write(games.join(exe), b"").unwrap();
        let found: Vec<PathBuf> = steam_blenders(&[steam]).into_iter().filter(|p| p.is_file()).collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, [games.join(exe)]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// **The Blender route is plumbed end to end.** A stand-in `blender`
    /// that writes a known `.glb` where it is told: what comes back must be
    /// that model, described. Blender itself is not needed to prove the
    /// arguments arrive and the output is read.
    #[test]
    fn a_blender_on_the_path_is_run_and_its_model_read_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("floptle-fake-blender-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models/blender_272_cube_7400_binary.fbx");
        let (glb, _) = crate::convert(&fixture).unwrap();
        std::fs::write(dir.join("cube.glb"), &glb).unwrap();
        // Blender's argv: … -- <source> <output>. Copy the cube to <output>.
        let fake = dir.join("blender");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\nfor a in \"$@\"; do last2=\"$last\"; last=\"$a\"; done\ncp '{}' \"$last\"\n", dir.join("cube.glb").display()),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("scene.blend"), b"BLENDER-v300").unwrap();

        let (bytes, report) = convert_with(&dir.join("scene.blend"), &fake).expect("the stand-in converts");
        assert_eq!(report.meshes, 1, "{report:?}");
        assert_eq!(report.triangles, 12);
        assert!(gltf::Gltf::from_slice(&bytes).is_ok());

        // A Blender that fails says why, in its words.
        std::fs::write(&fake, "#!/bin/sh\necho 'FLOPTLE_ERROR the file is from a newer Blender'\nexit 2\n").unwrap();
        let err = convert_with(&dir.join("scene.blend"), &fake).expect_err("fails");
        assert!(err.to_string().contains("newer Blender"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
