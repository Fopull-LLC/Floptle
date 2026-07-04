//! Particle effect asset DTOs (RON) — the on-disk form of `floptle-vfx`'s
//! authoring model (`docs/particle-system-proposal.md`).
//!
//! One effect per **`*.vfx.ron`** file, discovered anywhere under `assets/` by
//! extension (the `.anim.ron` discipline). Asset keys are project-relative paths
//! without the extension (`vfx/360Slash`). Every field past `name` has a serde
//! default so the format can grow without breaking older files.
//!
//! Doc ↔ runtime conversion lives editor/runtime-side (the anim precedent) —
//! this module is pure data + load/save.

use serde::{Deserialize, Serialize};

/// A keyed value: scalar, vector, or color (rgba).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum VfxValueDoc {
    F32(f32),
    Vec3([f32; 3]),
    Rgba([f32; 4]),
}

/// How a key reaches the next one.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VfxInterpDoc {
    Constant,
    #[default]
    Linear,
    Bezier,
}

/// What a curve returns outside its keyed range.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VfxExtrapolateDoc {
    #[default]
    Clamp,
    Repeat,
}

/// One drawn node on a curve. Life curves key `t` in the particle's normalized
/// lifetime `[0,1]`; automation lanes key it in seconds along the timeline.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct VfxKeyDoc {
    pub t: f32,
    pub v: VfxValueDoc,
    #[serde(default)]
    pub interp: VfxInterpDoc,
    #[serde(default)]
    pub in_tan: f32,
    #[serde(default)]
    pub out_tan: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct VfxCurveDoc {
    pub keys: Vec<VfxKeyDoc>,
    #[serde(default)]
    pub extrapolate: VfxExtrapolateDoc,
}

/// A property: one constant, a per-particle random between two bounds, OR a drawn
/// curve — the value-or-curve union.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum VfxPropDoc {
    Const(VfxValueDoc),
    /// Uniform random per particle, resolved at birth and held for its life.
    Range(VfxValueDoc, VfxValueDoc),
    Curve(VfxCurveDoc),
}

/// How a track's particles are drawn.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum VfxRenderDoc {
    /// Camera-facing textured quad; `None` = plain tinted quad.
    Billboard {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        texture: Option<String>,
    },
    /// An instanced mesh (phase 4).
    Mesh { asset_path: String },
}

impl Default for VfxRenderDoc {
    fn default() -> Self {
        Self::Billboard { texture: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VfxBlendDoc {
    #[default]
    Alpha,
    Additive,
    Premultiplied,
    Screen,
    Multiply,
}

/// How a flipbook advances through its frames.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VfxFlipModeDoc {
    #[default]
    OverLife,
    LoopFps,
}

/// A sprite-sheet flipbook: the billboard texture is a `cols × rows` grid of frames.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct VfxFlipbookDoc {
    #[serde(default = "one_u32")]
    pub cols: u32,
    #[serde(default = "one_u32")]
    pub rows: u32,
    #[serde(default)]
    pub mode: VfxFlipModeDoc,
    #[serde(default = "twelve_f32")]
    pub fps: f32,
}

/// How a billboard quad is oriented in the world (billboard tracks only).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VfxOrientDoc {
    /// Always faces the camera (classic billboard) — the default, so old files keep
    /// their look.
    #[default]
    FaceCamera,
    /// Stretched along the particle's velocity (sprays, sparks, rain).
    Velocity,
    /// Upright, locked to the world up-axis, yawing to the camera (flames, grass).
    Vertical,
    /// Flat on the ground, normal pointing up (decals, shockwaves, ripples).
    Horizontal,
    /// Fixed to the particle's birth (emit-direction) orientation (debris, cards).
    WorldFixed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VfxSpaceDoc {
    #[default]
    Local,
    World,
}

/// Where particles are born (and the emit direction the velocity's +Y aligns to).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum VfxShapeDoc {
    #[default]
    Point,
    Cone {
        angle: f32,
        radius: f32,
    },
    Sphere {
        radius: f32,
        #[serde(default)]
        shell: bool,
    },
    Edge {
        length: f32,
    },
    Ring {
        radius: f32,
    },
}

