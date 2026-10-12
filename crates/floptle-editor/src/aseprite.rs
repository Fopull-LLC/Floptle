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
//! **A packed or trimmed sheet is laid out again on a grid.** Aseprite can
//! pack frames tightly and trim their empty edges, and the engine's sheets are
//! grids, so the import says where each frame's pixels go in a grid of cells
//! the size of the untrimmed frame ([`Repack`]), and the caller draws that
//! grid as a new image. Only a rotated frame is refused, naming the checkbox.

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
    /// Set when the sheet is not a grid: how to draw one from it.
    pub(crate) repack: Option<Repack>,
}

/// How to redraw a packed or trimmed sheet as a grid: cell `i` of a
/// `cols × rows` grid of `cell_w × cell_h` cells is frame `i`, whose pixels are
/// `pieces[i]`.
#[derive(Debug, PartialEq)]
pub(crate) struct Repack {
    pub(crate) cell_w: u32,
    pub(crate) cell_h: u32,
    pub(crate) pieces: Vec<Piece>,
}

/// One frame's rectangle on the packed sheet, and where it sits in its cell.
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) struct Piece {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) dx: u32,
    pub(crate) dy: u32,
}

/// One clip from frames in play order, as `(cell, milliseconds)`.
///
/// The frame rate is the shortest frame, and everything longer becomes a
/// hold. Aseprite's timing is per frame in milliseconds and ours is a rate
/// plus holds; this is the conversion that keeps every frame's real duration
/// rather than averaging them into a rate that matches none.
pub(crate) fn clip(seq: &[(u32, u32)], looping: bool, cols: u32, rows: u32) -> SpriteAnimDoc {
    let base = seq.iter().map(|f| f.1).filter(|&m| m > 0).min().unwrap_or(100);
    SpriteAnimDoc {
        fps: 1000.0 / base as f32,
        looping,
        cols,
        rows,
        texture: String::new(), // filled in by the caller, which knows the path
        frames: seq
            .iter()
            .map(|&(cell, ms)| SpriteAnimFrameDoc {
                cell,
                hold: (ms.max(1) as f32 / base as f32).max(1.0),
                ..Default::default()
            })
            .collect(),
    }
}

/// The frames a tag plays, in order, and whether the clip loops. A tag that
/// repeats a set number of times plays that many rounds and then holds its
/// last frame; `0` repeats forever.
pub(crate) fn play_order(from: usize, to: usize, dir: crate::ase_file::Direction, repeat: u32) -> (Vec<usize>, bool) {
    let once = crate::ase_file::tag_frames(from, to, dir);
    if repeat == 0 {
        return (once, true);
    }
    let mut all: Vec<usize> = Vec::with_capacity(once.len() * repeat as usize);
    for _ in 0..repeat {
        all.extend(&once);
    }
    (all, false)
}

