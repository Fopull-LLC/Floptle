//! Getting a model's own textures **out of it** — as files, on disk, that a
//! material can point at.
//!
//! A `.glb` carries its images inside itself. The engine decodes them at import
//! and hands them to the GPU, and that was the end of the road: the Inspector
//! could tell you a material was `🖼 textured` and there was no way to see the
//! picture, edit it, or point anything else at it. Which blocks the ordinary
//! reason anybody looks — *"I want the character's skin as a base layer and my
//! own shirt drawn over it"*. You cannot layer on top of an image you cannot
//! obtain.
//!
//! It also blocks the smaller thing that happens every time somebody overrides
//! one part of a model: an override is a whole material, so a part that HAD a
//! texture and whose override names none draws untextured. Extracting first
//! means the override can start as what the part already looked like.
//!
//! Written next to the model as `<model-stem>_textures/<material>.png`, so a
//! model dropped in a folder keeps its art beside it and two models that both
//! call a material `Body` cannot overwrite each other.

use std::path::{Path, PathBuf};

/// One extracted image: the material it belongs to, and where it landed
/// (project-relative, which is how a material references a texture).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Extracted {
    pub material: String,
    pub path: String,
}

/// A file name that cannot escape its folder or collide with a path separator —
/// glTF material names are arbitrary strings and `Body/Skin` is a legal one.
fn safe_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_matches(['.', '_']).to_string();
    if trimmed.is_empty() { "material".to_string() } else { trimmed }
}

/// The file stem one material's texture is written under — derived from the
/// material NAME ALONE, so that asking where a texture went and putting it there
/// cannot disagree.
///
/// `safe_stem` maps arbitrary glTF names onto safe file names and is not
/// injective: `Body/Skin` and `Body_Skin` are two materials and one file name.
/// Resolving that by numbering collisions as they are met would make the answer
/// depend on the ORDER the parts were walked — and `extracted_file`, which looks
/// a texture up later to seed an override with it, has no order to walk. It
/// would hand the second material the first one's picture.
///
/// So a name that had to be sanitised carries a short tag derived from the
/// original, and a name that did not is left alone. Two different materials
/// cannot land on one stem, and the stem is a pure function of the name.
fn unique_stem(material: &str) -> String {
    let base = safe_stem(material);
    if base == material {
        return base;
    }
    // FNV-1a over the original name: stable across runs and machines (a
    // `DefaultHasher` is neither), and four bytes is plenty to separate the
    // handful of materials one model has.
    let mut h: u32 = 0x811c_9dc5;
    for b in material.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{base}-{h:08x}")
}

/// Where one model's extracted textures live.
pub(crate) fn extract_dir(model_rel: &str) -> String {
    let p = Path::new(model_rel);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let parent = p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
    let folder = format!("{}_textures", safe_stem(&stem));
    if parent.is_empty() { folder } else { format!("{parent}/{folder}") }
}

/// Decode `model_abs`'s embedded base-colour textures and write one PNG per
/// MATERIAL that has one, under [`extract_dir`].
///
/// Returns what was written, project-relative, in the model's own part order.
/// A material appearing on several parts is written once — it is one image.
///
/// Re-extracting overwrites: the model file is the source of truth for what its
/// materials look like, and a stale copy of an image somebody re-exported is
/// worse than no copy. (Anything a dev PAINTS belongs in their own texture, not
/// in the folder named after the model.)
pub(crate) fn extract_model_textures(
    model_abs: &Path,
    model_rel: &str,
    project_root: &Path,
) -> Result<Vec<Extracted>, String> {
    let model = floptle_assets::import(model_abs).map_err(|e| e.to_string())?;
    let dir_rel = extract_dir(model_rel);
    let dir_abs = project_root.join(&dir_rel);
    let mut out: Vec<Extracted> = Vec::new();
    for part in &model.parts {
        let Some(idx) = part.texture else { continue };
        if out.iter().any(|e| e.material == part.material) {
            continue;
        }
        // **One file per MATERIAL, even when two materials share an image.**
        //
        // The file is found again by material name — that is how the Inspector
        // seeds an override with what a part already wore, and how anybody
        // reading the folder knows which picture belongs to what. Naming a
        // shared image after whichever material happened to reach it first left
        // the others looking un-extracted forever: they re-extracted on every
        // click, and the file they were told to expect never existed. Two copies
        // of one image is a cheap price for a folder that says what it holds.
        let tex = model.textures.get(idx).ok_or("the model named an image it has not")?;
        let name = unique_stem(&part.material);
        let rel = format!("{dir_rel}/{name}.png");
        floptle_assets::save_texture_png(tex, &dir_abs.join(format!("{name}.png")))
            .map_err(|e| format!("writing {rel}: {e}"))?;
        out.push(Extracted { material: part.material.clone(), path: rel });
    }
    if out.is_empty() {
        return Err("this model carries no textures — its materials are flat colours".into());
    }
    Ok(out)
}