/// A steady force field added to a track's particles (wind / attractor / vortex /
/// turbulence). Directions + centres are in the track's simulation space.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum VfxForceDoc {
    Directional { dir: [f32; 3], strength: f32 },
    Point { center: [f32; 3], strength: f32 },
    Vortex { center: [f32; 3], axis: [f32; 3], strength: f32 },
    Turbulence { frequency: f32, strength: f32 },
}

/// A ranged emission span on the timeline — the draggable clip.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct VfxClipDoc {
    pub start: f32,
    pub end: f32,
}

/// A hand-placed instant emit — the draggable diamond.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct VfxBurstDoc {
    pub t: f32,
    pub count: u32,
}

/// What an automation lane modulates (birth-domain parameters; see proposal §2).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfxLaneTargetDoc {
    Rate,
    Count,
    Speed,
    Size,
    Tint,
    ShapeScale,
}

/// A DAW-style automation lane: one curve over effect time (keys in seconds).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VfxLaneDoc {
    pub target: VfxLaneTargetDoc,
    pub curve: VfxCurveDoc,
}

/// One visual layer AND its timeline lane.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VfxTrackDoc {
    pub name: String,
    #[serde(default = "true_bool", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default)]
    pub render: VfxRenderDoc,
    #[serde(default)]
    pub blend: VfxBlendDoc,
    /// How a billboard quad is aligned in the world (billboard tracks only).
    #[serde(default, skip_serializing_if = "is_face_camera")]
    pub orient: VfxOrientDoc,
    /// Billboard width-to-height ratio (1 = square).
    #[serde(default = "one_f32", skip_serializing_if = "is_one")]
    pub aspect: f32,
    /// Velocity-orientation length multiplier (1 = neutral).
    #[serde(default = "one_f32", skip_serializing_if = "is_one")]
    pub stretch: f32,
    /// Sprite-sheet flipbook (None = a plain single-frame texture).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flipbook: Option<VfxFlipbookDoc>,
    /// Full scene lighting per particle (default off — classic crisp VFX).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lit: bool,
    /// The track's cloud casts field shadows via an aggregate proxy (default off).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cast_shadows: bool,
    #[serde(default)]
    pub space: VfxSpaceDoc,

    #[serde(default)]
    pub clips: Vec<VfxClipDoc>,
    #[serde(default)]
    pub bursts: Vec<VfxBurstDoc>,
    #[serde(default)]
    pub automation: Vec<VfxLaneDoc>,

    #[serde(default = "ten_f32")]
    pub rate: f32,
    #[serde(default)]
    pub shape: VfxShapeDoc,
    #[serde(default = "one_f32")]
    pub particle_lifetime: f32,
    #[serde(default)]
    pub lifetime_jitter: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_alive: Option<u32>,

    #[serde(default = "default_velocity")]
    pub velocity: VfxPropDoc,
    #[serde(default = "default_size")]
    pub size: VfxPropDoc,
    #[serde(default = "default_rotation")]
    pub rotation: VfxPropDoc,
    #[serde(default = "default_angular")]
    pub angular_velocity: VfxPropDoc,
    #[serde(default = "default_color")]
    pub color: VfxPropDoc,
    #[serde(default)]
    pub gravity: f32,
    #[serde(default)]
    pub drag: f32,
    /// Force fields added to velocity each step (default: none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forces: Vec<VfxForceDoc>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VfxPlaybackDoc {
    Looping,
    #[default]
    OneShot,
}

/// OneShot end behavior (hidden in the UI for `Looping`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VfxEndDoc {
    #[default]
    Destroy,
    Persist,
}

/// The reusable, named effect — a lifetime plus tracks on its timeline.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VfxEffectDoc {
    pub name: String,
    #[serde(default = "one_f32")]
    pub lifetime: f32,
    #[serde(default)]
    pub playback: VfxPlaybackDoc,
    #[serde(default)]
    pub end: VfxEndDoc,
    #[serde(default)]
    pub tracks: Vec<VfxTrackDoc>,
    #[serde(default = "one_u32")]
    pub seed: u32,
}

