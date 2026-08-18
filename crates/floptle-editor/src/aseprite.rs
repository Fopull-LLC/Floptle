//! **Importing an Aseprite sheet.**
//!
//! Aseprite's *Export Sprite Sheet* writes a `.png` and a `.json` beside it. The
//! PNG the engine could always read; the JSON is where the two things a person
//! actually did live — **how the sheet is cut**, and **which frames are which
//! animation** — and without it both have to be re-entered by hand against a
//! picture, which is where a pixel artist decides the engine is not worth it.
//!
//! What comes out is [`floptle_scene::SpriteAnimDoc`]s: one per tag, or one for
//! the whole sheet when there are no tags. Deliberately not a new asset type —
//! the import writes the same file somebody would have written, so a clip can be
//! edited, cut down or extended afterwards and nothing remembers it was
//! imported.
//!
//! **Only a uniform grid.** Aseprite can pack a sheet tightly, with per-frame
//! rectangles that are not a grid at all, and the engine's sheets are grids. A
//! packed sheet is refused with a sentence naming what to change in the export
//! dialog, rather than imported as a grid it is not — which would draw every
//! frame subtly wrong and look like a bug in the renderer.

use floptle_scene::{SpriteAnimDoc, SpriteAnimFrameDoc};

/// What one import produced.
#[derive(Debug)]
pub(crate) struct Import {
    /// The image the sheet's frames live on, as named by the JSON.
    pub(crate) image: String,
    /// The grid the sheet cuts into — what the texture's import settings want.
    pub(crate) cols: u32,
    pub(crate) rows: u32,
    /// `(clip name, clip)`. One per tag; one called after the file when there
    /// are no tags.
    pub(crate) clips: Vec<(String, SpriteAnimDoc)>,
}

/// The frame index Aseprite put in a frame's key.
///
/// The keys look like `"hero 12.aseprite"` or `"hero 12"`, so the number is the
/// last run of digits. Falling back to position when there is none keeps a
/// hand-written or third-party file working rather than sorting it to nothing.
fn frame_number(key: &str) -> Option<u64> {
    let end = key.rfind(|c: char| c.is_ascii_digit())? + 1;
    let start = key[..end].rfind(|c: char| !c.is_ascii_digit()).map(|i| i + 1).unwrap_or(0);
    key[start..end].parse().ok()
}

/// One frame as the JSON gives it.
struct Frame {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// Milliseconds. Aseprite writes this per frame, which is the whole reason
    /// `hold` exists on our side.
    ms: u32,
}

