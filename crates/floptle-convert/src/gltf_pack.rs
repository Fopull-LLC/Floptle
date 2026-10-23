//! **A loose `.gltf` into one `.glb`, verbatim.**
//!
//! Not a format conversion — it is already glTF — but it is the thing people
//! most often need and least expect to. A `.gltf` is JSON that *points at* its
//! geometry (`scene.bin`) and its textures (`textures/colour.png`), so it is
//! three to thirty files that have to travel together. Half of them go missing
//! in a zip, a Drive share, or a move between folders, and what is left opens
//! as an empty scene with no error worth reading.
//!
//! Packing it into a `.glb` puts all of it in one file. That is the whole point
//! of `.glb`, and it is why this sits beside the real converters rather than
//! being treated as a no-op.
//!
//! **Verbatim, not rebuilt.** The JSON is kept as written and only its
//! references change: every buffer is folded into the one binary chunk (each
//! buffer view moved by where its buffer landed) and every image file is
//! copied in beside them. Nodes, skins, animations, materials with all their
//! maps and texture transforms, samplers and their filters, and extensions
//! all come through untouched, because nothing here reads them in order to
//! write them out again.

use std::path::Path;

use serde_json::{json, Value};

use crate::textures::{self, Finder};
use crate::{ConvertError, MapSlot, MapUse, MissingMap, Report};

