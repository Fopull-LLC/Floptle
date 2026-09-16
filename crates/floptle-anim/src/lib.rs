//! # floptle-anim
//!
//! The animation runtime: node hierarchies ("skeletons"), sampled clips, and a
//! layered controller with crossfades, stepped-FPS ("choppy retro") playback,
//! and clip events. Pure data + math — no GPU, no serde, no Lua. The editor
//! binds `.anim.ron` / `.actl.ron` docs (floptle-scene) to these types and
//! drives [`Controller::advance`] each frame between scripts and physics.
//!
//! Design notes (docs/animation.md):
//! - Clips are **name-bound at the asset layer, index-bound here**: the binder
//!   resolves node names → indices once, so per-frame sampling does zero string
//!   work and a clip retargets to any rig with matching node names.
//! - Layers stack by priority: index 0 is the base; higher layers *override*
//!   the nodes their clips animate (scaled by the layer weight), so an
//!   arms-only attack overlays a running base without touching the legs.
//! - Stepped FPS quantizes the *sample* time only; real time keeps flowing, so
//!   events and transitions stay exactly on schedule and an instant transition
//!   lands on frame 0 of the target with no quantization delay.

use floptle_core::math::{Mat4, Quat, Vec3};
use std::collections::HashMap;

/// Local translation/rotation/scale — the node-space currency of the runtime.
/// `f32` (not `Transform`'s `f64`): animated nodes live near their model/rig
/// origin, so `f32` is exact enough and half the size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformTRS {
    pub t: Vec3,
    pub r: Quat,
    pub s: Vec3,
}

impl Default for TransformTRS {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl TransformTRS {
    pub const IDENTITY: Self = Self { t: Vec3::ZERO, r: Quat::IDENTITY, s: Vec3::ONE };

    pub fn matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.s, self.r, self.t)
    }

    /// Decompose an affine matrix back into TRS (used when re-parenting a node
    /// keeps it in place: `local = inv(new_parent_world) · own_world`).
    pub fn from_matrix(m: Mat4) -> Self {
        let (s, r, t) = m.to_scale_rotation_translation();
        Self { t, r: r.normalize(), s }
    }

    /// Blend `a → b` by `k` (lerp translation/scale, slerp rotation).
    pub fn blend(a: &Self, b: &Self, k: f32) -> Self {
        Self { t: a.t.lerp(b.t, k), r: a.r.slerp(b.r, k), s: a.s.lerp(b.s, k) }
    }

    /// Like [`Self::matrix`], but rotation and scale happen about `pivot` (a point
    /// in this node's local space) instead of the node origin: `T(t)·T(pivot)·R·S·
    /// T(-pivot)`. With `pivot = 0` this is exactly `matrix()`. This is what lets an
    /// object whose mesh sits far from its origin (an N64-style forearm baked in
    /// model space) rotate about a chosen joint — the pivot point stays put as the
    /// rotation changes.
    ///
    /// This form assumes the node's REST rotation/scale are identity (the baked
    /// object-model case it was written for). A node with a rotated rest — a mesh
    /// bone-parented in Blender carries the bone's axis flip — must use
    /// [`Self::matrix_about_rest`], or the pivot displaces it at rest.
    pub fn matrix_about(&self, pivot: Vec3) -> Mat4 {
        self.matrix_about_rest(pivot, &Self::IDENTITY)
    }

    /// Rotation/scale about `pivot`, anchored against the node's `rest` pose.
    ///
    /// The pivot is a point in the node's own local (mesh) space, so where it sits
    /// in parent space depends on the rest rotation/scale that map local → parent:
    /// `anchor = rest.r · (rest.s * pivot)`. Pivoting there gives
    /// `T(t + anchor)·R·S·T(-pivot)`, which has the two properties the feature
    /// promises:
    ///
    /// * **Rest is exact.** Feed the rest pose back in and the result is bit-for-bit
    ///   `rest.matrix()` — the pivot never moves a node that isn't being posed.
    /// * **The pivot point holds still.** Re-key the rotation and the pivot stays
    ///   where rest put it, so the object orbits its joint.
    ///
    /// [`Self::matrix_about`] is this with an identity rest: for the baked
    /// object models that case was written for, `anchor == pivot` and the two
    /// agree exactly.
    pub fn matrix_about_rest(&self, pivot: Vec3, rest: &Self) -> Mat4 {
        if pivot == Vec3::ZERO {
            return self.matrix();
        }
        let anchor = rest.r * (rest.s * pivot);
        let rs = Mat4::from_scale_rotation_translation(self.s, self.r, Vec3::ZERO);
        Mat4::from_translation(self.t + anchor) * rs * Mat4::from_translation(-pivot)
    }
}

/// One node of an animated hierarchy. Topologically sorted (parent index <
/// child index) so the world walk is one forward pass.
#[derive(Clone, Debug)]
pub struct SkelNode {
    pub name: String,
    pub parent: Option<usize>,
    /// Rest-pose local TRS — the fallback for nodes a clip doesn't animate.
    pub rest: TransformTRS,
    /// The point (this node's local space) that rotation/scale pivot around — the
    /// object's "joint". `ZERO` = the node origin (the default for rigged bones,
    /// whose origin already is the joint). Editor-authored for baked object models
    /// (see the `.rig.ron` pivot overrides); ignored by clips (it only reshapes how
    /// a pose composes, never what's keyed).
    pub pivot: Vec3,
}

impl SkelNode {
    /// The pivot expressed in parent space, relative to the node's translation:
    /// `rest.r · (rest.s * pivot)`. This is the offset [`TransformTRS::matrix_about_rest`]
    /// adds to the pose translation, so anything that reverses that composition —
    /// the gizmo turning a dragged world transform back into a local pose — must
    /// subtract this, not the raw pivot. Identity rest rotation/scale → the two
    /// are the same vector.
    pub fn pivot_anchor(&self) -> Vec3 {
        self.rest.r * (self.rest.s * self.pivot)
    }
}

/// A model's animated node hierarchy, shared by every instance of the model.
#[derive(Clone, Debug, Default)]
pub struct Skeleton {
    pub nodes: Vec<SkelNode>,
    name_to_node: HashMap<String, usize>,
}

impl Skeleton {
    /// Build from topologically-sorted nodes (parent < child).
    pub fn new(nodes: Vec<SkelNode>) -> Self {
        debug_assert!(nodes
            .iter()
            .enumerate()
            .all(|(i, n)| n.parent.is_none_or(|p| p < i)));
        let name_to_node =
            nodes.iter().enumerate().map(|(i, n)| (n.name.clone(), i)).collect();
        Self { nodes, name_to_node }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.name_to_node.get(name).copied()
    }

    pub fn rest_pose(&self) -> Vec<TransformTRS> {
        self.nodes.iter().map(|n| n.rest).collect()
    }

    /// Compose the local `pose` down the hierarchy into world (model-space)
    /// matrices. `out` is resized to fit; parent-first single pass.
    pub fn world_matrices(&self, pose: &[TransformTRS], out: &mut Vec<Mat4>) {
        out.clear();
        out.reserve(self.nodes.len());
        for (i, n) in self.nodes.iter().enumerate() {
            let local = pose.get(i).unwrap_or(&n.rest).matrix_about_rest(n.pivot, &n.rest);
            let m = match n.parent {
                Some(p) => out[p] * local,
                None => local,
            };
            out.push(m);
        }
    }
}

/// Captured skinning data for a deforming (vertex-weighted) mesh part — the
/// data layer for the future GPU vertex-skinning path. `joints[i]` indexes the
/// skeleton node that palette slot `i` binds to.
#[derive(Clone, Debug)]
pub struct Skin {
    pub joints: Vec<usize>,
    pub inverse_bind: Vec<Mat4>,
}

impl Skin {
    /// The GPU joint palette: `world[joint] * inverse_bind[joint]` per slot.
    pub fn skinning_matrices(&self, world: &[Mat4], out: &mut Vec<Mat4>) {
        out.clear();
        for (slot, &j) in self.joints.iter().enumerate() {
            let w = world.get(j).copied().unwrap_or(Mat4::IDENTITY);
            out.push(w * self.inverse_bind[slot]);
        }
    }
}

/// How a key reaches the next one. glTF cubic-spline channels are de-tangented
/// to `Linear` at import (Blender exports Linear for bones — the cold path).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Interp {
    /// Hold this key's value, then snap to the next.
    Step,
    #[default]
    Linear,
    /// A spline through the neighbouring keys, so the motion has no corners at
    /// the keys: the tangent at each key is set by the keys either side of it.
    Smooth,
    /// Start slowly, arrive at full speed.
    EaseIn,
    /// Leave at full speed, settle slowly.
    EaseOut,
    /// Start and settle slowly.
    EaseInOut,
}

impl Interp {
    /// A blend factor 0..1 remapped by the mode's easing. `Smooth` is not an
    /// easing — its shape comes from the neighbouring keys — and stays linear here.
    #[inline]
    pub fn ease(self, k: f32) -> f32 {
        match self {
            Interp::EaseIn => k * k * k,
            Interp::EaseOut => 1.0 - (1.0 - k).powi(3),
            Interp::EaseInOut => {
                if k < 0.5 {
                    4.0 * k * k * k
                } else {
                    1.0 - (-2.0 * k + 2.0).powi(3) * 0.5
                }
            }
            _ => k,
        }
    }
}

/// The cubic Hermite basis at `k`: weights for the start value, the start
/// tangent, the end value and the end tangent.
#[inline]
fn hermite(k: f32) -> [f32; 4] {
    let (k2, k3) = (k * k, k * k * k);
    [2.0 * k3 - 3.0 * k2 + 1.0, k3 - 2.0 * k2 + k, -2.0 * k3 + 3.0 * k2, k3 - k2]
}

/// One property lane: parallel `times`/`values`, binary-searched on sample.
#[derive(Clone, Debug)]
pub struct Track<T> {
    pub times: Vec<f32>,
    pub values: Vec<T>,
    /// The lane's interpolation, used by every key that does not override it.
    pub interp: Interp,
    /// **Per-key overrides**, parallel to `times`: how key `i` reaches the key
    /// after it. Shorter than `times` (usually empty) — a key with no entry uses
    /// the lane's `interp`.
    ///
    /// Per key because a lane-wide switch is the wrong unit for the thing people
    /// actually want. "Stepped animation" in hand-drawn work is not a clip that
    /// never interpolates: it is a clip that holds each drawing and snaps on the
    /// beats the animator chose, with the odd move eased through. A lane flag
    /// forces all-or-nothing, and the only way to get a hold in the middle of a
    /// smooth lane was to key the same value twice and hope nobody retimed it.
    ///
    /// Empty by default, and empty is exactly the old behaviour, so a clip that
    /// never touches this samples bit-for-bit as it did — and writes a file with
    /// no new fields in it.
    pub key_interp: Vec<Interp>,
}

