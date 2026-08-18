//! Animation asset DTOs (RON) — baked clips + layered controllers.
//!
//! Two asset kinds, discovered anywhere under `assets/` by extension so users
//! can organize them freely:
//!
//! - **`*.anim.ron`** — a baked [`AnimClipDoc`]: self-contained keyframe data,
//!   channels keyed by **node name**. Extracted from a model's embedded glTF
//!   clips (default home: `assets/animations/<Model>/`), or hand-authored in
//!   the Animating window. Name-binding makes a clip model-independent: it
//!   plays on any rig with matching node names, *and* on plain scene nodes
//!   (cutscenes — the controller's node + descendants are matched by their
//!   scene `Name`s).
//! - **`*.actl.ron`** — an [`AnimControllerDoc`]: prioritized layers of states
//!   (clip + speed/loop/instant/stepped-fps) with a crossfade table. Attached
//!   to a node via the AnimationController component; edited in the visual
//!   graph window.
//!
//! Asset **keys** are project-relative paths without the extension, e.g.
//! `animations/UVMappedR6/Walk`. Loaders fall back to matching the file stem
//! (`Walk`) when a key doesn't resolve, so moving a clip to another folder
//! degrades gracefully instead of silently breaking a controller.

use serde::{Deserialize, Serialize};

/// A baked, self-contained animation clip.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AnimClipDoc {
    pub name: String,
    pub duration: f32,
    /// The model asset this was extracted from (`""` = hand-authored).
    #[serde(default)]
    pub source_model: String,
    pub channels: Vec<AnimChannelDoc>,
    /// Timeline events: call a Lua function on the node's scripts.
    #[serde(default)]
    pub events: Vec<AnimEventDoc>,
}

/// All keyed lanes for one named node.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AnimChannelDoc {
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<AnimTrackDoc3>,
    /// Quaternion keys (xyzw).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<AnimTrackDoc4>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<AnimTrackDoc3>,
    /// Generic component-property lanes (opacity, colors, image swaps…). Empty
    /// for a plain transform clip; skipped in RON when so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<AnimPropTrackDoc>,
}

/// A keyed lane that animates one component field on the node. `value` keys are
/// either numbers or strings (a UI image path, a material texture…). `step`
/// holds each key with no blend — always the case for string lanes.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AnimPropTrackDoc {
    /// Component name (e.g. "UiElement", "PointLight", "Material").
    pub component: String,
    /// Field name (e.g. "opacity", "image", "intensity").
    pub field: String,
    pub times: Vec<f32>,
    pub values: Vec<AnimPropValueDoc>,
    #[serde(default)]
    pub step: bool,
}

/// One property keyframe value: a number, a string (path/text), or a whole
/// sprite frame.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum AnimPropValueDoc {
    Float(f32),
    Text(String),
    /// A [`SpriteFrameDoc`] — the sprite lane's key. Always stepped.
    Frame(SpriteFrameDoc),
}

/// One frame of a sprite animation: **which image, and which piece of it**.
///
/// A frame names its own art, which is the whole design. Animating a material's
/// `cell` was always possible and confined a clip to one sheet forever; here a
/// clip can walk across sheets, and pick up a plain PNG that was never on a
/// sheet at all, because each frame carries everything needed to draw it.
///
/// `cols`/`rows` default to `1`, so `(texture: "shout.png")` is the whole image.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SpriteFrameDoc {
    /// Project-relative image path.
    pub texture: String,
    /// How the image is cut. Omitted means `1` — the whole image.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub cols: u32,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub rows: u32,
    /// Which cell, row-major from the top-left.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cell: u32,
}

impl Default for SpriteFrameDoc {
    fn default() -> Self {
        Self { texture: String::new(), cols: 1, rows: 1, cell: 0 }
    }
}

fn one() -> u32 {
    1
}

fn is_one(n: &u32) -> bool {
    *n == 1
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Default for AnimPropValueDoc {
    fn default() -> Self {
        AnimPropValueDoc::Float(0.0)
    }
}

/// A keyed vec3 lane. `step = true` holds each key (no interpolation).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AnimTrackDoc3 {
    pub times: Vec<f32>,
    pub values: Vec<[f32; 3]>,
    #[serde(default)]
    pub step: bool,
}

