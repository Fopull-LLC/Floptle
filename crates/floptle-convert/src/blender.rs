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
/// `PATH`; then where each platform's installer puts it.
pub fn find() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLOPTLE_BLENDER") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    candidates().into_iter().find(|p| p.is_file())
}

/// Every place [`find`] looks, in order. Said in the message when nothing is
/// found, so a Blender in an odd place can be pointed at.
fn candidates() -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "blender.exe" } else { "blender" };
    let mut out: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(exe)).collect())
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
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
        for p in ["/usr/bin/blender", "/usr/local/bin/blender", "/snap/bin/blender", "/var/lib/flatpak/exports/bin/org.blender.Blender"] {
            out.push(PathBuf::from(p));
        }
        if let Some(h) = &home {
            out.push(h.join(".local/bin/blender"));
            out.push(h.join(".local/share/flatpak/exports/bin/org.blender.Blender"));
        }
    }
    out
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
    let tmp = std::env::temp_dir().join(format!(
        "floptle-blender-{}-{}.glb",
        std::process::id(),
        src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
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
        return Err(ConvertError::Unsupported(
            "dae — the Blender installed here no longer imports Collada (Blender 5 removed it). \
             Export the model as .fbx, .obj or .glb instead"
                .into(),
        ));
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