impl<T> Track<T> {
    /// How key `i` reaches the next one.
    #[inline]
    pub fn interp_at(&self, i: usize) -> Interp {
        self.key_interp.get(i).copied().unwrap_or(self.interp)
    }
}

impl<T: Copy> Track<T> {
    /// The bracketing keys at `t`: (index a, index b, blend k in 0..1, mode).
    /// `k` is already eased; a `Smooth` segment is left for the sampler to
    /// shape from the neighbouring keys.
    fn bracket(&self, t: f32) -> Option<(usize, usize, f32, Interp)> {
        if self.times.is_empty() || self.values.len() != self.times.len() {
            return None;
        }
        let n = self.times.len();
        // partition_point = first index with times[i] > t.
        let hi = self.times.partition_point(|&k| k <= t);
        if hi == 0 {
            return Some((0, 0, 0.0, Interp::Linear));
        }
        if hi >= n {
            return Some((n - 1, n - 1, 0.0, Interp::Linear));
        }
        let (a, b) = (hi - 1, hi);
        let (ta, tb) = (self.times[a], self.times[b]);
        let k = if tb > ta { ((t - ta) / (tb - ta)).clamp(0.0, 1.0) } else { 0.0 };
        // The mode belongs to the key the playhead is LEAVING — the segment
        // after key `a` — which is the convention every keyframe editor uses and
        // the only one that lets a single hold be authored without touching the
        // key on the far side of it.
        let mode = self.interp_at(a);
        match mode {
            Interp::Step => Some((a, a, 0.0, mode)),
            _ => Some((a, b, mode.ease(k), mode)),
        }
    }

    /// The tangent at key `i` for a smooth segment: the finite difference
    /// across its neighbours, scaled to the segment `a → b`'s duration so it
    /// drops straight into the Hermite basis. One-sided at the ends.
    fn smooth_tangent(&self, i: usize, a: usize, b: usize, diff: impl Fn(T, T) -> T, scale: impl Fn(T, f32) -> T) -> T {
        let n = self.times.len();
        let (lo, hi) = (i.saturating_sub(1), (i + 1).min(n - 1));
        let span = (self.times[hi] - self.times[lo]).max(1e-6);
        let seg = self.times[b] - self.times[a];
        scale(diff(self.values[hi], self.values[lo]), seg / span)
    }
}

impl Track<Vec3> {
    pub fn sample(&self, t: f32) -> Option<Vec3> {
        let (a, b, k, mode) = self.bracket(t)?;
        if mode == Interp::Smooth && a != b {
            let ma = self.smooth_tangent(a, a, b, |x, y| x - y, |v, s| v * s);
            let mb = self.smooth_tangent(b, a, b, |x, y| x - y, |v, s| v * s);
            let h = hermite(k);
            return Some(self.values[a] * h[0] + ma * h[1] + self.values[b] * h[2] + mb * h[3]);
        }
        Some(self.values[a].lerp(self.values[b], k))
    }
}

impl Track<Quat> {
    pub fn sample(&self, t: f32) -> Option<Quat> {
        let (a, b, k, mode) = self.bracket(t)?;
        if mode == Interp::Smooth && a != b {
            // Squad: a slerp between the keys and a slerp between their
            // spline control points, blended by a parabola — the rotation
            // passes through each key with a continuous angular velocity.
            let n = self.times.len();
            let ctrl = |i: usize| squad_control(self.values[i.saturating_sub(1)], self.values[i], self.values[(i + 1).min(n - 1)]);
            let (qa, qb) = (self.values[a], self.values[b]);
            let (sa, sb) = (ctrl(a), ctrl(b));
            let outer = qa.slerp(qb, k);
            let inner = sa.slerp(sb, k);
            return Some(outer.slerp(inner, 2.0 * k * (1.0 - k)).normalize());
        }
        Some(self.values[a].slerp(self.values[b], k).normalize())
    }
}

impl Track<f32> {
    pub fn sample(&self, t: f32) -> Option<f32> {
        let (a, b, k, mode) = self.bracket(t)?;
        if mode == Interp::Smooth && a != b {
            let ma = self.smooth_tangent(a, a, b, |x, y| x - y, |v, s| v * s);
            let mb = self.smooth_tangent(b, a, b, |x, y| x - y, |v, s| v * s);
            let h = hermite(k);
            return Some(self.values[a] * h[0] + ma * h[1] + self.values[b] * h[2] + mb * h[3]);
        }
        Some(self.values[a] + (self.values[b] - self.values[a]) * k)
    }
}

/// The logarithm of a unit quaternion: the rotation as an axis scaled by half
/// its angle.
fn quat_log(q: Quat) -> Vec3 {
    let q = if q.w < 0.0 { -q } else { q };
    let v = Vec3::new(q.x, q.y, q.z);
    let len = v.length();
    if len < 1e-6 {
        return Vec3::ZERO;
    }
    v * (len.atan2(q.w) / len)
}

/// The inverse of [`quat_log`].
fn quat_exp(v: Vec3) -> Quat {
    let len = v.length();
    if len < 1e-6 {
        return Quat::IDENTITY;
    }
    let s = len.sin() / len;
    Quat::from_xyzw(v.x * s, v.y * s, v.z * s, len.cos())
}

/// The squad control point at `q` between its neighbours `prev` and `next`.
fn squad_control(prev: Quat, q: Quat, next: Quat) -> Quat {
    // Keep the neighbours in q's hemisphere so the logs measure the short way round.
    let prev = if prev.dot(q) < 0.0 { -prev } else { prev };
    let next = if next.dot(q) < 0.0 { -next } else { next };
    let inv = q.conjugate();
    let sum = quat_log(inv * next) + quat_log(inv * prev);
    (q * quat_exp(sum * -0.25)).normalize()
}

/// One **sprite frame**: which image, how that image is cut, and which piece.
///
/// The whole point of it being one value is that its four numbers cannot be
/// keyed apart. Sprite animation was always *possible* — point a material at a
/// sheet and animate its `cell` — but the texture and the sheet's grid were
/// separate lanes, so a clip could only ever live on **one sheet**, and getting
/// the two out of step gave you a cell index read against the wrong grid, which
/// draws a slice of the wrong picture rather than failing.
///
/// `cols`/`rows` of `1` (the default) is the whole image, which is what a
/// folder of exported PNGs gives you and what a lone `cell` could not express.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SpriteFrame {
    /// Project-relative image path.
    pub texture: String,
    /// How the image is cut. `1 × 1` is the whole image.
    pub cols: u32,
    pub rows: u32,
    /// Which cell, row-major from the top-left.
    pub cell: u32,
}

impl SpriteFrame {
    /// The whole of an image — no grid, no cell.
    pub fn whole(texture: impl Into<String>) -> Self {
        Self { texture: texture.into(), cols: 1, rows: 1, cell: 0 }
    }
}

/// A value a [`PropertyTrack`] keyframe can hold. Numbers cover the bulk of
/// animatable component fields (opacity, positions, colors, light intensity…);
/// text covers path-like fields; a frame is a whole sprite state at once.
#[derive(Clone, Debug, PartialEq)]
pub enum PropValue {
    Float(f32),
    Text(String),
    /// A sprite frame — always stepped, see [`SpriteFrame`].
    Frame(SpriteFrame),
}

/// A lane that animates one `(component, field)` on a node — the generic
/// property channel beside the fixed transform lanes. A lane can drive a
/// numeric field (lerp or step) or a string field like an image path (always
/// effectively stepped — you don't blend two textures).
#[derive(Clone, Debug)]
pub struct PropertyTrack {
    /// Component name as addressed by the ECS field applier ("UiElement",
    /// "PointLight", "Material"…).
    pub component: String,
    /// Field name ("opacity", "image", "intensity"…).
    pub field: String,
    /// Parallel to `values`, ascending.
    pub times: Vec<f32>,
    pub values: Vec<PropValue>,
    /// The lane's interpolation, used by every key that does not override it.
    pub interp: Interp,
    /// Per-key overrides, parallel to `times` — see [`Track::key_interp`].
    pub key_interp: Vec<Interp>,
}

impl PropertyTrack {
    /// How key `i` reaches the next one.
    #[inline]
    pub fn interp_at(&self, i: usize) -> Interp {
        self.key_interp.get(i).copied().unwrap_or(self.interp)
    }

    /// The value at `t` (holds the ends; step or lerp between keys). Text values
    /// — and any Step lane — hold the earlier key; numeric Linear lanes lerp.
    pub fn sample(&self, t: f32) -> Option<PropValue> {
        if self.times.is_empty() || self.values.len() != self.times.len() {
            return None;
        }
        let n = self.times.len();
        let hi = self.times.partition_point(|&k| k <= t);
        let a = hi.saturating_sub(1).min(n - 1);
        if hi == 0 || hi >= n || self.interp_at(a) == Interp::Step {
            return Some(self.values[a].clone());
        }
        let b = hi;
        let (ta, tb) = (self.times[a], self.times[b]);
        let mode = self.interp_at(a);
        let k = if tb > ta { mode.ease(((t - ta) / (tb - ta)).clamp(0.0, 1.0)) } else { 0.0 };
        match (&self.values[a], &self.values[b]) {
            (PropValue::Float(x), PropValue::Float(y)) => {
                if mode == Interp::Smooth {
                    // The spline through the neighbouring numeric keys.
                    let at = |i: usize| match &self.values[i] {
                        PropValue::Float(v) => *v,
                        _ => *x,
                    };
                    let (lo, hi2) = (a.saturating_sub(1), (b + 1).min(n - 1));
                    let seg = tb - ta;
                    let ma = (at(b) - at(lo)) / (self.times[b] - self.times[lo]).max(1e-6) * seg;
                    let mb = (at(hi2) - at(a)) / (self.times[hi2] - self.times[a]).max(1e-6) * seg;
                    let h = hermite(k);
                    return Some(PropValue::Float(x * h[0] + ma * h[1] + y * h[2] + mb * h[3]));
                }
                Some(PropValue::Float(x + (y - x) * k))
            }
            // Text, frames and mismatched pairs can't blend — hold the earlier
            // key. For a frame this is not a fallback but the only meaning it
            // has: half of one picture and half of the next is not a picture,
            // and blending the *cell indices* — the shape a naive version on
            // top of a numeric lane would take — plays every cell in between,
            // which reads as the clip running at the wrong speed, not as a bug.
            _ => Some(self.values[a].clone()),
        }
    }
}

/// One sampled property value, ready to apply: which node, which field, what to.
#[derive(Clone, Debug)]
pub struct PropSample {
    pub node: usize,
    pub component: String,
    pub field: String,
    pub value: PropValue,
}