fn true_bool() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}
fn one_f32() -> f32 {
    1.0
}
fn is_one(v: &f32) -> bool {
    *v == 1.0
}
fn is_face_camera(o: &VfxOrientDoc) -> bool {
    matches!(o, VfxOrientDoc::FaceCamera)
}
fn ten_f32() -> f32 {
    10.0
}
fn twelve_f32() -> f32 {
    12.0
}
fn one_u32() -> u32 {
    1
}
fn default_velocity() -> VfxPropDoc {
    VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 1.0, 0.0]))
}
fn default_size() -> VfxPropDoc {
    VfxPropDoc::Const(VfxValueDoc::F32(0.25))
}
fn default_rotation() -> VfxPropDoc {
    VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 0.0, 0.0]))
}
fn default_angular() -> VfxPropDoc {
    VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 0.0, 0.0]))
}

/// Legacy migration: an old scalar rotation (billboard spin) becomes the Vec3 Euler
/// form with the value on Z (roll), so pre-Vec3 effects keep spinning as before.
fn upgrade_rotation(prop: &mut VfxPropDoc) {
    fn up(v: &mut VfxValueDoc) {
        if let VfxValueDoc::F32(x) = *v {
            *v = VfxValueDoc::Vec3([0.0, 0.0, x]);
        }
    }
    match prop {
        VfxPropDoc::Const(v) => up(v),
        VfxPropDoc::Range(a, b) => {
            up(a);
            up(b);
        }
        VfxPropDoc::Curve(c) => c.keys.iter_mut().for_each(|k| up(&mut k.v)),
    }
}
fn default_color() -> VfxPropDoc {
    VfxPropDoc::Const(VfxValueDoc::Rgba([1.0, 1.0, 1.0, 1.0]))
}

/// File extension (as a suffix on the full file name).
pub const VFX_EXT: &str = ".vfx.ron";

use crate::SceneError;
use std::path::Path;

pub fn load_vfx_effect(path: &Path) -> Result<VfxEffectDoc, SceneError> {
    let text = std::fs::read_to_string(path).map_err(SceneError::Io)?;
    let mut doc: VfxEffectDoc = ron::from_str(&text).map_err(SceneError::Ron)?;
    for t in &mut doc.tracks {
        upgrade_rotation(&mut t.rotation); // legacy scalar spin → Vec3 Euler
    }
    Ok(doc)
}