/// A keyed quaternion lane (xyzw), slerped.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AnimTrackDoc4 {
    pub times: Vec<f32>,
    pub values: Vec<[f32; 4]>,
    #[serde(default)]
    pub step: bool,
}

/// A point on the clip's timeline that calls `func` on the node's scripts.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AnimEventDoc {
    pub t: f32,
    pub func: String,
}

/// A layered animation controller.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AnimControllerDoc {
    /// Crossfade seconds used when no per-transition override matches.
    #[serde(default = "default_fade")]
    pub default_fade: f32,
    /// Controller-wide stepped playback (frames/sec) for the retro choppy
    /// look; `None` = smooth. Individual states can override with their `fps`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_fps: Option<f32>,
    /// Priority stack: index 0 is the base; higher layers override the nodes
    /// their playing clip animates, scaled by the layer weight.
    pub layers: Vec<AnimLayerDoc>,
}

impl Default for AnimControllerDoc {
    fn default() -> Self {
        Self {
            default_fade: default_fade(),
            sample_fps: None,
            layers: vec![AnimLayerDoc {
                name: "Base".into(),
                weight: 1.0,
                states: Vec::new(),
                default_state: None,
                transitions: Vec::new(),
            }],
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AnimLayerDoc {
    pub name: String,
    /// Blend over the layers below (1 = full override).
    #[serde(default = "one_f32")]
    pub weight: f32,
    pub states: Vec<AnimStateDoc>,
    /// Auto-played on start (and returned to after one-shots finish).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_state: Option<String>,
    /// Per-pair crossfade overrides; anything else uses `default_fade`.
    #[serde(default)]
    pub transitions: Vec<AnimTransitionDoc>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AnimStateDoc {
    pub name: String,
    /// Clip asset key (`animations/UVMappedR6/Walk`).
    pub clip: String,
    #[serde(default = "one_f32")]
    pub speed: f32,
    #[serde(default = "true_bool")]
    pub looped: bool,
    /// Overrides the fade of EVERY transition into this state (seconds).
    /// `Some(0.0)` = always snap (instant); `None` = per-transition/default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_in: Option<f32>,
    /// Stepped-fps override for this state alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f32>,
    /// Node position in the controller graph editor.
    #[serde(default)]
    pub pos: [f32; 2],
}

/// One crossfade override: `from → to` in `fade` seconds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AnimTransitionDoc {
    pub from: String,
    pub to: String,
    pub fade: f32,
}

fn default_fade() -> f32 {
    0.25
}
fn one_f32() -> f32 {
    1.0
}
fn true_bool() -> bool {
    true
}

/// File extensions (as suffixes on the full file name).
pub const ANIM_CLIP_EXT: &str = ".anim.ron";
pub const ANIM_CTL_EXT: &str = ".actl.ron";
pub const SPRITE_ANIM_EXT: &str = ".spriteanim.ron";

/// A sprite clip written as **a list of frames** — `*.spriteanim.ron`.
///
/// The same thing a sprite lane in the timeline holds, in the shape a person
/// (or an importer) writes by hand: a frame rate and a list, rather than a time
/// per key. It exists because that is how sprite animation is *authored* —
/// twelve frames at twelve a second — while the timeline's shape is how
/// everything else in a clip is authored, and neither is wrong.
///
/// It loads **as an ordinary clip** ([`SpriteAnimDoc::to_clip`]), so a
/// `.spriteanim.ron` can go in a controller state, be blended between, carry
/// timeline events and be played from Lua exactly like an `.anim.ron`. One
/// animation system, two ways in.
///
/// ```ron
/// (
///   fps: 12,
///   loop: true,
///   cols: 8, rows: 4,                          // the sheet these frames cut
///   frames: [
///     (cell: 0),
///     (cell: 1),
///     (texture: "art/hero_extra.png", cell: 9),  // another sheet, mid-clip
///     (texture: "art/shout.png", cols: 1, rows: 1),  // a whole image
///     (cell: 2, hold: 3.0),                      // sits three frames long
///   ],
/// )
/// ```
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SpriteAnimDoc {
    /// Frames per second. A `hold` multiplies one frame's share of that.
    #[serde(default = "default_sprite_fps")]
    pub fps: f32,
    #[serde(rename = "loop", default = "yes")]
    pub looping: bool,
    /// The sheet every frame cuts unless it says otherwise. `1 × 1` — the
    /// default — means the frames are whole images.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub cols: u32,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub rows: u32,
    /// The image every frame uses unless it names its own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub texture: String,
    pub frames: Vec<SpriteAnimFrameDoc>,
}

/// One entry in a [`SpriteAnimDoc`]. Everything is optional: a frame that says
/// nothing is the clip's texture and grid at cell 0.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SpriteAnimFrameDoc {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub texture: String,
    /// This frame's own grid. `0` — the default — inherits the clip's, which is
    /// what almost every frame wants; a grid of zero is not a thing, so there is
    /// no value it could shadow. (RON writes `Some(4)` for an `Option`, and a
    /// file people edit by hand should not be asking them for that.)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cols: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rows: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cell: u32,
    /// How many frame-slots this one occupies. `1` (the default) is one slot.
    ///
    /// Here because a hand animator's timing is not a constant frame rate, and
    /// the alternative — repeating a frame four times — makes the list unread­able
    /// exactly where the timing is the interesting part.
    #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
    pub hold: f32,
}

fn default_sprite_fps() -> f32 {
    12.0
}

fn yes() -> bool {
    true
}

fn is_one_f32(x: &f32) -> bool {
    (*x - 1.0).abs() < f32::EPSILON
}

impl Default for SpriteAnimDoc {
    fn default() -> Self {
        Self {
            fps: default_sprite_fps(),
            looping: true,
            cols: 1,
            rows: 1,
            texture: String::new(),
            frames: Vec::new(),
        }
    }
}

impl SpriteAnimDoc {
    /// This clip as one stepped sprite lane on `node`.
    ///
    /// The duration runs to the **end** of the last frame, not to its start, so
    /// a four-frame clip at 12 fps lasts a third of a second and a loop does not
    /// eat the last frame — which is what keying at the frame times alone would
    /// do, and it reads as the clip being one frame short rather than as an
    /// off-by-one in the duration.
    pub fn to_clip(&self, name: &str, node: &str) -> AnimClipDoc {
        let fps = if self.fps.is_finite() && self.fps > 0.0 { self.fps } else { default_sprite_fps() };
        let mut times = Vec::with_capacity(self.frames.len());
        let mut values = Vec::with_capacity(self.frames.len());
        let mut t = 0.0f32;
        for f in &self.frames {
            times.push(t);
            values.push(AnimPropValueDoc::Frame(SpriteFrameDoc {
                texture: if f.texture.is_empty() { self.texture.clone() } else { f.texture.clone() },
                cols: if f.cols > 0 { f.cols } else { self.cols.max(1) },
                rows: if f.rows > 0 { f.rows } else { self.rows.max(1) },
                cell: f.cell,
            }));
            t += f.hold.max(0.0).max(f32::MIN_POSITIVE) / fps;
        }
        AnimClipDoc {
            name: name.to_string(),
            duration: t.max(1.0 / fps),
            source_model: String::new(),
            channels: vec![AnimChannelDoc {
                node: node.to_string(),
                properties: vec![AnimPropTrackDoc {
                    component: SPRITE_COMPONENT.into(),
                    field: SPRITE_FIELD.into(),
                    times,
                    values,
                    step: true,
                }],
                ..Default::default()
            }],
            events: Vec::new(),
        }
    }
}

/// What a sprite lane addresses. Not a real component — the applier writes a
/// material's texture, its sheet grid and the cell together, because those are
/// one thing to a person and keeping them four lanes is what confined a clip to
/// one sheet.
pub const SPRITE_COMPONENT: &str = "Sprite";
pub const SPRITE_FIELD: &str = "frame";

use crate::SceneError;
use std::path::Path;

pub fn load_anim_clip(path: &Path) -> Result<AnimClipDoc, SceneError> {
    let text = std::fs::read_to_string(path).map_err(SceneError::Io)?;
    ron::from_str(&text).map_err(SceneError::Ron)
}

pub fn load_sprite_anim(path: &Path) -> Result<SpriteAnimDoc, SceneError> {
    let text = std::fs::read_to_string(path).map_err(SceneError::Io)?;
    ron::from_str(&text).map_err(SceneError::Ron)
}

pub fn save_sprite_anim(doc: &SpriteAnimDoc, path: &Path) -> Result<(), SceneError> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = ron::ser::to_string_pretty(doc, ron::ser::PrettyConfig::default())
        .map_err(SceneError::Serialize)?;
    std::fs::write(path, text).map_err(SceneError::Io)
}

pub fn save_anim_clip(doc: &AnimClipDoc, path: &Path) -> Result<(), SceneError> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = ron::ser::to_string_pretty(doc, ron::ser::PrettyConfig::default())
        .map_err(SceneError::Serialize)?;
    std::fs::write(path, text).map_err(SceneError::Io)
}