/// All animated lanes for one skeleton node.
#[derive(Clone, Debug, Default)]
pub struct NodeChannels {
    pub node: usize,
    pub translation: Option<Track<Vec3>>,
    pub rotation: Option<Track<Quat>>,
    pub scale: Option<Track<Vec3>>,
    /// Generic property lanes (component fields, image swaps). Empty for the
    /// common transform-only clip.
    pub properties: Vec<PropertyTrack>,
}

/// A named point on a clip's timeline that calls a Lua function on the
/// controller's node when the playhead crosses it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipEvent {
    pub t: f32,
    pub func: String,
}

/// A sampled animation clip, bound to a specific skeleton (channels index
/// nodes directly — no per-frame name lookups).
#[derive(Clone, Debug, Default)]
pub struct Clip {
    pub name: String,
    pub duration: f32,
    pub channels: Vec<NodeChannels>,
    /// Sorted by `t`.
    pub events: Vec<ClipEvent>,
}

impl Clip {
    /// Write this clip's pose at `t` into `pose` (touches only animated nodes).
    pub fn sample_into(&self, t: f32, pose: &mut [TransformTRS]) {
        for ch in &self.channels {
            let Some(slot) = pose.get_mut(ch.node) else { continue };
            if let Some(v) = ch.translation.as_ref().and_then(|tr| tr.sample(t)) {
                slot.t = v;
            }
            if let Some(v) = ch.rotation.as_ref().and_then(|tr| tr.sample(t)) {
                slot.r = v;
            }
            if let Some(v) = ch.scale.as_ref().and_then(|tr| tr.sample(t)) {
                slot.s = v;
            }
        }
    }

    /// The set of node indices this clip animates (the layer's override mask).
    pub fn covered_nodes(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.channels.iter().map(|c| c.node).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Sample every property lane at `t`, appending the resolved values to
    /// `out`. Separate from `sample_into` because property values can't ride the
    /// transform pose (a texture path doesn't lerp) — they're applied directly
    /// to the ECS after the pose is composited.
    pub fn sample_properties(&self, t: f32, out: &mut Vec<PropSample>) {
        for ch in &self.channels {
            for pt in &ch.properties {
                if let Some(value) = pt.sample(t) {
                    out.push(PropSample {
                        node: ch.node,
                        component: pt.component.clone(),
                        field: pt.field.clone(),
                        value,
                    });
                }
            }
        }
    }

    /// True if any channel has a property lane (skip the sample when none do).
    pub fn has_properties(&self) -> bool {
        self.channels.iter().any(|c| !c.properties.is_empty())
    }
}

/// One state (a node in the controller graph): a clip plus how to play it.
#[derive(Clone, Debug)]
pub struct State {
    pub name: String,
    pub clip: Clip,
    pub speed: f32,
    pub looped: bool,
    /// Overrides the fade of every transition into this state (seconds).
    /// `Some(0.0)` = always snap; `None` = use the transition table / default.
    pub fade_in: Option<f32>,
    /// Stepped-playback override for this state (frames/sec); `None` falls
    /// back to the controller-wide `sample_fps`.
    pub fps: Option<f32>,
    /// Precomputed `clip.covered_nodes()`.
    covered: Vec<usize>,
}

impl State {
    pub fn new(name: String, clip: Clip) -> Self {
        let covered = clip.covered_nodes();
        Self { name, clip, speed: 1.0, looped: true, fade_in: None, fps: None, covered }
    }
}

/// A playing cursor into one state.
#[derive(Clone, Copy, Debug)]
struct Playback {
    state: usize,
    t: f32,
    /// A non-looped clip that reached its end (holds the last frame).
    finished: bool,
    /// The first `advance` after entering hasn't run yet — events at exactly
    /// t = 0 fire on that first tick (the crossing test is otherwise strict).
    fresh: bool,
}

impl Playback {
    fn enter(state: usize) -> Self {
        Self { state, t: 0.0, finished: false, fresh: true }
    }
}

/// What a crossfade blends *from*: the outgoing state (still advancing), or a
/// frozen pose snapshot (when a fade was interrupted mid-blend). A frozen
/// snapshot remembers which nodes it actually covers so a partial overlay
/// layer keeps overriding only its own nodes through the interrupted fade.
enum FadeFrom {
    Playback(Playback),
    Frozen { pose: Vec<TransformTRS>, covered: Vec<usize> },
}

/// An in-flight crossfade on a layer.
struct Fade {
    from: FadeFrom,
    t: f32,
    dur: f32,
}

/// One priority level of a controller: its own states + fade table + cursor.
pub struct Layer {
    pub name: String,
    pub states: Vec<State>,
    pub default_state: Option<usize>,
    /// `(from, to) → fade seconds`. Looked up before `default_fade`.
    pub fades: HashMap<(usize, usize), f32>,
    /// Blend of this layer over the ones below (1 = full override).
    pub weight: f32,
    cur: Option<Playback>,
    fade: Option<Fade>,
}

impl Layer {
    pub fn new(name: String, states: Vec<State>, default_state: Option<usize>) -> Self {
        Self {
            name,
            states,
            default_state,
            fades: HashMap::new(),
            weight: 1.0,
            cur: None,
            fade: None,
        }
    }

    pub fn state_index(&self, name: &str) -> Option<usize> {
        self.states.iter().position(|s| s.name == name)
    }

    /// The current state's (name, time, finished), if one is playing.
    pub fn current(&self) -> Option<(&str, f32, bool)> {
        self.cur.map(|p| (self.states[p.state].name.as_str(), p.t, p.finished))
    }

    fn fade_secs(&self, from: Option<usize>, to: usize, default_fade: f32) -> f32 {
        // A state-level fade-in override beats everything.
        if let Some(f) = self.states[to].fade_in {
            return f.max(0.0);
        }
        if let Some(f) = from
            && let Some(&d) = self.fades.get(&(f, to)) {
                return d;
            }
        default_fade
    }
}

/// A queued or reported event: which function to call on the node's scripts.
pub type FiredEvent = String;

/// One layer's replicable playback state (see [`Controller::net_state`]).
/// Both peers load the same controller asset, so a state INDEX + a time fully
/// describe what's playing — no strings or poses on the wire.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetLayerState {
    /// Playing state index within the layer (`None` = stopped/released).
    pub state: Option<u16>,
    /// Clip time in seconds.
    pub t: f32,
    /// Layer blend weight, 0..1.
    pub weight: f32,
    /// The playing clip's duration (send-side change prediction; 0 if stopped).
    pub dur: f32,
    /// Whether the playing state loops (send-side change prediction).
    pub looped: bool,
    /// Effective time rate of `t` (global speed × state speed) — how far the
    /// sender's clock moves per real second (send-side change prediction).
    pub rate: f32,
}

/// A whole controller's replicable state: global speed + per-layer playback.
#[derive(Clone, Debug, PartialEq)]
pub struct NetAnimState {
    pub speed: f32,
    pub layers: Vec<NetLayerState>,
}

/// The layered animation controller runtime for one node instance.
pub struct Controller {
    pub layers: Vec<Layer>,
    pub default_fade: f32,
    /// Controller-wide stepped playback (frames/sec); `None` = smooth.
    pub sample_fps: Option<f32>,
    rest: Vec<TransformTRS>,
    /// Final composited local pose (valid after `advance`).
    pose: Vec<TransformTRS>,
    scratch_a: Vec<TransformTRS>,
    scratch_b: Vec<TransformTRS>,
    fired: Vec<FiredEvent>,
    /// Global playback speed multiplier (Lua `setSpeed`).
    pub speed: f32,
}

impl Controller {
    pub fn new(rest: Vec<TransformTRS>, layers: Vec<Layer>, default_fade: f32) -> Self {
        Self {
            layers,
            default_fade,
            sample_fps: None,
            pose: rest.clone(),
            scratch_a: rest.clone(),
            scratch_b: rest.clone(),
            rest,
            fired: Vec::new(),
            speed: 1.0,
        }
    }

    /// The composited local pose from the last `advance`.
    pub fn pose(&self) -> &[TransformTRS] {
        &self.pose
    }

    /// Collect this frame's property-lane values across all layers. Properties
    /// don't blend, so each layer contributes its currently-playing state's
    /// values; lower layers first, higher (priority) layers last, so a later
    /// write wins — matching how transform layers override. Returns nothing for
    /// the common all-transform controller.
    pub fn sample_properties(&self) -> Vec<PropSample> {
        let mut out = Vec::new();
        for layer in &self.layers {
            if layer.weight <= 0.0 {
                continue;
            }
            // The current (or fade-incoming) playback; properties snap to it.
            if let Some(pb) = layer.cur {
                let clip = &layer.states[pb.state].clip;
                if clip.has_properties() {
                    clip.sample_properties(pb.t, &mut out);
                }
            }
        }
        out
    }

    /// True if any state's clip animates a property lane — lets the editor's
    /// preview skip its property snapshot for the common all-transform controller.
    pub fn has_properties(&self) -> bool {
        self.layers.iter().any(|l| l.states.iter().any(|s| s.clip.has_properties()))
    }

    /// Events fired since the last take (function names to call on the node).
    pub fn take_fired(&mut self) -> Vec<FiredEvent> {
        std::mem::take(&mut self.fired)
    }

    pub fn layer_index(&self, name: &str) -> Option<usize> {
        self.layers.iter().position(|l| l.name == name)
    }

    /// Find `state` by name — in `layer` if given, else the first layer that
    /// has it. Returns (layer index, state index).
    pub fn find_state(&self, state: &str, layer: Option<&str>) -> Option<(usize, usize)> {
        match layer {
            Some(l) => {
                let li = self.layer_index(l)?;
                Some((li, self.layers[li].state_index(state)?))
            }
            None => self
                .layers
                .iter()
                .enumerate()
                .find_map(|(li, l)| l.state_index(state).map(|si| (li, si))),
        }
    }

    /// Transition a layer to `state`. `fade_override` beats the fade table;
    /// the target's `instant` flag beats everything (always snaps).
    /// Re-requesting the state that's already playing is a no-op (unless it
    /// finished), so a script can call `play` every frame without freezing the
    /// blend — the crossfade-restart bug this runtime regression-tests.
    pub fn play(&mut self, layer: usize, state: usize, fade_override: Option<f32>) {
        self.transition(layer, state, fade_override, false);
    }

    /// Like [`Self::play`] but always restarts, even if `state` is current.
    pub fn restart(&mut self, layer: usize, state: usize, fade_override: Option<f32>) {
        self.transition(layer, state, fade_override, true);
    }