/// Pack `src` and its files into one `.glb`.
pub fn pack(src: &Path) -> Result<(Vec<u8>, Report), ConvertError> {
    let text = std::fs::read(src).map_err(|e| ConvertError::Io(e.to_string()))?;
    let mut root: Value = serde_json::from_slice(&text)
        .map_err(|e| ConvertError::Malformed(format!("it is not glTF JSON ({e})")))?;
    if !root["asset"].is_object() {
        return Err(ConvertError::Malformed("it has no glTF `asset` block".into()));
    }
    let mut report = Report::default();
    let mut finder = Finder::new(src);
    let dir = src.parent().unwrap_or(Path::new("."));

    // ---- every buffer into one ----
    let mut bin: Vec<u8> = Vec::new();
    let mut moved: Vec<usize> = Vec::new();
    for (i, b) in root["buffers"].as_array().cloned().unwrap_or_default().iter().enumerate() {
        let bytes = match b["uri"].as_str() {
            Some(uri) => read_uri(dir, uri).ok_or_else(|| {
                ConvertError::Malformed(format!(
                    "its geometry, `{}`, is not beside it — a .gltf has to travel with its .bin",
                    textures::short_name(uri)
                ))
            })?,
            None => {
                return Err(ConvertError::Malformed(format!("buffer {i} names no file")));
            }
        };
        while !bin.len().is_multiple_of(8) {
            bin.push(0);
        }
        moved.push(bin.len());
        bin.extend_from_slice(&bytes);
    }
    if let Some(views) = root["bufferViews"].as_array_mut() {
        for v in views {
            let b = v["buffer"].as_u64().unwrap_or(0) as usize;
            let off = v["byteOffset"].as_u64().unwrap_or(0) as usize + moved.get(b).copied().unwrap_or(0);
            v["buffer"] = json!(0);
            v["byteOffset"] = json!(off);
        }
    }

    // ---- every image file in beside them ----
    let image_count = root["images"].as_array().map_or(0, Vec::len);
    let mut missing_images: Vec<(usize, String)> = Vec::new();
    let mut image_names: Vec<String> = vec![String::new(); image_count];
    for i in 0..image_count {
        let Some(uri) = root["images"][i]["uri"].as_str().map(str::to_string) else {
            image_names[i] = "embedded".into();
            continue;
        };
        let name = if uri.starts_with("data:") { "embedded".to_string() } else { textures::short_name(&percent_decode(&uri)) };
        image_names[i] = name.clone();
        let found = if uri.starts_with("data:") {
            read_uri(dir, &uri)
        } else {
            let plain = percent_decode(&uri);
            finder.find(&[&plain]).and_then(|p| std::fs::read(p).ok())
        };
        let Some(mut bytes) = found else {
            missing_images.push((i, name));
            continue;
        };
        let mut mime = mime_of(&bytes);
        // glTF readers take PNG and JPEG; anything else that decodes is
        // turned into PNG rather than written as an image nothing can open.
        if mime.is_none()
            && let Some(px) = textures::decode(&bytes, &name)
            && let Some(png) = floptle_assets::encode_png(&px)
        {
            bytes = png;
            mime = Some("image/png");
        }
        let Some(mime) = mime else {
            report.warnings.push(format!("The texture `{name}` is in a format Floptle cannot read and was left out."));
            missing_images.push((i, name));
            continue;
        };
        while !bin.len().is_multiple_of(8) {
            bin.push(0);
        }
        let view = root["bufferViews"].as_array().map_or(0, Vec::len);
        if !root["bufferViews"].is_array() {
            root["bufferViews"] = json!([]);
        }
        if let Some(views) = root["bufferViews"].as_array_mut() {
            views.push(json!({ "buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len() }));
        }
        bin.extend_from_slice(&bytes);
        let img = &mut root["images"][i];
        if let Some(o) = img.as_object_mut() {
            o.remove("uri");
        }
        img["bufferView"] = json!(view);
        img["mimeType"] = json!(mime);
    }
    // An image that could not be found is replaced by one white pixel so every
    // index after it still points where it did, and no material samples it:
    // its references are taken out, so the slot falls back to the factor.
    for (i, _) in &missing_images {
        while !bin.len().is_multiple_of(8) {
            bin.push(0);
        }
        let px = floptle_render::TextureData { pixels: vec![255; 4], width: 1, height: 1 };
        let png = floptle_assets::encode_png(&px).unwrap_or_default();
        let view = root["bufferViews"].as_array().map_or(0, Vec::len);
        if let Some(views) = root["bufferViews"].as_array_mut() {
            views.push(json!({ "buffer": 0, "byteOffset": bin.len(), "byteLength": png.len() }));
        }
        bin.extend_from_slice(&png);
        let img = &mut root["images"][*i];
        if let Some(o) = img.as_object_mut() {
            o.remove("uri");
        }
        img["bufferView"] = json!(view);
        img["mimeType"] = json!("image/png");
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    root["buffers"] = json!([{ "byteLength": bin.len() }]);

    describe(&mut root, &image_names, &missing_images, &mut report);
    let glb = crate::stamp::join(&root, &bin);
    // The result has to be glTF a reader accepts, or this has only moved the
    // problem; and a pack with nothing to draw is no model.
    let gltf = gltf::Gltf::from_slice(&glb).map_err(|e| ConvertError::Malformed(e.to_string()))?;
    if gltf.document.meshes().len() == 0 {
        return Err(ConvertError::NoGeometry);
    }
    Ok((glb, report))
}

/// Fill `report` from a glTF document: its counts, the picture in each
/// material slot, its clips and skins. Material references to `missing`
/// images are taken out of `root`, and reported as missing.
pub(crate) fn describe(root: &mut Value, image_names: &[String], missing: &[(usize, String)], report: &mut Report) {
    let texture_image = |root: &Value, t: u64| root["textures"][t as usize]["source"].as_u64();
    let slots: [(&[&str], MapSlot); 5] = [
        (&["pbrMetallicRoughness", "baseColorTexture"], MapSlot::BaseColor),
        (&["pbrMetallicRoughness", "metallicRoughnessTexture"], MapSlot::Roughness),
        (&["normalTexture"], MapSlot::Normal),
        (&["occlusionTexture"], MapSlot::Occlusion),
        (&["emissiveTexture"], MapSlot::Emissive),
    ];
    let count = root["materials"].as_array().map_or(0, Vec::len);
    for mi in 0..count {
        let name = root["materials"][mi]["name"].as_str().unwrap_or("").to_string();
        for (path, slot) in slots {
            let info = path.iter().fold(&root["materials"][mi], |v, k| &v[*k]);
            let Some(t) = info["index"].as_u64() else { continue };
            let Some(img) = texture_image(root, t) else { continue };
            if let Some((_, n)) = missing.iter().find(|(i, _)| *i as u64 == img) {
                report.missing.push(MissingMap { material: name.clone(), slot, name: n.clone() });
                let (last, parent) = path.split_last().unwrap_or((&"", &[]));
                let holder = parent.iter().fold(&mut root["materials"][mi], |v, k| &mut v[*k]);
                if let Some(o) = holder.as_object_mut() {
                    o.remove(*last);
                }
                continue;
            }
            let texture = image_names.get(img as usize).cloned().unwrap_or_default();
            report.maps.push(MapUse { material: name.clone(), slot, texture: texture.clone(), note: None });
            if slot == MapSlot::Roughness {
                report.maps.push(MapUse { material: name.clone(), slot: MapSlot::Metallic, texture, note: Some("one picture, glTF's packing") });
            }
        }
    }
    report.materials = count;
    report.meshes = root["meshes"].as_array().map_or(0, |m| m.iter().map(|m| m["primitives"].as_array().map_or(0, Vec::len)).sum());
    report.textures = image_names.len() - missing.len();
    report.skins = root["skins"].as_array().map_or(0, Vec::len);
    for (i, a) in root["animations"].as_array().cloned().unwrap_or_default().iter().enumerate() {
        report.animations.push(a["name"].as_str().map(str::to_string).unwrap_or_else(|| format!("Clip{i}")));
    }
    // Triangles, from each primitive's index (or vertex) count.
    let acc = |i: &Value| i.as_u64().and_then(|i| root["accessors"][i as usize]["count"].as_u64());
    let mut tris = 0u64;
    for m in root["meshes"].as_array().cloned().unwrap_or_default() {
        for p in m["primitives"].as_array().cloned().unwrap_or_default() {
            let mode = p["mode"].as_u64().unwrap_or(4);
            if mode != 4 {
                continue;
            }
            tris += acc(&p["indices"]).or_else(|| acc(&p["attributes"]["POSITION"])).unwrap_or(0) / 3;
        }
    }
    report.triangles = tris as usize;
    for ext in root["extensionsRequired"].as_array().cloned().unwrap_or_default() {
        if let Some(e) = ext.as_str()
            && (e == "KHR_draco_mesh_compression" || e == "EXT_meshopt_compression")
        {
            report.warnings.push(format!(
                "The model is compressed with {e}, which Floptle cannot decode; re-export it without mesh compression."
            ));
        }
    }
}

/// The bytes a glTF URI names: a base64 data URI decoded, or a file beside
/// the model.
fn read_uri(dir: &Path, uri: &str) -> Option<Vec<u8>> {
    if let Some(rest) = uri.strip_prefix("data:") {
        let payload = rest.split_once(";base64,").map(|(_, b)| b)?;
        return base64_decode(payload);
    }
    std::fs::read(dir.join(percent_decode(uri))).ok()
}

/// A relative glTF URI is percent-encoded; file names with spaces and
/// non-ASCII letters arrive as `%20` and `%C3%A9`.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The glTF image type of encoded bytes, by their signature: the two every
/// reader takes.
fn mime_of(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else {
        None
    }
}