pub fn load_anim_controller(path: &Path) -> Result<AnimControllerDoc, SceneError> {
    let text = std::fs::read_to_string(path).map_err(SceneError::Io)?;
    ron::from_str(&text).map_err(SceneError::Ron)
}

pub fn save_anim_controller(doc: &AnimControllerDoc, path: &Path) -> Result<(), SceneError> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = ron::ser::to_string_pretty(doc, ron::ser::PrettyConfig::default())
        .map_err(SceneError::Serialize)?;
    std::fs::write(path, text).map_err(SceneError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_doc_round_trips() {
        let doc = AnimClipDoc {
            name: "Walk".into(),
            duration: 1.5,
            source_model: "models/_test/UVMappedR6.glb".into(),
            channels: vec![AnimChannelDoc {
                node: "Torso".into(),
                translation: Some(AnimTrackDoc3 {
                    times: vec![0.0, 1.5],
                    values: vec![[0.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                    step: false,
                }),
                rotation: Some(AnimTrackDoc4 {
                    times: vec![0.0],
                    values: vec![[0.0, 0.0, 0.0, 1.0]],
                    step: true,
                }),
                scale: None,
                // A numeric lane + a stepped image-swap lane.
                properties: vec![
                    AnimPropTrackDoc {
                        component: "UiElement".into(),
                        field: "opacity".into(),
                        times: vec![0.0, 1.5],
                        values: vec![AnimPropValueDoc::Float(0.0), AnimPropValueDoc::Float(1.0)],
                        step: false,
                    },
                    AnimPropTrackDoc {
                        component: "UiElement".into(),
                        field: "image".into(),
                        times: vec![0.0, 0.5],
                        values: vec![
                            AnimPropValueDoc::Text("textures/a.png".into()),
                            AnimPropValueDoc::Text("textures/b.png".into()),
                        ],
                        step: true,
                    },
                ],
            }],
            events: vec![AnimEventDoc { t: 0.7, func: "onFootstep".into() }],
        };
        let text = ron::ser::to_string_pretty(&doc, Default::default()).unwrap();
        let back: AnimClipDoc = ron::from_str(&text).unwrap();
        assert_eq!(doc, back);
        // A transform-only channel omits `properties` in RON (serde skip).
        let plain = AnimChannelDoc { node: "Leg".into(), ..Default::default() };
        let ptext = ron::ser::to_string_pretty(&plain, Default::default()).unwrap();
        assert!(!ptext.contains("properties"), "empty properties must not serialize");
    }

    #[test]
    fn controller_doc_round_trips() {
        let doc = AnimControllerDoc {
            default_fade: 0.5,
            sample_fps: Some(12.0),
            layers: vec![
                AnimLayerDoc {
                    name: "Movement".into(),
                    weight: 1.0,
                    states: vec![
                        AnimStateDoc {
                            name: "Idle".into(),
                            clip: "animations/UVMappedR6/Idle".into(),
                            speed: 1.0,
                            looped: true,
                            fade_in: None,
                            fps: None,
                            pos: [40.0, 40.0],
                        },
                        AnimStateDoc {
                            name: "Attack".into(),
                            clip: "animations/UVMappedR6/DashForwards".into(),
                            speed: 1.3,
                            looped: false,
                            fade_in: Some(0.0),
                            fps: Some(8.0),
                            pos: [240.0, 40.0],
                        },
                    ],
                    default_state: Some("Idle".into()),
                    transitions: vec![AnimTransitionDoc {
                        from: "Attack".into(),
                        to: "Idle".into(),
                        fade: 0.1,
                    }],
                },
                AnimLayerDoc {
                    name: "Overlay".into(),
                    weight: 0.75,
                    states: Vec::new(),
                    default_state: None,
                    transitions: Vec::new(),
                },
            ],
        };
        let text = ron::ser::to_string_pretty(&doc, Default::default()).unwrap();
        let back: AnimControllerDoc = ron::from_str(&text).unwrap();
        assert_eq!(doc, back);
    }

    /// The shape a person writes: a texture, a grid, and a list of cells.
    ///
    /// Written as text rather than built as a struct, because the point of the
    /// asset is that it is readable and editable by hand — a test that only
    /// constructs the type would not notice the file becoming unwritable.
    #[test]
    fn a_sprite_animation_reads_the_way_it_is_meant_to_be_written() {
        let doc: SpriteAnimDoc = ron::from_str(
            r#"(
                fps: 10,
                loop: false,
                cols: 8, rows: 4,
                texture: "art/hero.png",
                frames: [
                    (cell: 0),
                    (cell: 1),
                    (texture: "art/extra.png", cols: 4, rows: 4, cell: 9),
                    (texture: "art/shout.png", cols: 1, rows: 1),
                    (cell: 2, hold: 3.0),
                ],
            )"#,
        )
        .expect("a hand-written sprite animation should parse");
        assert_eq!(doc.fps, 10.0);
        assert!(!doc.looping);

        let clip = doc.to_clip("Walk", "");
        let lane = &clip.channels[0].properties[0];
        assert_eq!(lane.component, SPRITE_COMPONENT);
        assert_eq!(lane.field, SPRITE_FIELD);
        assert!(lane.step, "a sprite lane that could lerp would play every cell in between");

        let frame = |i: usize| match &lane.values[i] {
            AnimPropValueDoc::Frame(f) => f.clone(),
            other => panic!("frame {i} is not a frame: {other:?}"),
        };
        // A bare frame takes the clip's texture and grid…
        assert_eq!(frame(0), SpriteFrameDoc { texture: "art/hero.png".into(), cols: 8, rows: 4, cell: 0 });
        // …and one that names its own overrides both, mid-clip. This is the
        // whole point: a clip is not confined to one sheet.
        assert_eq!(frame(2), SpriteFrameDoc { texture: "art/extra.png".into(), cols: 4, rows: 4, cell: 9 });
        // A whole image is a 1x1 grid — the same rule, not a second one.
        assert_eq!(frame(3), SpriteFrameDoc { texture: "art/shout.png".into(), cols: 1, rows: 1, cell: 0 });

        // Times: 10 fps is a tenth of a second a frame, and the `hold` on the
        // last one makes it three.
        assert_eq!(lane.times, vec![0.0, 0.1, 0.2, 0.3, 0.4]);
        // The duration runs to the END of the last frame. Stopping at its START
        // would drop it from every loop and read as the clip being one short.
        assert!((clip.duration - 0.7).abs() < 1e-5, "duration was {}", clip.duration);
    }

    /// The defaults are the common case, so they must not be written out.
    #[test]
    fn a_plain_sprite_animation_writes_only_what_it_says() {
        let doc = SpriteAnimDoc {
            texture: "art/hero.png".into(),
            cols: 4,
            rows: 1,
            frames: vec![
                SpriteAnimFrameDoc { cell: 0, ..Default::default() },
                SpriteAnimFrameDoc { cell: 1, hold: 2.0, ..Default::default() },
            ],
            ..Default::default()
        };
        let text = ron::ser::to_string_pretty(&doc, Default::default()).unwrap();
        assert!(!text.contains("rows"), "rows is 1 and should not be written:\n{text}");
        assert!(!text.contains("hold: 1"), "a hold of 1 is the default:\n{text}");
        assert!(text.contains("hold: 2"), "a real hold has to survive:\n{text}");
        let back: SpriteAnimDoc = ron::from_str(&text).unwrap();
        assert_eq!(doc, back);
    }

    /// An empty or nonsense frame rate must not produce a zero-length clip —
    /// a clip with no duration divides by nothing and holds frame one forever,
    /// which reads as the animation not playing.
    #[test]
    fn a_broken_frame_rate_falls_back_instead_of_collapsing() {
        for fps in [0.0, -4.0, f32::NAN] {
            let doc = SpriteAnimDoc {
                fps,
                frames: vec![SpriteAnimFrameDoc::default(); 3],
                ..Default::default()
            };
            let clip = doc.to_clip("X", "");
            assert!(clip.duration > 0.0, "fps {fps} gave a zero-length clip");
            assert_eq!(clip.channels[0].properties[0].times.len(), 3);
        }
    }
}