    fn transition(&mut self, layer: usize, state: usize, fade_override: Option<f32>, force: bool) {
        let default_fade = self.default_fade;
        let sample_fps = self.sample_fps;
        let rest = &self.rest;
        let Some(l) = self.layers.get_mut(layer) else { return };
        if state >= l.states.len() {
            return;
        }
        if let Some(cur) = l.cur
            && cur.state == state && !cur.finished && !force {
                return; // already playing — keep the blend advancing.
            }
        let mut fade_dur = if let Some(f) = l.states[state].fade_in {
            // The state's fade-in override beats even an explicit request —
            // fade_in = 0 gives guaranteed-instant states.
            f.max(0.0)
        } else {
            fade_override.unwrap_or_else(|| {
                l.fade_secs(l.cur.map(|c| c.state), state, default_fade)
            })
        };
        if l.cur.is_none() && l.fade.is_none() {
            fade_dur = 0.0; // nothing to fade from — start clean.
        }
        if fade_dur <= 0.0 {
            l.fade = None;
        } else if let Some(cur) = l.cur {
            // If a fade is already in flight, freeze its current blend as the
            // new fade source (never juggle 3 live states).
            let from = if let Some(old) = l.fade.take() {
                Self::freeze_blend(l, &old, Some(cur), sample_fps, rest)
            } else {
                FadeFrom::Playback(cur)
            };
            l.fade = Some(Fade { from, t: 0.0, dur: fade_dur });
        } else if let Some(old) = l.fade.take() {
            // The layer was fading out — keep fading from that snapshot into
            // the new state instead of snapping.
            let from = Self::freeze_blend(l, &old, None, sample_fps, rest);
            l.fade = Some(Fade { from, t: 0.0, dur: fade_dur });
        }
        l.cur = Some(Playback::enter(state));
    }

    /// Freeze what a layer is currently showing (an in-flight fade + optional
    /// current state) into a rest-seeded snapshot with its covered-node set.
    fn freeze_blend(
        l: &Layer,
        fade: &Fade,
        cur: Option<Playback>,
        sample_fps: Option<f32>,
        rest: &[TransformTRS],
    ) -> FadeFrom {
        let mut pose = rest.to_vec();
        let mut covered: Vec<usize> = Vec::new();
        match cur {
            Some(cur) => {
                Self::eval_layer_pose(l, fade, cur, sample_fps, &mut pose);
                covered.extend_from_slice(&l.states[cur.state].covered);
            }
            None => {
                // A fade-out with nothing incoming: sample the outgoing source
                // at its current (fading) strength.
                let k = 1.0 - smoothstep((fade.t / fade.dur.max(1e-6)).clamp(0.0, 1.0));
                match &fade.from {
                    FadeFrom::Playback(p) => {
                        let st = &l.states[p.state];
                        let tq = Self::quantize(p.t, st.fps.or(sample_fps));
                        let mut sampled = rest.to_vec();
                        st.clip.sample_into(tq, &mut sampled);
                        for (o, s) in pose.iter_mut().zip(sampled.iter()) {
                            *o = TransformTRS::blend(o, s, k);
                        }
                    }
                    FadeFrom::Frozen { pose: fp, .. } => {
                        for (o, s) in pose.iter_mut().zip(fp.iter()) {
                            *o = TransformTRS::blend(o, s, k);
                        }
                    }
                }
            }
        }
        match &fade.from {
            FadeFrom::Playback(p) => covered.extend_from_slice(&l.states[p.state].covered),
            FadeFrom::Frozen { covered: c, .. } => covered.extend_from_slice(c),
        }
        covered.sort_unstable();
        covered.dedup();
        FadeFrom::Frozen { pose, covered }
    }

    /// Stop a layer's playback. A layer with a default state returns to it
    /// (crossfading per the fade table / `fade`); a layer without one fades
    /// out and releases to the layers below.
    pub fn stop_layer(&mut self, layer: usize, fade: Option<f32>) {
        let (default_state, cur_state) = match self.layers.get(layer) {
            Some(l) => (l.default_state, l.cur.map(|c| c.state)),
            None => return,
        };
        if let Some(d) = default_state {
            if cur_state != Some(d) {
                self.transition(layer, d, fade, false);
            }
            return;
        }
        let sample_fps = self.sample_fps;
        let default_fade = self.default_fade;
        let rest = &self.rest;
        let Some(l) = self.layers.get_mut(layer) else { return };
        let Some(cur) = l.cur.take() else { return };
        let dur = fade.unwrap_or(default_fade);
        if dur > 0.0 {
            let from = if let Some(old) = l.fade.take() {
                Self::freeze_blend(l, &old, Some(cur), sample_fps, rest)
            } else {
                FadeFrom::Playback(cur)
            };
            l.fade = Some(Fade { from, t: 0.0, dur });
        } else {
            l.fade = None;
        }
    }

    pub fn set_layer_weight(&mut self, layer: usize, w: f32) {
        if let Some(l) = self.layers.get_mut(layer) {
            l.weight = w.clamp(0.0, 1.0);
        }
    }

    /// Seek the current state of `layer` to absolute time `t` (scrubbing).
    pub fn seek(&mut self, layer: usize, t: f32) {
        if let Some(l) = self.layers.get_mut(layer)
            && let Some(cur) = l.cur.as_mut() {
                let dur = l.states[cur.state].clip.duration.max(1e-6);
                cur.t = t.clamp(0.0, dur);
                cur.finished = false;
                l.fade = None;
            }
    }

    /// This controller's replicable playback state — what animator networking
    /// ships instead of poses: per layer, the playing state index, clip time,
    /// and blend weight (plus the global speed). Each peer runs the same
    /// controller asset, so an index and a time reproduce the pose locally.
    /// `dur`/`looped`/`rate` ride along for the SENDER's change detection (a
    /// looping clip's time is predictable, so it needn't be re-sent every
    /// snapshot); they're ignored on apply, which reads its own clips.
    pub fn net_state(&self) -> NetAnimState {
        NetAnimState {
            speed: self.speed,
            layers: self
                .layers
                .iter()
                .map(|l| NetLayerState {
                    state: l.cur.map(|c| c.state.min(u16::MAX as usize - 1) as u16),
                    t: l.cur.map(|c| c.t).unwrap_or(0.0),
                    weight: l.weight,
                    dur: l
                        .cur
                        .map(|c| l.states[c.state].clip.duration)
                        .unwrap_or(0.0),
                    looped: l.cur.map(|c| l.states[c.state].looped).unwrap_or(false),
                    rate: l
                        .cur
                        .map(|c| self.speed * l.states[c.state].speed)
                        .unwrap_or(0.0),
                })
                .collect(),
        }
    }