/// The file one material's extracted texture would be at, whether or not it has
/// been extracted yet.
pub(crate) fn extracted_path(model_rel: &str, material: &str) -> String {
    format!("{}/{}.png", extract_dir(model_rel), unique_stem(material))
}

/// …and whether it is actually there.
pub(crate) fn extracted_file(project_root: &Path, model_rel: &str, material: &str) -> Option<String> {
    let rel = extracted_path(model_rel, material);
    let abs: PathBuf = project_root.join(&rel);
    floptle_vfs::is_file(&abs).then_some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two materials whose names sanitise to one file name get one file EACH.
    ///
    /// `safe_stem` is not injective — `Body/Skin` and `Body_Skin` are different
    /// materials and the same file name — and two materials silently sharing one
    /// picture is a wrong look nobody would think to check.
    #[test]
    fn two_materials_never_share_one_file() {
        // A name that needs no sanitising keeps it…
        assert_eq!(unique_stem("Body_Skin"), "Body_Skin");
        // …and one that does gets a tag from its ORIGINAL name, so the two
        // cannot collide however they are ordered.
        assert_ne!(unique_stem("Body/Skin"), unique_stem("Body_Skin"));
        assert!(unique_stem("Body/Skin").starts_with("Body_Skin-"));
        // Stable: the same name always lands in the same file, which is what
        // lets `extracted_file` find one without re-reading the model.
        assert_eq!(unique_stem("Body/Skin"), unique_stem("Body/Skin"));
        // …and that is exactly the path the writer uses.
        assert_eq!(
            extracted_path("models/hero.glb", "Body/Skin"),
            format!("models/hero_textures/{}.png", unique_stem("Body/Skin"))
        );
    }

    /// End to end, on a real `.glb`: the pictures come out as files.
    ///
    /// The model is the repo's own R6 avatar — three materials (`Clothing`,
    /// `Head`, `Pants`), all textured, which is the exact shape of the case this
    /// exists for: a character whose skin and clothes are separate images that a
    /// game wants to swap at runtime.
    #[test]
    fn a_models_own_textures_come_out_as_files() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let model = repo.join("assets/models/_test/UVMappedR6.glb");
        if !model.is_file() {
            return; // not a checkout with the test assets
        }
        let out = std::env::temp_dir().join(format!("floptle-extract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        floptle_vfs::create_dir_all(&out).unwrap();

        let written =
            extract_model_textures(&model, "models/avatar.glb", &out).expect("extraction");
        let mats: Vec<&str> = written.iter().map(|e| e.material.as_str()).collect();
        assert!(mats.contains(&"Clothing"), "{mats:?}");
        assert!(mats.contains(&"Head"), "{mats:?}");
        assert!(mats.contains(&"Pants"), "{mats:?}");
        for e in &written {
            let abs = out.join(&e.path);
            assert!(abs.is_file(), "{} was reported and not written", e.path);
            // A real PNG, not an empty file — the header is the cheap proof.
            let bytes = floptle_vfs::read(&abs).unwrap();
            assert!(bytes.len() > 100, "{} is {} bytes", e.path, bytes.len());
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{} is not a PNG", e.path);
            // Project-relative, which is how a material references a texture.
            assert!(!abs.to_string_lossy().is_empty() && !e.path.starts_with('/'));
        }
        // …and each is findable afterwards by name alone, which is what the
        // Inspector uses to seed an override with what the part already wore.
        assert_eq!(
            extracted_file(&out, "models/avatar.glb", "Clothing").as_deref(),
            Some("models/avatar_textures/Clothing.png")
        );
        assert_eq!(extracted_file(&out, "models/avatar.glb", "Nonesuch"), None);
    }

    /// A material name is an arbitrary string from a `.glb` and lands in a file
    /// name. `Body/Skin` must not write outside the folder, and `..` must not
    /// walk up out of the project.
    #[test]
    fn a_material_name_cannot_escape_its_folder() {
        assert_eq!(safe_stem("Body/Skin"), "Body_Skin");
        assert_eq!(safe_stem("../../etc/passwd"), "etc_passwd");
        assert_eq!(safe_stem(""), "material");
        assert_eq!(safe_stem("..."), "material");
        assert!(!extracted_path("models/hero.glb", "../x").contains(".."));
    }

    /// The folder is named after the model, beside it — two models that both
    /// call a material `Body` keep their own copy.
    #[test]
    fn extracted_textures_live_beside_their_model() {
        assert_eq!(extract_dir("models/hero.glb"), "models/hero_textures");
        assert_eq!(extract_dir("hero.glb"), "hero_textures");
        assert_eq!(
            extracted_path("models/hero.glb", "Clothing"),
            "models/hero_textures/Clothing.png"
        );
        assert_ne!(
            extracted_path("models/a.glb", "Body"),
            extracted_path("models/b.glb", "Body"),
            "two models must not share one file"
        );
    }
}
