//! **Automatic conversion writes only what it wrote.** Converting once, again
//! when the source changes, not again when only its time changed, and never
//! over a `.glb` somebody made — the rules the editor's background converter
//! leans on, checked here where they live.

mod support;

use std::path::Path;
use std::time::{Duration, SystemTime};

use floptle_convert::{convert_file, needs_conversion, output_state, source_of, stamp, ConvertError, Output};

fn models() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models")
}

fn set_mtime(p: &Path, t: SystemTime) {
    std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
}

#[test]
fn a_source_converts_once_and_again_only_when_it_changes() {
    let dir = support::scratch("own-change");
    let src = dir.join("ball.obj");
    std::fs::copy(models().join("blender_279_ball_0_obj.obj"), &src).unwrap();
    std::fs::copy(models().join("blender_279_ball_0_obj.mtl"), dir.join("blender_279_ball_0_obj.mtl")).unwrap();

    assert_eq!(output_state(&src), Output::Missing);
    assert!(needs_conversion(&src));
    let (out, _) = convert_file(&src).unwrap();
    assert_eq!(output_state(&src), Output::Converted { current: true });
    assert!(!needs_conversion(&src), "converted once is converted");
    assert_eq!(source_of(&out).as_deref(), Some(src.as_path()), "the .glb knows its source");

    // A new time and the same bytes — a clone, a copied folder — is not a
    // change: the hash in the stamp says so.
    let later = SystemTime::now() + Duration::from_secs(60);
    set_mtime(&src, later);
    assert!(!needs_conversion(&src), "a touched source with the same bytes is not converted again");

    // New bytes are.
    let mut text = std::fs::read_to_string(&src).unwrap();
    text.push_str("\n# edited\n");
    std::fs::write(&src, text).unwrap();
    set_mtime(&src, later + Duration::from_secs(60));
    assert_eq!(output_state(&src), Output::Converted { current: false });
    assert!(needs_conversion(&src));
    convert_file(&src).expect("its own output is written over");
    assert!(!needs_conversion(&src));

    // An OBJ's materials are in its .mtl; editing that is a change too.
    std::fs::write(src.with_extension("mtl"), "newmtl X\nKd 1 0 0\n").unwrap();
    set_mtime(&src.with_extension("mtl"), later + Duration::from_secs(120));
    assert!(needs_conversion(&src), "an edited .mtl converts the model again");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_glb_somebody_made_is_never_written_over() {
    let dir = support::scratch("own-theirs");
    let src = dir.join("cube.fbx");
    std::fs::copy(models().join("blender_272_cube_7400_binary.fbx"), &src).unwrap();
    // Their own .glb, of the same name — converted elsewhere and fixed up by
    // hand, or exported from Blender. No stamp.
    let (theirs, _) = floptle_convert::convert(&src).unwrap();
    std::fs::write(dir.join("cube.glb"), &theirs).unwrap();

    assert_eq!(output_state(&src), Output::TheirOwn);
    assert!(!needs_conversion(&src));
    let err = convert_file(&src).expect_err("refused");
    assert!(matches!(err, ConvertError::WouldOverwrite(_)), "{err}");
    assert!(err.to_string().contains("left alone"), "{err}");
    assert_eq!(std::fs::read(dir.join("cube.glb")).unwrap(), theirs, "not one byte of it changed");
    assert_eq!(source_of(&dir.join("cube.glb")), None, "and it is not counted as a conversion");

    // A conversion from a different source of the same name is not this
    // source's to replace either.
    std::fs::remove_file(dir.join("cube.glb")).unwrap();
    std::fs::copy(&src, dir.join("cube.obj")).ok();
    convert_file(&src).unwrap();
    assert_eq!(output_state(&dir.join("cube.obj")), Output::TheirOwn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_older_converters_output_is_converted_again() {
    let dir = support::scratch("own-old");
    let src = dir.join("cube.fbx");
    std::fs::copy(models().join("blender_272_cube_7400_binary.fbx"), &src).unwrap();
    let (out, _) = convert_file(&src).unwrap();
    let mut s = stamp::read_stamp(&out).unwrap();
    assert_eq!(s.converter, stamp::CONVERTER);
    s.converter -= 1;
    let restamped = stamp::stamp_glb(&std::fs::read(&out).unwrap(), &s);
    std::fs::write(&out, restamped).unwrap();
    assert_eq!(output_state(&src), Output::Converted { current: false });
    let _ = std::fs::remove_dir_all(&dir);
}