pub fn save_vfx_effect(doc: &VfxEffectDoc, path: &Path) -> Result<(), SceneError> {
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
    fn effect_doc_round_trips() {
        let doc = VfxEffectDoc {
            name: "360Slash".into(),
            lifetime: 0.6,
            playback: VfxPlaybackDoc::OneShot,
            end: VfxEndDoc::Destroy,
            seed: 7,
            tracks: vec![VfxTrackDoc {
                name: "Crescents".into(),
                enabled: true,
                render: VfxRenderDoc::Billboard { texture: Some("vfx/crescent.png".into()) },
                blend: VfxBlendDoc::Additive,
                orient: VfxOrientDoc::Velocity,
                aspect: 0.5,
                stretch: 2.0,
                flipbook: Some(VfxFlipbookDoc {
                    cols: 4,
                    rows: 4,
                    mode: VfxFlipModeDoc::LoopFps,
                    fps: 24.0,
                }),
                lit: false,
                cast_shadows: false,
                space: VfxSpaceDoc::Local,
                clips: vec![VfxClipDoc { start: 0.0, end: 0.4 }],
                bursts: vec![VfxBurstDoc { t: 0.12, count: 12 }],
                automation: vec![VfxLaneDoc {
                    target: VfxLaneTargetDoc::Rate,
                    curve: VfxCurveDoc {
                        keys: vec![
                            VfxKeyDoc {
                                t: 0.0,
                                v: VfxValueDoc::F32(1.0),
                                interp: VfxInterpDoc::Bezier,
                                in_tan: 0.0,
                                out_tan: -2.0,
                            },
                            VfxKeyDoc {
                                t: 0.6,
                                v: VfxValueDoc::F32(0.2),
                                interp: VfxInterpDoc::Linear,
                                in_tan: 0.0,
                                out_tan: 0.0,
                            },
                        ],
                        extrapolate: VfxExtrapolateDoc::Clamp,
                    },
                }],
                rate: 60.0,
                shape: VfxShapeDoc::Edge { length: 1.4 },
                particle_lifetime: 0.35,
                lifetime_jitter: 0.2,
                max_alive: Some(128),
                velocity: VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 9.0, 0.0])),
                size: VfxPropDoc::Curve(VfxCurveDoc {
                    keys: vec![
                        VfxKeyDoc {
                            t: 0.0,
                            v: VfxValueDoc::F32(0.2),
                            interp: VfxInterpDoc::Linear,
                            in_tan: 0.0,
                            out_tan: 0.0,
                        },
                        VfxKeyDoc {
                            t: 1.0,
                            v: VfxValueDoc::F32(0.0),
                            interp: VfxInterpDoc::Linear,
                            in_tan: 0.0,
                            out_tan: 0.0,
                        },
                    ],
                    extrapolate: VfxExtrapolateDoc::Clamp,
                }),
                rotation: default_rotation(),
                angular_velocity: default_angular(),
                color: default_color(),
                gravity: 0.5,
                drag: 0.1,
                forces: vec![
                    VfxForceDoc::Directional { dir: [1.0, 0.0, 0.0], strength: 2.0 },
                    VfxForceDoc::Turbulence { frequency: 0.5, strength: 1.5 },
                ],
            }],
        };
        let text = ron::ser::to_string_pretty(&doc, Default::default()).unwrap();
        let back: VfxEffectDoc = ron::from_str(&text).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn forces_default_empty_and_round_trip() {
        // Old files (no forces field) load with an empty force list…
        let doc: VfxEffectDoc =
            ron::from_str(r#"(name: "Old", tracks: [(name: "T")])"#).unwrap();
        assert!(doc.tracks[0].forces.is_empty());
        // …and every force variant round-trips.
        let f = vec![
            VfxForceDoc::Point { center: [0.0, 1.0, 0.0], strength: -3.0 },
            VfxForceDoc::Vortex { center: [0.0; 3], axis: [0.0, 1.0, 0.0], strength: 4.0 },
        ];
        let text = ron::ser::to_string_pretty(&f, Default::default()).unwrap();
        let back: Vec<VfxForceDoc> = ron::from_str(&text).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn range_prop_round_trips() {
        let p = VfxPropDoc::Range(VfxValueDoc::F32(0.1), VfxValueDoc::F32(0.5));
        let text = ron::ser::to_string_pretty(&p, Default::default()).unwrap();
        let back: VfxPropDoc = ron::from_str(&text).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn minimal_effect_parses_with_defaults() {
        // The wizard writes exactly this much; everything else must default.
        let doc: VfxEffectDoc =
            ron::from_str(r#"(name: "Campfire", lifetime: 2.0, playback: Looping)"#).unwrap();
        assert_eq!(doc.name, "Campfire");
        assert_eq!(doc.playback, VfxPlaybackDoc::Looping);
        assert!(doc.tracks.is_empty());
        assert_eq!(doc.seed, 1);
    }

    #[test]
    fn pre_orientation_track_defaults_to_face_camera_square() {
        // A track authored before orientation existed (no orient/aspect/stretch)
        // must load as a plain camera-facing square billboard — no visual change.
        let doc: VfxEffectDoc = ron::from_str(
            r#"(name: "Old", tracks: [(name: "T", render: Billboard(texture: Some("x.png")))])"#,
        )
        .unwrap();
        let t = &doc.tracks[0];
        assert_eq!(t.orient, VfxOrientDoc::FaceCamera);
        assert_eq!(t.aspect, 1.0);
        assert_eq!(t.stretch, 1.0);
    }

    #[test]
    fn default_orientation_fields_are_omitted_from_ron() {
        // The common case (face-camera, square) must not bloat the file.
        let doc = VfxEffectDoc {
            name: "Clean".into(),
            lifetime: 1.0,
            playback: VfxPlaybackDoc::OneShot,
            end: VfxEndDoc::Destroy,
            seed: 1,
            tracks: vec![VfxTrackDoc {
                orient: VfxOrientDoc::FaceCamera,
                aspect: 1.0,
                stretch: 1.0,
                ..minimal_track()
            }],
        };
        let text = ron::ser::to_string_pretty(&doc, Default::default()).unwrap();
        assert!(!text.contains("orient"), "default orient must be skipped");
        assert!(!text.contains("aspect"), "default aspect must be skipped");
        assert!(!text.contains("stretch"), "default stretch must be skipped");
    }

    /// A minimal track for tests that only care about a couple of fields.
    fn minimal_track() -> VfxTrackDoc {
        ron::from_str(r#"(name: "T")"#).unwrap()
    }
}