/// Read an Aseprite sheet JSON.
///
/// `stem` names the clip when the sheet carries no tags.
pub(crate) fn parse(text: &str, stem: &str) -> Result<Import, String> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("that is not an Aseprite sheet JSON: {e}"))?;

    // Aseprite writes `frames` as either an array or an object keyed by frame
    // name, depending on one checkbox in the export dialog. Both are ordinary,
    // so both are read rather than one being declared the right one.
    let frames_v = v.get("frames").ok_or("no `frames` in this JSON — is it an Aseprite export?")?;
    let raw: Vec<&serde_json::Value> = match frames_v {
        serde_json::Value::Array(a) => a.iter().collect(),
        // **Sorted by the frame NUMBER in the key, not by the key.** The object
        // form is keyed `"hero 0.aseprite"`, `"hero 1.aseprite"`, … and the map
        // this is parsed into is ordered lexicographically, which for twelve
        // frames is 0, 1, 10, 11, 2, 3 … — so every clip came out scrambled and
        // every tag's `from`/`to` then indexed into the scrambled list. Nothing
        // detects it: the frames are all still grid-aligned.
        serde_json::Value::Object(o) => {
            let mut rows: Vec<(u64, &serde_json::Value)> = o
                .iter()
                .enumerate()
                .map(|(i, (k, v))| (frame_number(k).unwrap_or(i as u64), v))
                .collect();
            rows.sort_by_key(|(n, _)| *n);
            rows.into_iter().map(|(_, v)| v).collect()
        }
        _ => return Err("`frames` is neither a list nor an object".into()),
    };
    if raw.is_empty() {
        return Err("this sheet has no frames".into());
    }

    let mut frames = Vec::with_capacity(raw.len());
    for f in &raw {
        let r = f.get("frame").ok_or("a frame has no rectangle")?;
        let n = |k: &str| -> Result<u32, String> {
            r.get(k)
                .and_then(|x| x.as_u64())
                .map(|x| x as u32)
                .ok_or_else(|| format!("a frame's rectangle has no `{k}`"))
        };
        frames.push(Frame {
            x: n("x")?,
            y: n("y")?,
            w: n("w")?,
            h: n("h")?,
            // Aseprite always writes this; a sheet exported by something else
            // might not, and one frame rate for the lot is a fine answer.
            ms: f.get("duration").and_then(|d| d.as_u64()).unwrap_or(100) as u32,
        });
    }

    let (cw, ch) = (frames[0].w, frames[0].h);
    if cw == 0 || ch == 0 {
        return Err("this sheet's frames have no size".into());
    }
    if frames.iter().any(|f| f.w != cw || f.h != ch) {
        return Err(
            "this sheet's frames are not all the same size, so it is not a grid. Re-export \
             with a constant sprite size (uncheck Trim in Aseprite's export dialog)."
                .into(),
        );
    }
    if frames.iter().any(|f| f.x % cw != 0 || f.y % ch != 0) {
        return Err(
            "this sheet's frames are packed rather than laid out on a grid. Re-export with \
             Sheet Type: By Rows or By Columns, and no padding or trimming."
                .into(),
        );
    }

    let meta = v.get("meta");
    // **Only a plain relative name.** `Path::join` DISCARDS its base when the
    // argument is absolute, and Aseprite's CLI export routinely writes an
    // absolute path here — so the clip would be written pointing at
    // `/home/whoever/art/hero.png`, which exists on the importing machine and
    // on nobody else's. A `..` is refused for the same reason: it would reach
    // outside the project and be written into an asset file.
    let image = meta
        .and_then(|m| m.get("image"))
        .and_then(|i| i.as_str())
        .map(|i| i.replace('\\', "/"))
        .filter(|i| {
            !i.is_empty()
                && !i.starts_with('/')
                && !i.split('/').any(|seg| seg == "..")
                && !i.contains(':')
        })
        .unwrap_or_default();
    // The sheet's own size when it says, else the extent of the frames — which
    // is the same number for every export that is actually a grid.
    let size = meta.and_then(|m| m.get("size"));
    let sheet_w = size.and_then(|s| s.get("w")).and_then(|x| x.as_u64()).map(|x| x as u32);
    let sheet_h = size.and_then(|s| s.get("h")).and_then(|x| x.as_u64()).map(|x| x as u32);
    let cols = sheet_w.map(|w| w / cw).unwrap_or_else(|| {
        frames.iter().map(|f| f.x / cw + 1).max().unwrap_or(1)
    });
    let rows = sheet_h.map(|h| h / ch).unwrap_or_else(|| {
        frames.iter().map(|f| f.y / ch + 1).max().unwrap_or(1)
    });
    let (cols, rows) = (cols.max(1), rows.max(1));

    let cell_of = |f: &Frame| (f.y / ch) * cols + (f.x / cw);

    // Tags name the animations. A sheet without any is still one clip — the
    // whole sheet, in order — because that is what a person who exported a
    // single loop has, and refusing it would mean the common simple case is the
    // one that needs a workaround.
    let tags: Vec<(String, usize, usize)> = meta
        .and_then(|m| m.get("frameTags"))
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    Some((
                        t.get("name")?.as_str()?.to_string(),
                        t.get("from")?.as_u64()? as usize,
                        t.get("to")?.as_u64()? as usize,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let spans: Vec<(String, usize, usize)> = if tags.is_empty() {
        vec![(stem.to_string(), 0, frames.len() - 1)]
    } else {
        tags
    };

    let mut clips = Vec::new();
    for (name, from, to) in spans {
        let (from, to) = (from.min(frames.len() - 1), to.min(frames.len() - 1));
        if to < from {
            continue;
        }
        let span = &frames[from..=to];
        // The frame rate is the SHORTEST frame, and everything longer becomes a
        // hold. Aseprite's timing is per frame in milliseconds and ours is a
        // rate plus holds; this is the conversion that keeps every frame's real
        // duration rather than averaging them into a rate that matches none.
        let base = span.iter().map(|f| f.ms).filter(|&m| m > 0).min().unwrap_or(100);
        clips.push((
            name,
            SpriteAnimDoc {
                fps: 1000.0 / base as f32,
                looping: true,
                cols,
                rows,
                texture: String::new(), // filled in by the caller, which knows the path
                frames: span
                    .iter()
                    .map(|f| SpriteAnimFrameDoc {
                        cell: cell_of(f),
                        hold: (f.ms.max(1) as f32 / base as f32).max(1.0),
                        ..Default::default()
                    })
                    .collect(),
            },
        ));
    }
    if clips.is_empty() {
        return Err("this sheet's tags name no frames".into());
    }
    Ok(Import { image, cols, rows, clips })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAGGED: &str = r#"{
      "frames": [
        { "frame": { "x": 0,  "y": 0,  "w": 16, "h": 16 }, "duration": 100 },
        { "frame": { "x": 16, "y": 0,  "w": 16, "h": 16 }, "duration": 100 },
        { "frame": { "x": 32, "y": 0,  "w": 16, "h": 16 }, "duration": 300 },
        { "frame": { "x": 0,  "y": 16, "w": 16, "h": 16 }, "duration": 100 }
      ],
      "meta": {
        "image": "hero.png",
        "size": { "w": 64, "h": 32 },
        "frameTags": [
          { "name": "walk", "from": 0, "to": 2 },
          { "name": "jump", "from": 3, "to": 3 }
        ]
      }
    }"#;

    #[test]
    fn a_tagged_sheet_becomes_one_clip_per_tag() {
        let im = parse(TAGGED, "hero").unwrap();
        assert_eq!(im.image, "hero.png");
        assert_eq!((im.cols, im.rows), (4, 2));
        let names: Vec<&str> = im.clips.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["walk", "jump"]);

        let walk = &im.clips[0].1;
        assert_eq!(walk.frames.len(), 3);
        // Row-major cells from the rectangles, which is the thing nobody wants
        // to work out by eye off a picture.
        assert_eq!(walk.frames.iter().map(|f| f.cell).collect::<Vec<_>>(), [0, 1, 2]);
        // The shortest frame sets the rate; the 300ms one becomes a hold of 3,
        // so every frame keeps its real duration.
        assert_eq!(walk.fps, 10.0);
        assert_eq!(walk.frames[2].hold, 3.0);
        assert_eq!(walk.frames[0].hold, 1.0);

        // A tag naming one frame is a clip of one — the idle pose, the hit
        // frame — and must not be dropped for being short.
        let jump = &im.clips[1].1;
        assert_eq!(jump.frames.len(), 1);
        assert_eq!(jump.frames[0].cell, 4, "second row, first column");
    }

    /// **Eleven frames is where the object form used to scramble.** Keys sort
    /// lexicographically, so `0, 1, 10, 2, …` — and the tags then index into
    /// that order, so every clip took the wrong frames. Two frames is the one
    /// count where lexicographic and numeric order agree, which is why the
    /// original test could not see it.
    #[test]
    fn the_object_form_is_ordered_by_frame_number_not_by_key() {
        let mut frames = String::new();
        for i in 0..12 {
            if i > 0 {
                frames.push(',');
            }
            frames.push_str(&format!(
                r#""hero {i}.aseprite": {{ "frame": {{ "x": {}, "y": 0, "w": 8, "h": 8 }}, "duration": 100 }}"#,
                i * 8
            ));
        }
        let src = format!(
            r#"{{ "frames": {{ {frames} }},
                 "meta": {{ "image": "hero.png", "size": {{ "w": 96, "h": 8 }},
                            "frameTags": [ {{ "name": "walk", "from": 0, "to": 3 }} ] }} }}"#
        );
        let im = parse(&src, "hero").unwrap();
        assert_eq!((im.cols, im.rows), (12, 1));
        let walk = &im.clips[0].1;
        assert_eq!(
            walk.frames.iter().map(|f| f.cell).collect::<Vec<_>>(),
            [0, 1, 2, 3],
            "the tag took the wrong frames — the object form was ordered by key"
        );
    }

    /// An absolute path in `meta.image` would be written verbatim into the clip
    /// and into the texture's import settings: correct on the machine that ran
    /// the import, broken for everyone who pulls the project.
    #[test]
    fn an_absolute_or_escaping_image_path_is_refused() {
        for bad in ["/home/artist/hero.png", "../../outside.png", "C:/art/hero.png"] {
            let src = format!(
                r#"{{ "frames": [ {{ "frame": {{ "x": 0, "y": 0, "w": 8, "h": 8 }} }} ],
                     "meta": {{ "image": "{bad}", "size": {{ "w": 8, "h": 8 }} }} }}"#
            );
            let im = parse(&src, "hero").unwrap();
            assert!(im.image.is_empty(), "{bad} was kept");
        }
    }

    /// Aseprite writes `frames` as an object when "Array" is unchecked. Both are
    /// ordinary exports, so both import.
    #[test]
    fn the_object_form_of_frames_imports_too() {
        let src = r#"{
          "frames": {
            "hero 0.ase": { "frame": { "x": 0, "y": 0, "w": 8, "h": 8 }, "duration": 50 },
            "hero 1.ase": { "frame": { "x": 8, "y": 0, "w": 8, "h": 8 }, "duration": 50 }
          },
          "meta": { "image": "hero.png", "size": { "w": 16, "h": 8 } }
        }"#;
        let im = parse(src, "hero").unwrap();
        assert_eq!((im.cols, im.rows), (2, 1));
        assert_eq!(im.clips.len(), 1);
        assert_eq!(im.clips[0].0, "hero", "an untagged sheet is one clip named after the file");
        assert_eq!(im.clips[0].1.fps, 20.0);
    }

    /// A packed or trimmed sheet is refused with the fix, not imported as a grid
    /// it is not — which would draw every frame slightly wrong and read as a
    /// renderer bug.
    #[test]
    fn a_packed_sheet_says_what_to_change() {
        let trimmed = r#"{
          "frames": [
            { "frame": { "x": 0, "y": 0, "w": 16, "h": 16 } },
            { "frame": { "x": 16, "y": 0, "w": 12, "h": 16 } }
          ],
          "meta": { "image": "a.png", "size": { "w": 32, "h": 16 } }
        }"#;
        let e = parse(trimmed, "a").unwrap_err();
        assert!(e.contains("same size") && e.contains("Trim"), "{e}");

        let packed = r#"{
          "frames": [
            { "frame": { "x": 0, "y": 0, "w": 16, "h": 16 } },
            { "frame": { "x": 17, "y": 0, "w": 16, "h": 16 } }
          ],
          "meta": { "image": "a.png", "size": { "w": 34, "h": 16 } }
        }"#;
        let e = parse(packed, "a").unwrap_err();
        assert!(e.contains("packed") && e.contains("By Rows"), "{e}");
    }

    /// Anything that is not an Aseprite sheet says so rather than producing an
    /// empty clip somebody then wonders about.
    #[test]
    fn something_that_is_not_a_sheet_says_so() {
        assert!(parse("{}", "x").unwrap_err().contains("no `frames`"));
        assert!(parse("not json", "x").unwrap_err().contains("not an Aseprite sheet"));
        assert!(parse(r#"{"frames":[]}"#, "x").unwrap_err().contains("no frames"));
    }
}