    /// Apply a replicated [`NetAnimState`] (a remote peer's authoritative
    /// animator) onto this controller:
    ///
    /// - a **state change** transitions through the same fade rules a local
    ///   `play()` would use, then lands mid-clip at the replicated time with
    ///   the fade preserved — a late joiner sees a walk loop blend in at the
    ///   right phase, not snap;
    /// - the **same state** only corrects time drift beyond
    ///   `drift_tolerance` seconds (wrap-aware for looping clips, so the
    ///   seam of a loop never reads as drift) — between updates the local
    ///   controller extrapolates by advancing normally;
    /// - out-of-range indices (mismatched controller assets between peers)
    ///   and extra layers are ignored, never panic.
    pub fn apply_net_state(&mut self, ns: &NetAnimState, drift_tolerance: f32) {
        self.speed = ns.speed;
        for (li, nl) in ns.layers.iter().enumerate() {
            if li >= self.layers.len() {
                break; // asset mismatch: fewer layers here — ignore the rest
            }
            self.layers[li].weight = nl.weight.clamp(0.0, 1.0);
            match nl.state {
                None => {
                    // Remotely released. (A layer with a default state never
                    // reports None — stop() transitions to the default — so
                    // this is the fade-out-and-release path.)
                    if self.layers[li].cur.is_some() {
                        self.stop_layer(li, None);
                    }
                }
                Some(si) => {
                    let si = si as usize;
                    let l = &self.layers[li];
                    if si >= l.states.len() {
                        continue; // asset mismatch: unknown state — ignore
                    }
                    let dur = l.states[si].clip.duration.max(1e-6);
                    let looped = l.states[si].looped;
                    let target_t = nl.t.clamp(0.0, dur);
                    match l.cur {
                        Some(c) if c.state == si => {
                            // Same state: drift-correct only. Distance is
                            // wrap-aware on loops (0.02 vs dur-0.01 is 30 ms
                            // apart, not a whole clip).
                            let lin = (c.t - target_t).abs();
                            let dist = if looped { lin.min(dur - lin) } else { lin };
                            if dist > drift_tolerance {
                                self.seek(li, target_t);
                            }
                        }
                        _ => {
                            // Transition with the state's own fade rules, then
                            // land at the replicated time KEEPING the fade
                            // (seek() would kill it — that's for scrubbing).
                            self.play(li, si, None);
                            if let Some(cur) = self.layers[li].cur.as_mut() {
                                cur.t = target_t;
                                cur.finished = !looped && target_t >= dur - 1e-4;
                                // Entry events already fired on the authority;
                                // a replica joining mid-clip must not re-fire
                                // the t=0 events.
                                cur.fresh = false;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Quantized sample time for the retro stepped look. Real time flows
    /// smoothly; only the *sampling* snaps to the frame grid, so transitions
    /// and events never drift out of sync.
    fn quantize(t: f32, fps: Option<f32>) -> f32 {
        match fps {
            Some(f) if f > 0.0 => (t * f).floor() / f,
            _ => t,
        }
    }

    /// Evaluate one layer's blended local pose into `out` (seeded with rest
    /// where unanimated). Static so `transition` can call it mid-borrow.
    fn eval_layer_pose(
        l: &Layer,
        fade: &Fade,
        cur: Playback,
        sample_fps: Option<f32>,
        out: &mut [TransformTRS],
    ) {
        let cs = &l.states[cur.state];
        let ct = Self::quantize(cur.t, cs.fps.or(sample_fps));
        match &fade.from {
            FadeFrom::Playback(p) => {
                let ps = &l.states[p.state];
                let pt = Self::quantize(p.t, ps.fps.or(sample_fps));
                // out already holds rest; layer 'from' first, then blend in cur.
                let mut b = out.to_vec();
                ps.clip.sample_into(pt, out);
                cs.clip.sample_into(ct, &mut b);
                let k = smoothstep((fade.t / fade.dur.max(1e-6)).clamp(0.0, 1.0));
                for (o, nb) in out.iter_mut().zip(b.iter()) {
                    *o = TransformTRS::blend(o, nb, k);
                }
            }
            FadeFrom::Frozen { pose: frozen, .. } => {
                let mut b = out.to_vec();
                cs.clip.sample_into(ct, &mut b);
                let k = smoothstep((fade.t / fade.dur.max(1e-6)).clamp(0.0, 1.0));
                for ((o, f), nb) in out.iter_mut().zip(frozen.iter()).zip(b.iter()) {
                    *o = *f;
                    *o = TransformTRS::blend(o, nb, k);
                }
            }
        }
    }

    /// Advance every layer by `dt` seconds and recomposite the final pose.
    pub fn advance(&mut self, dt: f32) {
        let dt = dt * self.speed;
        let sample_fps = self.sample_fps;
        let default_fade = self.default_fade;

        // The accumulator starts at rest; each layer folds over it.
        self.pose.copy_from_slice(&self.rest);

        for li in 0..self.layers.len() {
            let l = &mut self.layers[li];

            // Start the default state if idle (base behavior on spawn).
            if l.cur.is_none() && l.fade.is_none()
                && let Some(d) = l.default_state
                    && d < l.states.len() {
                        l.cur = Some(Playback::enter(d));
                    }

            // -- advance the current playback + fire crossed events --
            if let Some(cur) = l.cur.as_mut() {
                let st = &l.states[cur.state];
                let dur = st.clip.duration.max(1e-6);
                let prev_t = cur.t;
                let fresh = cur.fresh;
                cur.fresh = false;
                let mut new_t = cur.t + dt * st.speed;
                let forward = dt * st.speed > 0.0;
                if st.looped {
                    if forward && !st.clip.events.is_empty() {
                        // Count each event's crossings in (prev_t, new_t] on the
                        // unwrapped timeline (occurrences at t + k·dur). Capped
                        // at 2 so a frame hitch / breakpoint stall can't queue
                        // hundreds of Lua calls; `fresh` makes the very first
                        // tick start-inclusive so an event at exactly t = 0
                        // still fires.
                        let occurrences_upto = |x: f32, et: f32| -> i64 {
                            if x < et { 0 } else { ((x - et) / dur).floor() as i64 + 1 }
                        };
                        for ev in &st.clip.events {
                            if ev.t < 0.0 || ev.t > dur {
                                continue;
                            }
                            let mut n =
                                (occurrences_upto(new_t, ev.t) - occurrences_upto(prev_t, ev.t)).max(0);
                            if fresh && (ev.t - prev_t).abs() < 1e-6 {
                                n += 1; // start boundary is inclusive on entry
                            }
                            for _ in 0..n.min(2) {
                                self.fired.push(ev.func.clone());
                            }
                        }
                    }
                    // rem_euclid keeps reverse playback (< 0 speed) in range too.
                    new_t = new_t.rem_euclid(dur);
                } else {
                    if forward {
                        for ev in &st.clip.events {
                            let crossed = ev.t > prev_t && ev.t <= new_t.min(dur);
                            let at_start = fresh && (ev.t - prev_t).abs() < 1e-6;
                            if crossed || at_start {
                                self.fired.push(ev.func.clone());
                            }
                        }
                    }
                    if new_t >= dur {
                        new_t = dur;
                        cur.finished = true;
                    }
                    new_t = new_t.max(0.0); // reverse playback clamps at the start
                }
                cur.t = new_t;
            }

            // -- advance the fade --
            if let Some(f) = l.fade.as_mut() {
                f.t += dt;
                if let FadeFrom::Playback(p) = &mut f.from {
                    let st = &l.states[p.state];
                    let dur = st.clip.duration.max(1e-6);
                    p.t += dt * st.speed;
                    if st.looped {
                        p.t %= dur;
                    } else if p.t >= dur {
                        p.t = dur;
                    }
                }
                if f.t >= f.dur {
                    l.fade = None;
                }
            }

            // -- a finished one-shot returns to default (base) or fades out --
            if let Some(cur) = l.cur
                && cur.finished {
                    if let Some(d) = l.default_state {
                        if d != cur.state {
                            let fade = l.fade_secs(Some(cur.state), d, default_fade);
                            if fade <= 0.0 {
                                l.fade = None;
                            } else if let Some(old) = l.fade.take() {
                                // Still blending in when it finished — freeze
                                // the mid-blend so the return doesn't pop.
                                let from =
                                    Self::freeze_blend(l, &old, Some(cur), sample_fps, &self.rest);
                                l.fade = Some(Fade { from, t: 0.0, dur: fade });
                            } else {
                                l.fade =
                                    Some(Fade { from: FadeFrom::Playback(cur), t: 0.0, dur: fade });
                            }
                            l.cur = Some(Playback::enter(d));
                        }
                        // d == cur.state: hold the last frame.
                    } else if li > 0 && l.fade.is_none() {
                        // Higher layer with no default: release back to the
                        // layers below.
                        let dur = default_fade;
                        if dur > 0.0 {
                            l.fade =
                                Some(Fade { from: FadeFrom::Playback(cur), t: 0.0, dur });
                        }
                        l.cur = None;
                    }
                }

            // -- sample + fold this layer into the accumulator --
            let l = &self.layers[li];
            let w = l.weight;
            if w <= 0.0 {
                continue;
            }
            match (l.cur, l.fade.as_ref()) {
                (Some(cur), Some(fade)) => {
                    self.scratch_a.copy_from_slice(&self.rest);
                    Self::eval_layer_pose(l, fade, cur, sample_fps, &mut self.scratch_a);
                    // Fold over accumulator on the union of covered nodes —
                    // a frozen snapshot carries the covered set it captured.
                    let mut nodes = l.states[cur.state].covered.clone();
                    match &fade.from {
                        FadeFrom::Playback(p) => {
                            nodes.extend_from_slice(&l.states[p.state].covered);
                        }
                        FadeFrom::Frozen { covered, .. } => {
                            nodes.extend_from_slice(covered);
                        }
                    }
                    nodes.sort_unstable();
                    nodes.dedup();
                    for &n in &nodes {
                        if let (Some(dst), Some(src)) =
                            (self.pose.get(n).copied(), self.scratch_a.get(n))
                        {
                            self.pose[n] = TransformTRS::blend(&dst, src, w);
                        }
                    }
                }
                (Some(cur), None) => {
                    let st = &l.states[cur.state];
                    let t = Self::quantize(cur.t, st.fps.or(sample_fps));
                    self.scratch_b.copy_from_slice(&self.rest);
                    st.clip.sample_into(t, &mut self.scratch_b);
                    for &n in &st.covered {
                        if let (Some(dst), Some(src)) =
                            (self.pose.get(n).copied(), self.scratch_b.get(n))
                        {
                            self.pose[n] = TransformTRS::blend(&dst, src, w);
                        }
                    }
                }
                (None, Some(fade)) => {
                    // Fading out: the outgoing pose releases to the layers
                    // below as the fade completes.
                    let k = 1.0 - smoothstep((fade.t / fade.dur.max(1e-6)).clamp(0.0, 1.0));
                    match &fade.from {
                        FadeFrom::Playback(p) => {
                            let st = &l.states[p.state];
                            let t = Self::quantize(p.t, st.fps.or(sample_fps));
                            self.scratch_b.copy_from_slice(&self.rest);
                            st.clip.sample_into(t, &mut self.scratch_b);
                            for &n in &st.covered {
                                if let (Some(dst), Some(src)) =
                                    (self.pose.get(n).copied(), self.scratch_b.get(n))
                                {
                                    self.pose[n] = TransformTRS::blend(&dst, src, w * k);
                                }
                            }
                        }
                        FadeFrom::Frozen { pose: frozen, covered } => {
                            for &n in covered {
                                if let (Some(dst), Some(src)) =
                                    (self.pose.get(n).copied(), frozen.get(n))
                                {
                                    self.pose[n] = TransformTRS::blend(&dst, src, w * k);
                                }
                            }
                        }
                    }
                }
                (None, None) => {}
            }
        }
    }
}

fn smoothstep(k: f32) -> f32 {
    k * k * (3.0 - 2.0 * k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_about_pivot_keeps_pivot_fixed_and_orbits_the_rest() {
        // A 90° turn about Z, pivoting on (2,0,0). The pivot point must stay put;
        // a point offset from it must swing around it. (t = 0.)
        let trs = TransformTRS {
            t: Vec3::ZERO,
            r: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            s: Vec3::ONE,
        };
        let pivot = Vec3::new(2.0, 0.0, 0.0);
        let m = trs.matrix_about(pivot);
        let fixed = m.transform_point3(pivot);
        assert!((fixed - pivot).length() < 1e-5, "pivot moved: {fixed}");
        // The point one unit +X of the pivot (3,0,0) swings to +Y of the pivot (2,1,0).
        let swung = m.transform_point3(Vec3::new(3.0, 0.0, 0.0));
        assert!((swung - Vec3::new(2.0, 1.0, 0.0)).length() < 1e-5, "expected (2,1,0), got {swung}");
        // pivot = zero must be identical to plain matrix().
        let plain = TransformTRS { t: Vec3::new(1.0, 2.0, 3.0), ..trs };
        assert_eq!(plain.matrix_about(Vec3::ZERO), plain.matrix());
    }

    #[test]
    fn a_pivot_never_moves_a_node_with_a_rotated_rest() {
        // A mesh bone-parented in Blender exports with the bone's axis flip baked
        // into its rest rotation (180° about X is the usual one). Give such a node
        // the centroid pivot the editor auto-assigns to every object node: the REST
        // pose must come out bit-identical to `rest.matrix()`. The old
        // `T(t + pivot)·R·S·T(-pivot)` form displaced it by `pivot - R·pivot`, which
        // flung an R6 character's limbs off the body on import.
        let rest = TransformTRS {
            t: Vec3::new(0.08, -0.05, -0.01),
            r: Quat::from_rotation_x(std::f32::consts::PI),
            s: Vec3::ONE,
        };
        let pivot = Vec3::new(0.0, -0.88, 0.0); // a limb's centroid, well off its origin
        let posed = rest.matrix_about_rest(pivot, &rest);
        let plain = rest.matrix();
        for p in [Vec3::ZERO, Vec3::new(0.5, -1.8, 0.5), pivot] {
            let (a, b) = (posed.transform_point3(p), plain.transform_point3(p));
            assert!((a - b).length() < 1e-6, "rest displaced by the pivot: {a} vs {b}");
        }

        // ...and posing it still orbits the pivot: the pivot POINT stays where rest
        // put it, no matter what rotation is keyed on top.
        let at_rest = plain.transform_point3(pivot);
        let keyed = TransformTRS { r: Quat::from_rotation_z(0.7) * rest.r, ..rest };
        let moved = keyed.matrix_about_rest(pivot, &rest).transform_point3(pivot);
        assert!((moved - at_rest).length() < 1e-6, "pivot moved when posed: {moved} vs {at_rest}");

        // An identity rest is the case `matrix_about` was written for — unchanged.
        let flat = TransformTRS { t: rest.t, r: Quat::from_rotation_z(0.7), s: Vec3::ONE };
        assert_eq!(flat.matrix_about_rest(pivot, &TransformTRS::IDENTITY), flat.matrix_about(pivot));
    }

    /// **One key holds and the next one moves, in the same lane.**
    ///
    /// A lane-wide `Interp` cannot express this at all, and it is what "stepped
    /// animation" actually means: a clip that holds each pose and snaps on the
    /// beats the animator chose, not a clip that never interpolates anywhere.
    /// The only way to author a hold in a smooth lane used to be keying the same
    /// value twice and hoping nobody retimed it.
    ///
    /// The mode belongs to the key the playhead is LEAVING — the segment after
    /// it — which is the convention every keyframe editor uses and the only one
    /// under which a single hold can be authored without touching the key on the
    /// far side of it. Sampling inside both segments is what pins that down: a
    /// version that read the arriving key's mode instead would hold the second
    /// segment and ease the first, and every assertion about the endpoints alone
    /// would still pass.
    #[test]
    fn a_key_can_hold_while_the_next_one_interpolates() {
        let tr = Track {
            times: vec![0.0, 1.0, 2.0],
            values: vec![0.0f32, 10.0, 20.0],
            interp: Interp::Linear,
            key_interp: vec![Interp::Step, Interp::Linear, Interp::Linear],
        };
        // Segment 0→1 holds: anywhere inside it reads the key it left.
        assert_eq!(tr.sample(0.25), Some(0.0));
        assert_eq!(tr.sample(0.99), Some(0.0));
        // …and arrives exactly on the next key.
        assert_eq!(tr.sample(1.0), Some(10.0));
        // Segment 1→2 EASES.
        assert_eq!(tr.sample(1.5), Some(15.0));
        assert_eq!(tr.sample(2.0), Some(20.0));
    }

    /// An eased key starts or settles slowly: halfway through its time it is
    /// nowhere near halfway through its move.
    #[test]
    fn an_eased_key_is_slow_at_one_end() {
        let track = |mode| Track {
            times: vec![0.0, 1.0],
            values: vec![0.0f32, 8.0],
            interp: Interp::Linear,
            key_interp: vec![mode, Interp::Linear],
        };
        assert_eq!(track(Interp::EaseIn).sample(0.5), Some(1.0));
        assert_eq!(track(Interp::EaseOut).sample(0.5), Some(7.0));
        assert_eq!(track(Interp::EaseInOut).sample(0.5), Some(4.0));
        assert!(track(Interp::EaseInOut).sample(0.25).unwrap() < 1.0);
        assert!(track(Interp::EaseInOut).sample(0.75).unwrap() > 7.0);
        // Every mode still lands on its keys.
        for mode in [Interp::EaseIn, Interp::EaseOut, Interp::EaseInOut, Interp::Smooth] {
            assert_eq!(track(mode).sample(0.0), Some(0.0));
            assert_eq!(track(mode).sample(1.0), Some(8.0));
        }
    }

    /// A smooth key has no corner: the velocity just before it equals the
    /// velocity just after it, and the curve still passes through every key.
    #[test]
    fn a_smooth_key_has_the_same_velocity_on_both_sides() {
        let tr = Track {
            times: vec![0.0, 1.0, 3.0, 4.0],
            values: vec![Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0), Vec3::new(2.0, 6.0, 0.0), Vec3::new(0.0, 6.0, 0.0)],
            interp: Interp::Smooth,
            key_interp: Vec::new(),
        };
        for (i, &t) in tr.times.iter().enumerate() {
            assert!((tr.sample(t).unwrap() - tr.values[i]).length() < 1e-5, "misses key {i}");
        }
        let h = 1e-3;
        for &t in &tr.times[1..3] {
            let before = (tr.sample(t).unwrap() - tr.sample(t - h).unwrap()) / h;
            let after = (tr.sample(t + h).unwrap() - tr.sample(t).unwrap()) / h;
            assert!((before - after).length() < 0.05, "a corner at {t}: {before} then {after}");
        }
        // A linear lane over the same keys does have corners, so the test can tell.
        let linear = Track { interp: Interp::Linear, ..tr.clone() };
        let before = (linear.sample(1.0).unwrap() - linear.sample(1.0 - h).unwrap()) / h;
        let after = (linear.sample(1.0 + h).unwrap() - linear.sample(1.0).unwrap()) / h;
        assert!((before - after).length() > 1.0, "linear should have a corner here");
    }

    /// The same for a rotation lane: squad passes through every key and turns
    /// at the same rate on both sides of one.
    #[test]
    fn a_smooth_rotation_passes_through_its_keys_without_a_corner() {
        let rot = |deg: f32| Quat::from_rotation_y(deg.to_radians());
        let tr = Track {
            times: vec![0.0, 1.0, 2.0, 3.0],
            values: vec![rot(0.0), rot(40.0), rot(50.0), rot(120.0)],
            interp: Interp::Smooth,
            key_interp: Vec::new(),
        };
        for (i, &t) in tr.times.iter().enumerate() {
            assert!(tr.sample(t).unwrap().dot(tr.values[i]).abs() > 0.99999, "misses key {i}");
        }
        // A wide step: the angle between two nearly equal quaternions is lost
        // to f32 precision below a few milliradians.
        let h = 0.02;
        let rate = |a: Quat, b: Quat| a.angle_between(b) / h;
        for &t in &tr.times[1..3] {
            let before = rate(tr.sample(t - h).unwrap(), tr.sample(t).unwrap());
            let after = rate(tr.sample(t).unwrap(), tr.sample(t + h).unwrap());
            assert!((before - after).abs() < 0.15, "a corner at {t}: {before} then {after} rad/s");
        }
        let linear = Track { interp: Interp::Linear, ..tr.clone() };
        let before = rate(linear.sample(1.0 - h).unwrap(), linear.sample(1.0).unwrap());
        let after = rate(linear.sample(1.0).unwrap(), linear.sample(1.0 + h).unwrap());
        assert!((before - after).abs() > 0.3, "linear should turn at a different rate after key 1");
    }

    /// An empty `key_interp` is the lane's own mode, for every key — which is
    /// what makes this feature free for every clip that does not use it, and
    /// what keeps an untouched file byte-identical.
    #[test]
    fn a_lane_with_no_per_key_modes_behaves_exactly_as_it_did() {
        let base = Track {
            times: vec![0.0, 1.0],
            values: vec![0.0f32, 10.0],
            interp: Interp::Linear,
            key_interp: Vec::new(),
        };
        assert_eq!(base.sample(0.5), Some(5.0));
        assert_eq!(base.interp_at(0), Interp::Linear);
        let stepped = Track { interp: Interp::Step, ..base.clone() };
        assert_eq!(stepped.sample(0.5), Some(0.0));
        assert_eq!(stepped.interp_at(0), Interp::Step);
        // …and a per-key entry OVERRIDES the lane in both directions, so the
        // lane flag is a default and not a ceiling.
        let eased = Track { key_interp: vec![Interp::Linear, Interp::Linear], ..stepped.clone() };
        assert_eq!(eased.sample(0.5), Some(5.0), "a per-key Linear must beat a Step lane");
        let held = Track { key_interp: vec![Interp::Step, Interp::Step], ..base };
        assert_eq!(held.sample(0.5), Some(0.0), "a per-key Step must beat a Linear lane");
    }

    /// Property lanes get the same treatment — a light that snaps on and then
    /// fades out is one lane, not two.
    #[test]
    fn a_property_lane_holds_per_key_too() {
        let pt = PropertyTrack {
            component: "PointLight".into(),
            field: "intensity".into(),
            times: vec![0.0, 1.0, 2.0],
            values: vec![PropValue::Float(0.0), PropValue::Float(4.0), PropValue::Float(0.0)],
            interp: Interp::Linear,
            key_interp: vec![Interp::Step, Interp::Linear, Interp::Linear],
        };
        assert_eq!(pt.sample(0.5), Some(PropValue::Float(0.0)), "the held segment eased");
        assert_eq!(pt.sample(1.5), Some(PropValue::Float(2.0)), "the eased segment held");
    }

    fn skel2() -> Skeleton {
        Skeleton::new(vec![
            SkelNode { name: "Root".into(), parent: None, rest: TransformTRS::IDENTITY, pivot: Vec3::ZERO },
            SkelNode {
                name: "Arm".into(),
                parent: Some(0),
                rest: TransformTRS { t: Vec3::new(0.0, 1.0, 0.0), ..TransformTRS::IDENTITY },
                pivot: Vec3::ZERO,
            },
        ])
    }

    fn move_clip(name: &str, node: usize, x0: f32, x1: f32, dur: f32) -> Clip {
        Clip {
            name: name.into(),
            duration: dur,
            channels: vec![NodeChannels {
                node,
                translation: Some(Track {
                    times: vec![0.0, dur],
                    values: vec![Vec3::new(x0, 0.0, 0.0), Vec3::new(x1, 0.0, 0.0)],
                    interp: Interp::Linear,
                    key_interp: Vec::new(),
                }),
                rotation: None,
                scale: None,
                properties: Vec::new(),
            }],
            events: Vec::new(),
        }
    }

    fn one_layer_ctl(states: Vec<State>, default: Option<usize>) -> Controller {
        let skel = skel2();
        let layer = Layer::new("Base".into(), states, default);
        Controller::new(skel.rest_pose(), vec![layer], 0.5)
    }

    #[test]
    fn track_sampling_lerps_and_clamps() {
        let tr = Track {
            times: vec![0.0, 1.0],
            values: vec![Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0)],
            interp: Interp::Linear,
            key_interp: Vec::new(),
        };
        assert_eq!(tr.sample(-1.0), Some(Vec3::ZERO));
        assert_eq!(tr.sample(0.5), Some(Vec3::new(1.0, 0.0, 0.0)));
        assert_eq!(tr.sample(9.0), Some(Vec3::new(2.0, 0.0, 0.0)));
        let st = Track { interp: Interp::Step, ..tr };
        assert_eq!(st.sample(0.99), Some(Vec3::ZERO));
    }

    #[test]
    fn property_track_steps_text_and_lerps_floats() {
        // A text (image-swap) lane holds the current key — never blends.
        let img = PropertyTrack {
            component: "UiElement".into(),
            field: "image".into(),
            times: vec![0.0, 0.5, 1.0],
            values: vec![
                PropValue::Text("a.png".into()),
                PropValue::Text("b.png".into()),
                PropValue::Text("c.png".into()),
            ],
            interp: Interp::Step,
            key_interp: Vec::new(),
        };
        assert_eq!(img.sample(0.0), Some(PropValue::Text("a.png".into())));
        assert_eq!(img.sample(0.49), Some(PropValue::Text("a.png".into())));
        assert_eq!(img.sample(0.5), Some(PropValue::Text("b.png".into())));
        assert_eq!(img.sample(9.0), Some(PropValue::Text("c.png".into())));

        // A numeric Linear lane interpolates.
        let op = PropertyTrack {
            component: "UiElement".into(),
            field: "opacity".into(),
            times: vec![0.0, 1.0],
            values: vec![PropValue::Float(0.0), PropValue::Float(1.0)],
            interp: Interp::Linear,
            key_interp: Vec::new(),
        };
        assert_eq!(op.sample(0.25), Some(PropValue::Float(0.25)));
    }

    /// A frame lane holds, even when something has set it to `Linear`.
    ///
    /// Worth pinning rather than trusting: the lane's interp flag is written by
    /// the editor and read from a file, so "always stepped" has to be a property
    /// of the value, not a promise about the flag.
    #[test]
    fn a_sprite_frame_never_blends_however_the_lane_is_flagged() {
        let f = |tex: &str, cell: u32| SpriteFrame {
            texture: tex.into(),
            cols: 4,
            rows: 1,
            cell,
        };
        let lane = PropertyTrack {
            component: "Sprite".into(),
            field: "frame".into(),
            times: vec![0.0, 1.0],
            values: vec![PropValue::Frame(f("a.png", 0)), PropValue::Frame(f("b.png", 3))],
            interp: Interp::Linear,
            key_interp: Vec::new(),
        };
        assert_eq!(lane.sample(0.0), Some(PropValue::Frame(f("a.png", 0))));
        assert_eq!(lane.sample(0.999), Some(PropValue::Frame(f("a.png", 0))));
        assert_eq!(lane.sample(1.0), Some(PropValue::Frame(f("b.png", 3))));
        // …and past the end it holds the last, like every other lane.
        assert_eq!(lane.sample(99.0), Some(PropValue::Frame(f("b.png", 3))));
    }

    /// `whole` is the plain-image case a lone cell index could not express.
    #[test]
    fn a_whole_image_is_a_one_by_one_grid() {
        let f = SpriteFrame::whole("art/shout.png");
        assert_eq!(f.cols, 1);
        assert_eq!(f.rows, 1);
        assert_eq!(f.cell, 0);
    }

    #[test]
    fn controller_samples_active_state_properties() {
        let mut clip = move_clip("Swap", 0, 0.0, 0.0, 1.0);
        clip.channels[0].properties.push(PropertyTrack {
            component: "UiElement".into(),
            field: "image".into(),
            times: vec![0.0, 0.5],
            values: vec![PropValue::Text("a.png".into()), PropValue::Text("b.png".into())],
            interp: Interp::Step,
            key_interp: Vec::new(),
        });
        let mut c = one_layer_ctl(vec![State::new("Swap".into(), clip)], Some(0));
        c.advance(0.1);
        let s = c.sample_properties();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].component, "UiElement");
        assert_eq!(s[0].value, PropValue::Text("a.png".into()));
        c.advance(0.5); // cross the 0.5 key
        let s = c.sample_properties();
        assert_eq!(s[0].value, PropValue::Text("b.png".into()));
    }

    #[test]
    fn world_matrices_compose_parents() {
        let skel = skel2();
        let mut out = Vec::new();
        skel.world_matrices(&skel.rest_pose(), &mut out);
        let p = out[1].transform_point3(Vec3::ZERO);
        assert!((p.y - 1.0).abs() < 1e-6);
    }

    #[test]
    fn repeated_play_same_state_keeps_advancing() {
        // Regression: issuing play() every frame (a movement script) must not
        // restart the blend / freeze the clip at t=0.
        let mut c = one_layer_ctl(
            vec![
                State::new("Idle".into(), move_clip("Idle", 0, 0.0, 0.0, 1.0)),
                State::new("Walk".into(), move_clip("Walk", 0, 0.0, 4.0, 2.0)),
            ],
            Some(0),
        );
        c.advance(0.0);
        c.play(0, 1, None);
        for _ in 0..30 {
            c.play(0, 1, None); // re-issued every frame
            c.advance(0.05);
        }
        let (name, t, _) = c.layers[0].current().unwrap();
        assert_eq!(name, "Walk");
        assert!(t > 1.0, "walk time should keep advancing, got {t}");
    }

    #[test]
    fn zero_fade_in_override_skips_fades_and_lands_on_frame_zero() {
        let mut attack = State::new("Attack".into(), move_clip("Attack", 0, 9.0, 9.0, 1.0));
        attack.fade_in = Some(0.0); // "always instant"
        attack.looped = false;
        let mut c = one_layer_ctl(
            vec![State::new("Idle".into(), move_clip("Idle", 0, 0.0, 0.0, 1.0)), attack],
            Some(0),
        );
        c.sample_fps = Some(8.0); // stepped playback ON — must not delay the snap
        c.advance(0.1);
        c.play(0, 1, Some(3.0)); // even an explicit fade is overridden by instant
        c.advance(0.0);
        // Frame 0 of Attack (x = 9) must show immediately — no blend residue.
        assert!((c.pose()[0].t.x - 9.0).abs() < 1e-4, "got {}", c.pose()[0].t.x);
    }

    #[test]
    fn stepped_fps_quantizes_pose_but_not_time() {
        let mut c = one_layer_ctl(
            vec![State::new("Walk".into(), move_clip("Walk", 0, 0.0, 1.0, 1.0))],
            Some(0),
        );
        c.sample_fps = Some(4.0); // frames at 0, .25, .5, .75
        c.advance(0.1); // t = 0.1 → quantized to 0.0
        assert!((c.pose()[0].t.x - 0.0).abs() < 1e-5);
        c.advance(0.2); // t = 0.3 → frame 0.25
        assert!((c.pose()[0].t.x - 0.25).abs() < 1e-5);
        let (_, t, _) = c.layers[0].current().unwrap();
        assert!((t - 0.3).abs() < 1e-5, "real time flows smoothly, got {t}");
    }

    #[test]
    fn events_fire_once_per_crossing_and_across_loops() {
        let mut clip = move_clip("Walk", 0, 0.0, 1.0, 1.0);
        clip.events.push(ClipEvent { t: 0.5, func: "step".into() });
        let mut c = one_layer_ctl(vec![State::new("Walk".into(), clip)], Some(0));
        c.advance(0.4); // 0 → 0.4: nothing
        assert!(c.take_fired().is_empty());
        c.advance(0.2); // 0.4 → 0.6: crosses 0.5
        assert_eq!(c.take_fired(), vec!["step".to_string()]);
        c.advance(1.0); // 0.6 → 1.6 (wraps): crosses 0.5 once (next lap)
        assert_eq!(c.take_fired(), vec!["step".to_string()]);
        c.advance(2.0); // two full laps → fires twice
        assert_eq!(c.take_fired().len(), 2);
    }

    #[test]
    fn crossfade_blends_between_states() {
        let mut c = one_layer_ctl(
            vec![
                State::new("A".into(), move_clip("A", 0, 0.0, 0.0, 1.0)),
                State::new("B".into(), move_clip("B", 0, 2.0, 2.0, 1.0)),
            ],
            Some(0),
        );
        c.default_fade = 1.0;
        c.advance(0.1);
        c.play(0, 1, None);
        c.advance(0.5); // halfway through the fade
        let x = c.pose()[0].t.x;
        assert!(x > 0.2 && x < 1.8, "mid-fade pose should sit between: {x}");
        c.advance(1.0); // fade done
        assert!((c.pose()[0].t.x - 2.0).abs() < 1e-4);
    }

    #[test]
    fn per_state_fade_override_and_table() {
        let mut c = one_layer_ctl(
            vec![
                State::new("A".into(), move_clip("A", 0, 0.0, 0.0, 1.0)),
                State::new("B".into(), move_clip("B", 0, 2.0, 2.0, 1.0)),
            ],
            Some(0),
        );
        c.default_fade = 5.0;
        c.layers[0].fades.insert((0, 1), 0.0); // A→B snaps
        c.advance(0.1);
        c.play(0, 1, None);
        c.advance(0.0);
        assert!((c.pose()[0].t.x - 2.0).abs() < 1e-4, "A→B override must snap");
    }

    #[test]
    fn higher_layer_overrides_covered_nodes_only() {
        let skel = skel2();
        // Base moves node 0; the overlay moves node 1 only.
        let base = Layer::new(
            "Move".into(),
            vec![State::new("Walk".into(), move_clip("Walk", 0, 3.0, 3.0, 1.0))],
            Some(0),
        );
        let mut over = Layer::new(
            "Attack".into(),
            vec![State::new("Slash".into(), move_clip("Slash", 1, 7.0, 7.0, 0.5))],
            None,
        );
        over.states[0].looped = false;
        let mut c = Controller::new(skel.rest_pose(), vec![base, over], 0.0);
        c.advance(0.1);
        // Only the base is playing: node 1 stays at rest.
        assert!((c.pose()[0].t.x - 3.0).abs() < 1e-4);
        assert!((c.pose()[1].t.x - 0.0).abs() < 1e-4);
        // Trigger the overlay: node 1 is overridden, node 0 keeps the base.
        c.play(1, 0, None);
        c.advance(0.1);
        assert!((c.pose()[0].t.x - 3.0).abs() < 1e-4, "base survives under overlay");
        assert!((c.pose()[1].t.x - 7.0).abs() < 1e-4, "overlay owns its nodes");
        // The one-shot finishes → the layer releases automatically.
        c.advance(1.0);
        c.advance(0.1);
        assert!((c.pose()[1].t.x - 0.0).abs() < 1e-2, "overlay released after finish");
    }

    #[test]
    fn layer_weight_scales_override() {
        let skel = skel2();
        let base = Layer::new(
            "Move".into(),
            vec![State::new("Walk".into(), move_clip("Walk", 0, 0.0, 0.0, 1.0))],
            Some(0),
        );
        let over = Layer::new(
            "Add".into(),
            vec![State::new("Lean".into(), move_clip("Lean", 0, 2.0, 2.0, 1.0))],
            Some(0),
        );
        let mut c = Controller::new(skel.rest_pose(), vec![base, over], 0.0);
        c.set_layer_weight(1, 0.5);
        c.advance(0.1);
        assert!((c.pose()[0].t.x - 1.0).abs() < 1e-4, "half-weight blends halfway");
    }

    #[test]
    fn interrupted_fade_freezes_rest_not_identity() {
        // Regression: the frozen snapshot must seed from REST — node 1 (rest
        // t = (0,1,0)) is untouched by all clips and must never collapse to
        // the origin while a double-interrupted fade blends.
        let mut c = one_layer_ctl(
            vec![
                State::new("A".into(), move_clip("A", 0, 0.0, 0.0, 1.0)),
                State::new("B".into(), move_clip("B", 0, 2.0, 2.0, 1.0)),
                State::new("C".into(), move_clip("C", 0, 4.0, 4.0, 1.0)),
            ],
            Some(0),
        );
        c.default_fade = 1.0;
        c.advance(0.1);
        c.play(0, 1, None);
        c.advance(0.3); // A→B mid-fade
        c.play(0, 2, None); // interrupt → frozen snapshot
        c.advance(0.3);
        assert!(
            (c.pose()[1].t.y - 1.0).abs() < 1e-4,
            "unanimated node must stay at rest during a frozen fade, got {}",
            c.pose()[1].t.y
        );
    }

    #[test]
    fn frozen_fade_covers_only_its_nodes() {
        // Regression: interrupting a fade on an arms-only overlay must not
        // make the overlay override the whole body.
        let skel = skel2();
        let base = Layer::new(
            "Move".into(),
            vec![State::new("Walk".into(), move_clip("Walk", 0, 3.0, 3.0, 1.0))],
            Some(0),
        );
        let over = Layer::new(
            "Arms".into(),
            vec![
                State::new("SlashA".into(), move_clip("SlashA", 1, 7.0, 7.0, 1.0)),
                State::new("SlashB".into(), move_clip("SlashB", 1, 9.0, 9.0, 1.0)),
                State::new("SlashC".into(), move_clip("SlashC", 1, 11.0, 11.0, 1.0)),
            ],
            None,
        );
        let mut c = Controller::new(skel.rest_pose(), vec![base, over], 0.5);
        c.advance(0.1);
        c.play(1, 0, None);
        c.advance(0.1);
        c.play(1, 1, None); // start a fade on the overlay
        c.advance(0.1);
        c.play(1, 2, None); // interrupt → frozen source on the overlay
        c.advance(0.1);
        assert!(
            (c.pose()[0].t.x - 3.0).abs() < 1e-4,
            "base-owned node must not be touched by the overlay's frozen fade, got {}",
            c.pose()[0].t.x
        );
    }

    #[test]
    fn event_at_time_zero_fires_on_entry_and_each_lap() {
        let mut clip = move_clip("Walk", 0, 0.0, 1.0, 1.0);
        clip.events.push(ClipEvent { t: 0.0, func: "kick".into() });
        let mut c = one_layer_ctl(vec![State::new("Walk".into(), clip)], Some(0));
        c.advance(0.4); // entry: t=0 event fires once
        assert_eq!(c.take_fired(), vec!["kick".to_string()]);
        c.advance(0.7); // wraps past 1.0 → t=0 occurrence at lap boundary
        assert_eq!(c.take_fired(), vec!["kick".to_string()]);
    }

    #[test]
    fn frame_hitch_cannot_spam_events() {
        let mut clip = move_clip("Walk", 0, 0.0, 1.0, 1.0);
        clip.events.push(ClipEvent { t: 0.5, func: "step".into() });
        let mut c = one_layer_ctl(vec![State::new("Walk".into(), clip)], Some(0));
        c.advance(0.1);
        let _ = c.take_fired();
        c.advance(300.0); // laptop-sleep-sized stall: 300 laps
        assert!(
            c.take_fired().len() <= 2,
            "a hitch must not queue hundreds of event calls"
        );
    }

    #[test]
    fn negative_speed_keeps_looping_in_range() {
        let mut c = one_layer_ctl(
            vec![State::new("Walk".into(), move_clip("Walk", 0, 0.0, 1.0, 1.0))],
            Some(0),
        );
        c.layers[0].states[0].speed = -1.0;
        for _ in 0..20 {
            c.advance(0.13);
            let (_, t, _) = c.layers[0].current().unwrap();
            assert!((0.0..1.0).contains(&t), "reverse loop out of range: {t}");
        }
        // And it actually moves (samples other than frame 0).
        let x = c.pose()[0].t.x;
        assert!(x > 1e-3, "reverse playback should sample mid-clip, got {x}");
    }

    #[test]
    fn stop_layer_with_default_returns_to_default_not_rest() {
        let mut c = one_layer_ctl(
            vec![
                State::new("Idle".into(), move_clip("Idle", 0, 1.0, 1.0, 1.0)),
                State::new("Walk".into(), move_clip("Walk", 0, 5.0, 5.0, 1.0)),
            ],
            Some(0),
        );
        c.default_fade = 0.2;
        c.advance(0.1);
        c.play(0, 1, Some(0.0));
        c.advance(0.1);
        c.stop_layer(0, Some(0.0));
        c.advance(0.1);
        let (name, _, _) = c.layers[0].current().unwrap();
        assert_eq!(name, "Idle", "stop on a defaulted layer returns to default");
        assert!(
            (c.pose()[0].t.x - 1.0).abs() < 1e-4,
            "pose must be the default state, not rest (got {})",
            c.pose()[0].t.x
        );
    }

    #[test]
    fn play_while_fading_out_crossfades_instead_of_snapping() {
        let skel = skel2();
        let mut over = Layer::new(
            "Arms".into(),
            vec![
                State::new("A".into(), move_clip("A", 1, 8.0, 8.0, 1.0)),
                State::new("B".into(), move_clip("B", 1, 2.0, 2.0, 1.0)),
            ],
            None,
        );
        over.weight = 1.0;
        let base = Layer::new(
            "Base".into(),
            vec![State::new("Idle".into(), move_clip("Idle", 0, 0.0, 0.0, 1.0))],
            Some(0),
        );
        let mut c = Controller::new(skel.rest_pose(), vec![base, over], 0.4);
        c.advance(0.1);
        c.play(1, 0, Some(0.0));
        c.advance(0.1);
        c.stop_layer(1, Some(0.4)); // fade the overlay out…
        c.advance(0.1);
        c.play(1, 1, Some(0.4)); // …then play mid-fade-out
        c.advance(0.0);
        let x = c.pose()[1].t.x;
        assert!(
            x > 0.5 && x < 7.9,
            "mid-fade-out play should blend (not snap to 2.0 or stick at 8.0), got {x}"
        );
    }

    #[test]
    fn one_shot_returns_to_default() {
        let mut jump = State::new("Jump".into(), move_clip("Jump", 0, 5.0, 5.0, 0.5));
        jump.looped = false;
        let mut c = one_layer_ctl(
            vec![State::new("Idle".into(), move_clip("Idle", 0, 1.0, 1.0, 1.0)), jump],
            Some(0),
        );
        c.default_fade = 0.0;
        c.advance(0.1);
        c.play(0, 1, None);
        c.advance(0.6); // jump finishes (0.5s)
        c.advance(0.1);
        let (name, _, _) = c.layers[0].current().unwrap();
        assert_eq!(name, "Idle", "one-shot returns to the default state");
    }

    // ---- animator networking (net_state / apply_net_state) -----------------

    fn net_pair() -> (Controller, Controller) {
        // "Server" and "client" load the same controller asset.
        let mk = || {
            one_layer_ctl(
                vec![
                    State::new("Idle".into(), move_clip("Idle", 0, 0.0, 1.0, 1.0)),
                    State::new("Run".into(), move_clip("Run", 0, 0.0, 2.0, 2.0)),
                ],
                Some(0),
            )
        };
        (mk(), mk())
    }

    #[test]
    fn net_state_round_trips_onto_a_replica() {
        let (mut server, mut client) = net_pair();
        server.advance(0.1); // enters Idle
        server.play(0, 1, None);
        server.advance(0.75);
        server.speed = 1.5;
        client.advance(0.1); // replica idles at the default

        client.apply_net_state(&server.net_state(), 0.25);
        let (name, t, _) = client.layers[0].current().unwrap();
        assert_eq!(name, "Run");
        assert!((t - 0.75).abs() < 1e-4, "lands mid-clip at the replicated time, got {t}");
        assert_eq!(client.speed, 1.5, "global speed replicates");
        // Mid-clip entry keeps the transition fade alive (seek would kill it)
        // and never re-fires the authority's entry events.
        assert!(client.layers[0].fade.is_some(), "blends in, doesn't snap");
        assert!(!client.layers[0].cur.unwrap().fresh);
    }

    #[test]
    fn net_apply_same_state_corrects_only_real_drift() {
        let (mut server, mut client) = net_pair();
        for c in [&mut server, &mut client] {
            c.play(0, 1, None);
            c.advance(0.5);
        }
        // 40 ms apart — inside tolerance: the replica's own advance stands.
        server.advance(0.04);
        client.apply_net_state(&server.net_state(), 0.25);
        let (_, t, _) = client.layers[0].current().unwrap();
        assert!((t - 0.5).abs() < 1e-4, "small drift is left alone, got {t}");
        // 600 ms apart — beyond tolerance: hard-corrected.
        server.advance(0.6);
        client.apply_net_state(&server.net_state(), 0.25);
        let (_, t, _) = client.layers[0].current().unwrap();
        assert!((t - 1.14).abs() < 1e-3, "large drift seeks, got {t}");
    }

    #[test]
    fn net_apply_drift_is_wrap_aware_on_loops() {
        let (mut server, mut client) = net_pair();
        // Both loop Idle (1.0 s); phases 0.98 vs 0.02 are 40 ms apart through
        // the seam, not 960 ms — no correction.
        server.play(0, 0, None);
        client.play(0, 0, None);
        server.advance(0.98);
        client.advance(1.02); // wraps to 0.02
        client.apply_net_state(&server.net_state(), 0.25);
        let (_, t, _) = client.layers[0].current().unwrap();
        assert!((t - 0.02).abs() < 1e-4, "loop-seam phase gap is not drift, got {t}");
    }

    #[test]
    fn net_apply_survives_mismatched_assets() {
        let (mut server, mut client) = net_pair();
        client.play(0, 1, None);
        client.advance(0.3);
        // A state index this build doesn't have, plus an extra layer: both
        // ignored — the replica keeps playing what it has, no panic.
        let bogus = NetAnimState {
            speed: 1.0,
            layers: vec![
                NetLayerState { state: Some(99), t: 0.5, weight: 1.0, dur: 1.0, looped: true, rate: 1.0 },
                NetLayerState { state: Some(0), t: 0.1, weight: 0.5, dur: 1.0, looped: true, rate: 1.0 },
            ],
        };
        client.apply_net_state(&bogus, 0.25);
        let (name, t, _) = client.layers[0].current().unwrap();
        assert_eq!((name, (t * 10.0).round() as i32), ("Run", 3));
        // And a stopped layer replicates as None → the replica releases too.
        server.layers[0].default_state = None;
        server.play(0, 0, None);
        server.advance(0.1);
        server.stop_layer(0, Some(0.0));
        client.layers[0].default_state = None;
        client.apply_net_state(&server.net_state(), 0.25);
        assert!(client.layers[0].cur.is_none(), "released remotely → released here");
    }
}
