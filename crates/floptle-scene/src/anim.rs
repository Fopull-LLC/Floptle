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

/// One property keyframe value: a number or a string (path/text).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum AnimPropValueDoc {
    Float(f32),
    Text(String),
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

use crate::SceneError;
use std::path::Path;

pub fn load_anim_clip(path: &Path) -> Result<AnimClipDoc, SceneError> {
    let text = std::fs::read_to_string(path).map_err(SceneError::Io)?;
    ron::from_str(&text).map_err(SceneError::Ron)
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
}