/// A tag's direction as the JSON spells it.
fn direction(s: &str) -> crate::ase_file::Direction {
    use crate::ase_file::Direction::*;
    match s {
        "reverse" => Reverse,
        "pingpong" => PingPong,
        "pingpong_reverse" => PingPongReverse,
        _ => Forward,
    }
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
    /// Where the rectangle sits in the untrimmed frame, and that frame's size.
    dx: u32,
    dy: u32,
    full_w: u32,
    full_h: u32,
    rotated: bool,
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
        // **Sorted by the frame number in the key, not by the key.** The object
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
        let (w, h) = (n("w")?, n("h")?);
        let num = |o: Option<&serde_json::Value>, k: &str| {
            o.and_then(|o| o.get(k)).and_then(|x| x.as_u64()).map(|x| x as u32)
        };
        let src = f.get("spriteSourceSize");
        let full = f.get("sourceSize");
        frames.push(Frame {
            x: n("x")?,
            y: n("y")?,
            w,
            h,
            dx: num(src, "x").unwrap_or(0),
            dy: num(src, "y").unwrap_or(0),
            full_w: num(full, "w").unwrap_or(w),
            full_h: num(full, "h").unwrap_or(h),
            rotated: f.get("rotated").and_then(|r| r.as_bool()).unwrap_or(false),
            // Aseprite always writes this; a sheet exported by something else
            // might not, and one frame rate for the lot is a fine answer.
            ms: f.get("duration").and_then(|d| d.as_u64()).unwrap_or(100) as u32,
        });
    }

    let (cw, ch) = (frames[0].w, frames[0].h);
    if cw == 0 || ch == 0 {
        return Err("this sheet's frames have no size".into());
    }
    if frames.iter().any(|f| f.rotated) {
        return Err(
            "this sheet has rotated frames, which cannot be drawn upright again. Re-export \
             with Allow Rotation unchecked in Aseprite's export dialog."
                .into(),
        );
    }
    let grid = frames.iter().all(|f| {
        f.w == cw && f.h == ch && f.x % cw == 0 && f.y % ch == 0 && f.dx == 0 && f.dy == 0
            && f.full_w == f.w && f.full_h == f.h
    });
    let repack = (!grid).then(|| Repack {
        cell_w: frames.iter().map(|f| f.full_w.max(f.dx + f.w)).max().unwrap_or(cw),
        cell_h: frames.iter().map(|f| f.full_h.max(f.dy + f.h)).max().unwrap_or(ch),
        pieces: frames
            .iter()
            .map(|f| Piece { x: f.x, y: f.y, w: f.w, h: f.h, dx: f.dx, dy: f.dy })
            .collect(),
    });

    let meta = v.get("meta");
    // **Only a plain relative name.** `Path::join` discards its base when the
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
    let (cols, rows) = if repack.is_some() {
        let n = frames.len() as u32;
        let cols = n.min(8);
        (cols, n.div_ceil(cols))
    } else {
        let cols = sheet_w.map(|w| w / cw).unwrap_or_else(|| {
            frames.iter().map(|f| f.x / cw + 1).max().unwrap_or(1)
        });
        let rows = sheet_h.map(|h| h / ch).unwrap_or_else(|| {
            frames.iter().map(|f| f.y / ch + 1).max().unwrap_or(1)
        });
        (cols.max(1), rows.max(1))
    };

    let cells: Vec<u32> = if repack.is_some() {
        (0..frames.len() as u32).collect()
    } else {
        frames.iter().map(|f| (f.y / ch) * cols + (f.x / cw)).collect()
    };

    // Tags name the animations. A sheet without any is still one clip — the
    // whole sheet, in order — because that is what a person who exported a
    // single loop has, and refusing it would mean the common simple case is the
    // one that needs a workaround.
    type Span = (String, usize, usize, crate::ase_file::Direction, u32);
    let tags: Vec<Span> = meta
        .and_then(|m| m.get("frameTags"))
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    // Aseprite writes `repeat` as a string, when it writes it.
                    let repeat = t
                        .get("repeat")
                        .and_then(|r| r.as_u64().or_else(|| r.as_str()?.parse().ok()))
                        .unwrap_or(0) as u32;
                    Some((
                        t.get("name")?.as_str()?.to_string(),
                        t.get("from")?.as_u64()? as usize,
                        t.get("to")?.as_u64()? as usize,
                        direction(t.get("direction").and_then(|d| d.as_str()).unwrap_or("forward")),
                        repeat,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let spans: Vec<Span> = if tags.is_empty() {
        vec![(stem.to_string(), 0, frames.len() - 1, crate::ase_file::Direction::Forward, 0)]
    } else {
        tags
    };

    let mut clips = Vec::new();
    for (name, from, to, dir, repeat) in spans {
        // **A tag naming a frame the sheet does not have is refused, not
        // trimmed.** It means the JSON and the PNG came from different exports,
        // and clamping hands back a clip that is quietly short — which reads as
        // the animation having lost frames and sends the search to the
        // renderer. Say which tag and what it asked for.
        if from >= frames.len() || to >= frames.len() {
            return Err(format!(
                "the tag \"{name}\" names frames {from} to {to}, but this sheet only has \
                 {} of them. The .json and the .png are from different exports \u{2014} \
                 re-export both together.",
                frames.len(),
            ));
        }
        if to < from {
            continue;
        }
        let (order, looping) = play_order(from, to, dir, repeat);
        let seq: Vec<(u32, u32)> = order.iter().map(|&i| (cells[i], frames[i].ms)).collect();
        clips.push((name, clip(&seq, looping, cols, rows)));
    }
    if clips.is_empty() {
        return Err("this sheet's tags name no frames".into());
    }
    Ok(Import { image, cols, rows, clips, repack })
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

    /// A packed, trimmed sheet comes back with where each frame's pixels go in
    /// a grid, and its clips index that grid. It used to be refused.
    #[test]
    fn a_packed_trimmed_sheet_is_laid_out_on_a_grid() {
        let packed = r#"{
          "frames": [
            { "frame": { "x": 0, "y": 0, "w": 16, "h": 16 }, "duration": 100,
              "spriteSourceSize": { "x": 0, "y": 0, "w": 16, "h": 16 }, "sourceSize": { "w": 16, "h": 16 } },
            { "frame": { "x": 17, "y": 0, "w": 12, "h": 10 }, "duration": 100,
              "spriteSourceSize": { "x": 2, "y": 6, "w": 12, "h": 10 }, "sourceSize": { "w": 16, "h": 16 } }
          ],
          "meta": { "image": "a.png", "size": { "w": 34, "h": 16 },
                    "frameTags": [ { "name": "run", "from": 0, "to": 1, "direction": "reverse" } ] }
        }"#;
        let im = parse(packed, "a").unwrap();
        let r = im.repack.expect("a packed sheet must be repacked");
        assert_eq!((r.cell_w, r.cell_h, im.cols, im.rows), (16, 16, 2, 1));
        assert_eq!(r.pieces[1], Piece { x: 17, y: 0, w: 12, h: 10, dx: 2, dy: 6 });
        let run = &im.clips[0].1;
        assert_eq!(run.frames.iter().map(|f| f.cell).collect::<Vec<_>>(), [1, 0], "reverse plays back to front");
    }

    /// A rotated frame cannot be drawn upright again, and says which checkbox.
    #[test]
    fn a_rotated_sheet_says_what_to_change() {
        let rotated = r#"{ "frames": [ { "frame": { "x": 0, "y": 0, "w": 8, "h": 16 }, "rotated": true } ],
                           "meta": { "image": "a.png" } }"#;
        let e = parse(rotated, "a").unwrap_err();
        assert!(e.contains("Allow Rotation"), "{e}");
    }

    /// A tag that repeats twice plays two rounds and stops; ping-pong goes
    /// there and back without doubling its ends.
    #[test]
    fn tag_direction_and_repeat_shape_the_clip() {
        let json = TAGGED.replace(
            r#"{ "name": "walk", "from": 0, "to": 2 }"#,
            r#"{ "name": "walk", "from": 0, "to": 2, "direction": "pingpong", "repeat": "2" }"#,
        );
        let im = parse(&json, "hero").unwrap();
        let walk = &im.clips[0].1;
        assert_eq!(walk.frames.iter().map(|f| f.cell).collect::<Vec<_>>(), [0, 1, 2, 1, 0, 1, 2, 1]);
        assert!(!walk.looping, "a tag that repeats a set number of times does not loop");
        assert!(im.clips[1].1.looping, "a tag with no repeat count loops");
    }

    /// Anything that is not an Aseprite sheet says so rather than producing an
    /// empty clip somebody then wonders about.
    #[test]
    fn something_that_is_not_a_sheet_says_so() {
        assert!(parse("{}", "x").unwrap_err().contains("no `frames`"));
        assert!(parse("not json", "x").unwrap_err().contains("not an Aseprite sheet"));
        assert!(parse(r#"{"frames":[]}"#, "x").unwrap_err().contains("no frames"));
    }
    /// **A tag naming frames the sheet does not have is refused.** It only
    /// happens when the `.json` and the `.png` came from different exports, and
    /// clamping it produced a clip that was silently short — the failure then
    /// looks like the animation losing frames, which is the wrong place to
    /// start looking.
    #[test]
    fn a_tag_past_the_end_of_the_sheet_is_refused() {
        let json = TAGGED.replace("\"to\": 2", "\"to\": 9");
        let err = parse(&json, "hero").expect_err("a tag past the end must not import");
        assert!(
            err.contains("walk"),
            "the message has to name the tag: {err}"
        );
        assert!(
            err.contains("4"),
            "the message has to say how many there are: {err}"
        );
        // The in-range file still imports — the guard is on out of range, not
        // on tags.
        assert!(parse(TAGGED, "hero").is_ok());
    }
}
