//! `decals.*`: marks laid on the world's static surfaces.
//!
//! A decal is a box (a centre, a facing, a width, a height and a depth) whose
//! picture is projected onto whatever static collider faces lie inside it. The
//! faces are clipped to the box here, the moment the decal is added, so a mark
//! that lands on an edge folds over it, one on a floor by a wall climbs the
//! wall, and one on a rock follows the rock. Nothing is a node. The editor
//! merges every decal that shares a picture into one mesh and draws it as a
//! surface, lit, shadowed and fogged like the ground under it.
//!
//! The faces come from the colliders lent to the host for the script pass, so
//! a decal lands on what a ray would hit: level meshes, terrain, Collidable
//! boxes and planes. A sphere or capsule collider has no faces to give.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use glam::{DVec3, Vec3};
use mlua::{Lua, Table, Value};

/// The keys `decals.add` reads.
pub(crate) const ADD_KEYS: &[&str] = &[
    "texture", "pos", "normal", "size", "height", "depth", "up", "rotation", "color", "alpha", "cell",
    "sheetCols", "sheetRows", "layers", "maxAngle", "frame",
];

/// The keys `decals.set` reads.
pub(crate) const SET_KEYS: &[&str] = &["alpha", "color"];

/// How many decals are kept before the oldest goes, until `decals.setMax`.
pub const DEFAULT_MAX: usize = 1024;
/// The most `decals.setMax` allows.
pub const MAX_MAX: usize = 65536;
/// The most triangles one decal keeps: a mark the size of a room laid on a
/// finely meshed floor is cut off here and says so.
pub const MAX_TRIS: usize = 4096;
/// How far round from the decal's facing a face may turn and still take the
/// picture, in degrees: past square, so the side of a box and a wall beside
/// a floor take it, and short of the back of a thin wall or the underside of
/// a ledge.
pub const DEFAULT_MAX_ANGLE: f32 = 100.0;
/// How far a decal sits off the surface it lies on, in metres, so it never
/// fights the surface for the same depth.
pub const LIFT: f32 = 0.004;
/// How long laying marks may take in one frame, in milliseconds. A mark asked
/// for once this is spent waits its turn and is laid in a later frame, within
/// the same budget: a big mark over finely meshed ground takes tens of
/// milliseconds to lay, and a frame that lays a handful of them at once is a
/// visible hitch.
pub const LAY_BUDGET_MS: f64 = 2.0;

/// One corner of a laid decal, relative to the decal's centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecalVert {
    pub pos: Vec3,
    pub normal: Vec3,
    pub uv: [f32; 2],
    /// How much of the decal's alpha shows here: 1, falling to 0 on a face
    /// turned almost as far as `maxAngle` from the decal.
    pub fade: f32,
}

/// A decal as laid.
#[derive(Clone, Debug)]
pub struct Decal {
    pub texture: String,
    pub pos: DVec3,
    pub verts: Vec<DecalVert>,
    pub indices: Vec<u32>,
    pub color: [f32; 3],
    pub alpha: f32,
    /// What the mark rides, so it stays on the rock it was put on as that
    /// moves.
    pub frame: DecalFrame,
    /// What it was asked to be, so it can be laid again when the ground under
    /// it is dug or re-meshed. Its `normal` and `up` are as they stood when it
    /// was laid; its centre is `pos`.
    pub spec: DecalSpec,
    /// Not laid yet: asked for once the frame's laying budget was spent, it
    /// waits its turn (see [`LAY_BUDGET_MS`]).
    pub waiting: bool,
}

/// The node a decal rides, if any.
#[derive(Clone, Debug, PartialEq)]
pub enum DecalFrame {
    /// Not yet placed: the node `decals.add` named, or `None` for the moving
    /// world the mark is in, if any. The editor places it after the script
    /// pass that laid it.
    Asked(Option<u32>),
    /// Rides this node, which stood here when the mark was laid.
    On(u32, floptle_core::transform::Transform),
    /// Stays where it was laid.
    World,
}

/// Every decal in the world, oldest first, and which pictures changed.
#[derive(Debug)]
pub struct DecalStore {
    decals: BTreeMap<u32, Decal>,
    next: u32,
    max: usize,
    /// Decals changed, added or gone since the editor last looked, by id: the
    /// editor rebuilds only the batch pages these are in.
    dirty: std::collections::BTreeSet<u32>,
    /// Changes when the store is emptied wholesale (a new Play, a scene
    /// switch), so the editor drops every batch it holds.
    epoch: u64,
    warned: HashSet<&'static str>,
    /// The nodes whose colliders draw nothing (see [`lay_on`]), set by the
    /// driver.
    undrawn: HashSet<u32>,
    /// Milliseconds spent laying marks this frame, against [`LAY_BUDGET_MS`].
    spent_ms: f64,
    /// The laying still to do for each waiting mark.
    jobs: std::collections::HashMap<u32, LayJob>,
    /// What the last whole frame spent laying, in milliseconds.
    last_ms: f64,
}

impl Default for DecalStore {
    fn default() -> Self {
        Self {
            decals: BTreeMap::new(),
            next: 1,
            max: DEFAULT_MAX,
            dirty: std::collections::BTreeSet::new(),
            epoch: next_epoch(),
            warned: HashSet::new(),
            undrawn: HashSet::new(),
            spent_ms: 0.0,
            jobs: std::collections::HashMap::new(),
            last_ms: 0.0,
        }
    }
}

fn next_epoch() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static EPOCH: AtomicU64 = AtomicU64::new(1);
    EPOCH.fetch_add(1, Ordering::Relaxed)
}

impl DecalStore {
    pub fn len(&self) -> usize {
        self.decals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.decals.is_empty()
    }

    /// Triangles across every decal.
    pub fn triangles(&self) -> usize {
        self.decals.values().map(|d| d.indices.len() / 3).sum()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn get(&self, id: u32) -> Option<&Decal> {
        self.decals.get(&id)
    }

    /// The decals with ids in `lo..hi`, oldest first.
    pub fn in_range(&self, lo: u32, hi: u32) -> impl Iterator<Item = (u32, &Decal)> + '_ {
        self.decals.range(lo..hi).map(|(&id, d)| (id, d))
    }

    /// Every decal showing `texture`, oldest first.
    pub fn with_texture<'a>(&'a self, texture: &'a str) -> impl Iterator<Item = &'a Decal> + 'a {
        self.decals.values().filter(move |d| d.texture == texture)
    }

    /// Decals laid since the last call that are waiting to be told what they
    /// ride: (id, what was asked, centre).
    pub fn unplaced(&self) -> Vec<(u32, Option<u32>, DVec3)> {
        self.decals
            .iter()
            .filter_map(|(&id, d)| match d.frame {
                DecalFrame::Asked(f) => Some((id, f, d.pos)),
                _ => None,
            })
            .collect()
    }

    /// Settle what a decal rides.
    pub fn place(&mut self, id: u32, frame: DecalFrame) {
        if let Some(d) = self.decals.get_mut(&id) {
            d.frame = frame;
            self.dirty.insert(id);
        }
    }

    /// The decals changed, added or gone since the last call, by id.
    pub fn take_dirty(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// Empty the store: every decal goes, and the editor drops its batches.
    pub fn clear(&mut self) {
        let max = self.max;
        let undrawn = std::mem::take(&mut self.undrawn);
        *self = Self::default();
        self.max = max;
        self.undrawn = undrawn;
    }

    fn insert(&mut self, d: Decal) -> u32 {
        let id = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.dirty.insert(id);
        self.decals.insert(id, d);
        self.trim();
        id
    }

    /// What is left of this frame's laying budget, in milliseconds.
    pub fn budget_left_ms(&self) -> f64 {
        LAY_BUDGET_MS - self.spent_ms
    }

    /// Count `ms` of laying against this frame's budget.
    pub fn spend(&mut self, ms: f64) {
        self.spent_ms += ms;
    }

    /// A new frame: its laying budget is whole again.
    pub fn new_frame(&mut self) {
        self.last_ms = std::mem::take(&mut self.spent_ms);
    }

    /// What the last whole frame spent laying marks, in milliseconds: in
    /// `decals.add` and in the pieces laid after it.
    pub fn last_frame_ms(&self) -> f64 {
        self.last_ms
    }

    /// Lay the next piece of waiting mark `id`, which is at `center` (sim
    /// frame) now and has turned by `turn` since it was asked for, and count
    /// the time against the frame's budget. `Some(true)` when the mark is
    /// done: laid, or gone for having nothing to lie on. `None` for a mark
    /// with nothing left to lay.
    pub fn step_job(
        &mut self,
        id: u32,
        cols: &[floptle_physics::AnchoredCollider],
        center: Vec3,
        turn: glam::Quat,
    ) -> Option<bool> {
        let mut job = self.jobs.remove(&id)?;
        let t0 = floptle_core::time::Instant::now();
        let done = job.step(cols, &self.undrawn, center, turn);
        self.spend(t0.elapsed().as_secs_f64() * 1000.0);
        if !done {
            self.jobs.insert(id, job);
            return Some(false);
        }
        let laid = job.finish();
        if laid.indices.is_empty() {
            self.remove(id);
            return Some(true);
        }
        if let Some(d) = self.decals.get_mut(&id) {
            d.verts = laid.verts;
            d.indices = laid.indices;
            d.waiting = false;
            self.dirty.insert(id);
        }
        Some(true)
    }

    /// The marks waiting to be laid, oldest first.
    pub fn waiting(&self) -> Vec<u32> {
        self.decals.iter().filter(|(_, d)| d.waiting).map(|(&id, _)| id).collect()
    }

    /// The nodes whose colliders draw nothing, so no mark lies on them.
    pub fn set_undrawn(&mut self, undrawn: HashSet<u32>) {
        self.undrawn = undrawn;
    }

    pub fn undrawn(&self) -> &HashSet<u32> {
        &self.undrawn
    }

    /// Every decal's id, oldest first.
    pub fn ids(&self) -> Vec<u32> {
        self.decals.keys().copied().collect()
    }

    /// Lay decal `id` again where it now is, on ground that changed shape
    /// under it: centred at `pos` (world), riding `frame`, facing `spec`'s
    /// way. A mark with nothing left to lie on goes. True when it stayed.
    pub fn relaid(&mut self, id: u32, pos: DVec3, frame: DecalFrame, spec: DecalSpec, laid: Laid) -> bool {
        if laid.indices.is_empty() {
            self.remove(id);
            return false;
        }
        let Some(d) = self.decals.get_mut(&id) else { return false };
        d.pos = pos;
        d.frame = frame;
        d.spec = spec;
        d.verts = laid.verts;
        d.indices = laid.indices;
        d.waiting = false;
        self.dirty.insert(id);
        true
    }

    fn remove(&mut self, id: u32) -> bool {
        self.jobs.remove(&id);
        match self.decals.remove(&id) {
            Some(_) => {
                self.dirty.insert(id);
                true
            }
            None => false,
        }
    }

    fn trim(&mut self) {
        while self.decals.len() > self.max {
            let Some((&oldest, _)) = self.decals.iter().next() else { break };
            self.remove(oldest);
        }
    }

    fn set_max(&mut self, max: usize) {
        self.max = max.clamp(1, MAX_MAX);
        self.trim();
    }
}

/// What a decal is asked to be, in the sim frame's terms.
#[derive(Clone, Debug)]
pub struct DecalSpec {
    /// The centre, sim frame.
    pub center: Vec3,
    /// Out of the surface: the picture is projected along `-normal`.
    pub normal: Vec3,
    /// Which way the top of the picture points, roughly; flattened onto the
    /// decal's plane.
    pub up: Vec3,
    pub width: f32,
    pub height: f32,
    pub depth: f32,
    /// The picture's window: `u0, v0, u1, v1`.
    pub uv: [f32; 4],
    pub mask: u32,
    /// Faces turned further than this from `normal` get nothing (cosine).
    pub min_facing: f32,
}

/// What laying a decal produced.
#[derive(Debug, Default)]
pub struct Laid {
    pub verts: Vec<DecalVert>,
    pub indices: Vec<u32>,
    /// Colliders in reach with no faces to give.
    pub faceless: usize,
    /// It was cut off at [`MAX_TRIS`].
    pub capped: bool,
}

/// The decal's frame: right, up and out.
pub fn decal_axes(normal: Vec3, up: Vec3) -> (Vec3, Vec3, Vec3) {
    let n = normal.try_normalize().unwrap_or(Vec3::Y);
    let flat = up - n * up.dot(n);
    let u = flat.try_normalize().unwrap_or_else(|| {
        let alt = if n.y.abs() < 0.9 { Vec3::Y } else { Vec3::NEG_Z };
        (alt - n * alt.dot(n)).normalize()
    });
    (u.cross(n), u, n)
}

/// The width, in cosine, of the fade at the edge of `maxAngle`.
const FADE_BAND: f32 = 0.08;

/// Lay a decal on the faces of `cols`: clip each face in the box to the box,
/// and keep what faces the decal.
///
/// The picture is not simply projected along the decal's facing, which would
/// smear it down any steep side into streaks. It is **unfolded**: a point on
/// the surface lands in the picture where it would if the surface between it
/// and the decal's plane were bent flat, by moving it outward by how far it
/// sits below that plane, in the direction the surface faces. Over the edge
/// of a box the picture carries on down the side as though folded there; on
/// the floor by a wall it carries on up the wall; on flat ground it is the
/// plain projection. The border is cut in that unfolded picture, so the mark
/// ends where its picture ends.
pub fn lay(cols: &[floptle_physics::AnchoredCollider], spec: &DecalSpec) -> Laid {
    lay_on(cols, spec, &HashSet::new())
}

/// [`lay`], on every collider but those of the nodes in `undrawn`: a node
/// with a collider and nothing drawn (an Empty holding a box) is not a
/// surface anyone sees, and a mark on it floats in the air as a pane.
pub fn lay_on(cols: &[floptle_physics::AnchoredCollider], spec: &DecalSpec, undrawn: &HashSet<u32>) -> Laid {
    let mut job = LayJob::new(spec);
    while !job.step(cols, undrawn, spec.center, glam::Quat::IDENTITY) {}
    job.finish()
}

/// The side of one piece of a [`LayJob`], in metres. A step gathers the
/// surface under one piece; on terrain that means meshing the chunks there
/// the first time they are asked, so a piece this size is a chunk or two of
/// work rather than the whole mark's.
pub const LAY_TILE: f32 = 8.0;

/// Laying one decal, a piece at a time, so a big mark can be spread over
/// frames. Everything is kept in the mark's own frame as it stood when it was
/// asked for, centred on it: a mark on a world that moves between steps still
/// fits together, because each step is told where the mark is now and how far
/// it has turned since.
#[derive(Debug)]
pub struct LayJob {
    spec: DecalSpec,
    right: Vec3,
    up: Vec3,
    n: Vec3,
    half: Vec3,
    /// The box every face that could take the picture lies in, around the
    /// centre, in world axes as they stood when it was asked for.
    lo: Vec3,
    hi: Vec3,
    tiles: [u32; 3],
    next: u32,
    laid: Laid,
    poly: Vec<Corner>,
    scratch: Vec<Corner>,
}

impl LayJob {
    pub fn new(spec: &DecalSpec) -> Self {
        let (right, up, n) = decal_axes(spec.normal, spec.up);
        let half = Vec3::new(spec.width * 0.5, spec.height * 0.5, spec.depth * 0.5);
        // The box's reach grows by its depth on every side: a face that folds
        // in from beside the box still lands inside the picture.
        let reach = right.abs() * half.x + up.abs() * half.y + n.abs() * half.z + Vec3::splat(half.z);
        let tiles = ((reach * 2.0) / LAY_TILE).ceil().max(Vec3::ONE);
        Self {
            spec: spec.clone(),
            right,
            up,
            n,
            half,
            lo: -reach,
            hi: reach,
            tiles: [tiles.x as u32, tiles.y as u32, tiles.z as u32],
            next: 0,
            laid: Laid::default(),
            poly: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Pieces in all, and pieces done.
    pub fn pieces(&self) -> (u32, u32) {
        (self.tiles[0] * self.tiles[1] * self.tiles[2], self.next)
    }

    /// Is every piece laid?
    pub fn done(&self) -> bool {
        self.next >= self.tiles[0] * self.tiles[1] * self.tiles[2] || self.laid.capped
    }

    /// The piece a point (in the job's frame) belongs to: the one its
    /// position, held to the box, falls in.
    fn tile_of(&self, p: Vec3) -> u32 {
        let size = (self.hi - self.lo) / Vec3::new(self.tiles[0] as f32, self.tiles[1] as f32, self.tiles[2] as f32);
        let k = ((p.clamp(self.lo, self.hi) - self.lo) / size).floor();
        let ix = (k.x as u32).min(self.tiles[0] - 1);
        let iy = (k.y as u32).min(self.tiles[1] - 1);
        let iz = (k.z as u32).min(self.tiles[2] - 1);
        (iz * self.tiles[1] + iy) * self.tiles[0] + ix
    }

    /// Lay the next piece. `center` is where the mark is now (sim frame) and
    /// `turn` how far it has turned since it was asked for. True when the
    /// whole mark is laid.
    pub fn step(
        &mut self,
        cols: &[floptle_physics::AnchoredCollider],
        undrawn: &HashSet<u32>,
        center: Vec3,
        turn: glam::Quat,
    ) -> bool {
        if self.done() {
            return true;
        }
        let t = self.next;
        self.next += 1;
        let size = (self.hi - self.lo) / Vec3::new(self.tiles[0] as f32, self.tiles[1] as f32, self.tiles[2] as f32);
        let (ix, iy, iz) = (t % self.tiles[0], (t / self.tiles[0]) % self.tiles[1], t / (self.tiles[0] * self.tiles[1]));
        let tlo = self.lo + size * Vec3::new(ix as f32, iy as f32, iz as f32);
        let thi = tlo + size;
        // The piece where the mark is now: its turned box, boxed in world axes.
        let (c, h) = ((tlo + thi) * 0.5, (thi - tlo) * 0.5);
        let m = glam::Mat3::from_quat(turn);
        let wh = m.x_axis.abs() * h.x + m.y_axis.abs() * h.y + m.z_axis.abs() * h.z;
        let wc = center + turn * c;
        let (glo, ghi) = (wc - wh, wc + wh);
        let mask = self.spec.mask;
        let (tris, faceless) = if undrawn.is_empty() {
            floptle_physics::triangles_in_box(cols, glo, ghi, mask)
        } else {
            let mut tris = Vec::new();
            let mut faceless = 0;
            for col in cols.iter().filter(|c| c.eid.is_none_or(|e| !undrawn.contains(&e))) {
                let (t, f) = floptle_physics::triangles_in_box(std::slice::from_ref(col), glo, ghi, mask);
                tris.extend(t);
                faceless += f;
            }
            (tris, faceless)
        };
        self.laid.faceless = self.laid.faceless.max(faceless);
        let back = turn.inverse();
        let (right, up, n, half, spec) = (self.right, self.up, self.n, self.half, &self.spec);
        for tri in tris {
            // Into the frame the mark was asked for in, centred on it.
            let p = tri.p.map(|p| back * (p - center));
            let nrm = tri.n.map(|v| back * v);
            // Each face is laid by exactly one piece: the one its middle is in.
            if self.tile_of((p[0] + p[1] + p[2]) / 3.0) != t {
                continue;
            }
            let face = (p[1] - p[0]).cross(p[2] - p[0]);
            let Some(face) = face.try_normalize() else { continue };
            let facing = face.dot(n);
            if facing < spec.min_facing {
                continue;
            }
            // Into the decal's frame: x along right, y along up, z out.
            self.poly.clear();
            for k in 0..3 {
                let r = p[k];
                let vn = nrm[k].try_normalize().unwrap_or(face);
                self.poly.push(Corner {
                    at: Vec3::new(r.dot(right), r.dot(up), r.dot(n)),
                    normal: Vec3::new(vn.dot(right), vn.dot(up), vn.dot(n)),
                    flat: [0.0; 2],
                });
            }
            // The depth first, in the box's own frame…
            for sign in [1.0f32, -1.0] {
                clip(&self.poly, |c| sign * c.at.z - half.z, &mut self.scratch);
                std::mem::swap(&mut self.poly, &mut self.scratch);
            }
            // …then the border, in the unfolded picture.
            for c in &mut self.poly {
                c.flat = [c.at.x - c.at.z * c.normal.x, c.at.y - c.at.z * c.normal.y];
            }
            for (axis, limit) in [(0, half.x), (1, half.y)] {
                for sign in [1.0f32, -1.0] {
                    clip(&self.poly, |c| sign * c.flat[axis] - limit, &mut self.scratch);
                    std::mem::swap(&mut self.poly, &mut self.scratch);
                }
            }
            let poly = &self.poly;
            if poly.len() < 3 {
                continue;
            }
            let laid = &mut self.laid;
            if laid.indices.len() / 3 + poly.len() - 2 > MAX_TRIS {
                laid.capped = true;
                break;
            }
            let fade = ((facing - spec.min_facing) / FADE_BAND).clamp(0.0, 1.0);
            let fade = fade * fade * (3.0 - 2.0 * fade);
            let lift = face * LIFT;
            let base = laid.verts.len() as u32;
            for c in poly {
                let s = c.flat[0] / spec.width + 0.5;
                let tv = 0.5 - c.flat[1] / spec.height;
                let normal = right * c.normal.x + up * c.normal.y + n * c.normal.z;
                laid.verts.push(DecalVert {
                    pos: right * c.at.x + up * c.at.y + n * c.at.z + lift,
                    normal: normal.try_normalize().unwrap_or(face),
                    uv: [spec.uv[0] + s * (spec.uv[2] - spec.uv[0]), spec.uv[1] + tv * (spec.uv[3] - spec.uv[1])],
                    fade,
                });
            }
            for k in 1..poly.len() as u32 - 1 {
                laid.indices.extend_from_slice(&[base, base + k, base + k + 1]);
            }
        }
        self.done()
    }

    /// What the job laid.
    pub fn finish(self) -> Laid {
        self.laid
    }
}

/// A corner of a face being clipped, in the decal's frame.
#[derive(Clone, Copy, Debug)]
struct Corner {
    at: Vec3,
    normal: Vec3,
    /// Where it lands in the unfolded picture.
    flat: [f32; 2],
}

impl Corner {
    fn lerp(self, b: Self, k: f32) -> Self {
        Self {
            at: self.at + (b.at - self.at) * k,
            normal: self.normal + (b.normal - self.normal) * k,
            flat: [self.flat[0] + (b.flat[0] - self.flat[0]) * k, self.flat[1] + (b.flat[1] - self.flat[1]) * k],
        }
    }
}

/// One Sutherland–Hodgman step: keep the corners where `outside(c) <= 0`,
/// cutting each edge that crosses.
fn clip(poly: &[Corner], outside: impl Fn(&Corner) -> f32, out: &mut Vec<Corner>) {
    out.clear();
    if poly.len() < 3 {
        return;
    }
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let (da, db) = (outside(&a), outside(&b));
        if da <= 0.0 {
            out.push(a);
        }
        if (da <= 0.0) != (db <= 0.0) {
            out.push(a.lerp(b, da / (da - db)));
        }
    }
}

/// What the Lua side shares with the host.
pub(crate) struct DecalShared {
    pub store: Rc<RefCell<DecalStore>>,
    pub colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    pub sim_origin: Rc<RefCell<DVec3>>,
    pub layers: Rc<RefCell<floptle_core::Layers>>,
    pub logs: Rc<RefCell<Vec<crate::ScriptLog>>>,
    pub profile: crate::SharedProfile,
}

fn color_of(v: &Value, call: &str) -> mlua::Result<[f32; 3]> {
    let Value::Table(t) = v else {
        return Err(mlua::Error::runtime(format!("{call}: `color` is {{r, g, b}} from 0 to 1")));
    };
    let get = |i: usize, k: &str| -> mlua::Result<f32> {
        match t.raw_get::<Option<f32>>(i)? {
            Some(x) => Ok(x),
            None => Ok(t.raw_get::<Option<f32>>(k)?.unwrap_or(1.0)),
        }
    };
    Ok([get(1, "r")?, get(2, "g")?, get(3, "b")?])
}

fn number(t: &Table, key: &str, call: &str) -> mlua::Result<Option<f32>> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Integer(i) => Ok(Some(i as f32)),
        Value::Number(x) if x.is_finite() => Ok(Some(x as f32)),
        _ => Err(mlua::Error::runtime(format!("{call}: `{key}` is a number"))),
    }
}

fn vec3(t: &Table, key: &str, call: &str) -> mlua::Result<Option<DVec3>> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        v => crate::math_api::vec3_of(&v)
            .map(Some)
            .ok_or_else(|| mlua::Error::runtime(format!("{call}: `{key}` is a vec3"))),
    }
}

fn warn_once(shared: &DecalShared, store: &mut DecalStore, key: &'static str, msg: String) {
    if store.warned.insert(key) {
        shared.logs.borrow_mut().push(crate::ScriptLog { level: crate::LogLevel::Warn, msg, source: None });
    }
}

fn publish_counts(shared: &DecalShared, store: &DecalStore) {
    shared.profile.borrow_mut().set_decal_counts(store.len(), store.triangles());
}

pub(crate) fn install_decal_api(lua: &Lua, shared: DecalShared) -> mlua::Result<()> {
    let shared = Rc::new(shared);
    let t = lua.create_table()?;

    let s = shared.clone();
    t.set(
        "add",
        lua.create_function(move |_, opts: Table| {
            let call = "decals.add";
            crate::opts::check_keys(&opts, ADD_KEYS, call)?;
            let texture: String = match opts.get::<Value>("texture")? {
                Value::String(p) => p.to_str()?.to_string(),
                _ => return Err(mlua::Error::runtime("decals.add: `texture` is the picture's path, like \"textures/decals/blood.png\"")),
            };
            let pos = vec3(&opts, "pos", call)?
                .ok_or_else(|| mlua::Error::runtime("decals.add: `pos` is where the mark goes, a vec3 on the surface"))?;
            let normal = vec3(&opts, "normal", call)?
                .ok_or_else(|| mlua::Error::runtime("decals.add: `normal` is the surface's facing there, a vec3 (a raycast hit's nx, ny, nz)"))?
                .as_vec3();
            let size = number(&opts, "size", call)?
                .ok_or_else(|| mlua::Error::runtime("decals.add: `size` is the mark's width in metres"))?;
            if size <= 0.0 {
                return Err(mlua::Error::runtime("decals.add: `size` is above 0"));
            }
            let height = number(&opts, "height", call)?.unwrap_or(size).max(1e-3);
            let depth = number(&opts, "depth", call)?.unwrap_or(size.max(height) * 0.5).max(1e-3);
            let n = normal.try_normalize().ok_or_else(|| mlua::Error::runtime("decals.add: `normal` has no length"))?;
            let mut up = vec3(&opts, "up", call)?.map(|v| v.as_vec3()).unwrap_or(Vec3::Y);
            if let Some(r) = number(&opts, "rotation", call)? {
                let (_, u, _) = decal_axes(n, up);
                up = glam::Quat::from_axis_angle(n, r) * u;
            }
            let color = match opts.get::<Value>("color")? {
                Value::Nil => [1.0; 3],
                v => color_of(&v, call)?,
            };
            let alpha = number(&opts, "alpha", call)?.unwrap_or(1.0).clamp(0.0, 1.0);
            let cols = number(&opts, "sheetCols", call)?.unwrap_or(1.0).max(1.0).floor();
            let rows = number(&opts, "sheetRows", call)?.unwrap_or(1.0).max(1.0).floor();
            let cell = number(&opts, "cell", call)?.unwrap_or(0.0).max(0.0).floor();
            let (cx, cy) = (cell % cols, (cell / cols).floor().min(rows - 1.0));
            let uv = [cx / cols, cy / rows, (cx + 1.0) / cols, (cy + 1.0) / rows];
            let max_angle = number(&opts, "maxAngle", call)?.unwrap_or(DEFAULT_MAX_ANGLE).clamp(0.0, 170.0);
            let frame = match opts.get::<Value>("frame")? {
                Value::Nil => None,
                v => Some(crate::env::node_id_of(&v).ok_or_else(|| {
                    mlua::Error::runtime(format!("decals.add: `frame` is the node the mark rides, got {}", v.type_name()))
                })?),
            };
            let mask = match opts.get::<Value>("layers")? {
                Value::Nil => !0u32,
                v => {
                    let names: Vec<String> = match v {
                        Value::String(n) => vec![n.to_str()?.to_string()],
                        Value::Table(list) => list.sequence_values::<String>().collect::<mlua::Result<_>>()?,
                        _ => return Err(mlua::Error::runtime("decals.add: `layers` is a layer name or a list of them")),
                    };
                    let table = s.layers.borrow();
                    let mut m = 0u32;
                    for name in &names {
                        match table.index_of(name) {
                            Some(i) => m |= 1 << i,
                            None => {
                                return Err(mlua::Error::runtime(format!(
                                    "decals.add: no layer named '{name}' (project layers: {})",
                                    table.names.join(", ")
                                )))
                            }
                        }
                    }
                    m
                }
            };
            let origin = *s.sim_origin.borrow();
            let spec = DecalSpec {
                center: (pos - origin).as_vec3(),
                normal: n,
                up,
                width: size,
                height,
                depth,
                uv,
                mask,
                min_facing: max_angle.to_radians().cos(),
            };
            let keep = spec.clone();
            // Laid a piece at a time while this frame's budget lasts; what is
            // left waits its turn, the driver lays it in later frames, and
            // `decals.landed(id)` says when it is done.
            let mut job = LayJob::new(&spec);
            let mut done = false;
            while s.store.borrow().budget_left_ms() > 0.0 {
                let t0 = floptle_core::time::Instant::now();
                done = job.step(&s.colliders.borrow(), s.store.borrow().undrawn(), spec.center, glam::Quat::IDENTITY);
                s.store.borrow_mut().spend(t0.elapsed().as_secs_f64() * 1000.0);
                if done {
                    break;
                }
            }
            let mut store = s.store.borrow_mut();
            if !done {
                let id = store.insert(Decal {
                    texture,
                    pos,
                    verts: Vec::new(),
                    indices: Vec::new(),
                    color,
                    alpha,
                    frame: DecalFrame::Asked(frame),
                    spec: keep,
                    waiting: true,
                });
                store.jobs.insert(id, job);
                publish_counts(&s, &store);
                return Ok(Some(id));
            }
            let laid = job.finish();
            if laid.capped {
                warn_once(
                    &s,
                    &mut store,
                    "capped",
                    format!(
                        "decals.add: a mark of {size} m met more than {MAX_TRIS} triangles and was cut off there; \
                         a smaller mark, or a less finely meshed surface, lays whole"
                    ),
                );
            }
            if laid.indices.is_empty() {
                if laid.faceless > 0 {
                    warn_once(
                        &s,
                        &mut store,
                        "faceless",
                        "decals.add: the only thing under this mark is a sphere or capsule collider, which has no \
                         faces for a decal to lie on; it lands on meshes, terrain, boxes and planes"
                            .into(),
                    );
                }
                return Ok(None);
            }
            let id = store.insert(Decal {
                texture,
                pos,
                verts: laid.verts,
                indices: laid.indices,
                color,
                alpha,
                frame: DecalFrame::Asked(frame),
                spec: keep,
                waiting: false,
            });
            publish_counts(&s, &store);
            Ok(Some(id))
        })?,
    )?;

    let s = shared.clone();
    t.set(
        "set",
        lua.create_function(move |_, (id, opts): (u32, Table)| {
            crate::opts::check_keys(&opts, SET_KEYS, "decals.set")?;
            let alpha = number(&opts, "alpha", "decals.set")?;
            let color = match opts.get::<Value>("color")? {
                Value::Nil => None,
                v => Some(color_of(&v, "decals.set")?),
            };
            let mut store = s.store.borrow_mut();
            let Some(d) = store.decals.get_mut(&id) else { return Ok(false) };
            if let Some(a) = alpha {
                d.alpha = a.clamp(0.0, 1.0);
            }
            if let Some(c) = color {
                d.color = c;
            }
            store.dirty.insert(id);
            Ok(true)
        })?,
    )?;

    let s = shared.clone();
    t.set(
        "remove",
        lua.create_function(move |_, id: u32| {
            let mut store = s.store.borrow_mut();
            let gone = store.remove(id);
            publish_counts(&s, &store);
            Ok(gone)
        })?,
    )?;

    let s = shared.clone();
    t.set(
        "clear",
        lua.create_function(move |_, ()| {
            let mut store = s.store.borrow_mut();
            store.clear();
            publish_counts(&s, &store);
            Ok(())
        })?,
    )?;

    // decals.landed(id) → true once the mark is laid, false while it waits
    // its turn, nil when there is no such mark (removed, trimmed, or it found
    // nothing to lie on).
    let s = shared.clone();
    t.set(
        "landed",
        lua.create_function(move |_, id: u32| Ok(s.store.borrow().get(id).map(|d| !d.waiting)))?,
    )?;

    let s = shared.clone();
    t.set("count", lua.create_function(move |_, ()| Ok(s.store.borrow().len()))?)?;

    let s = shared.clone();
    t.set(
        "setMax",
        lua.create_function(move |_, n: f64| {
            if !n.is_finite() || n < 1.0 {
                return Err(mlua::Error::runtime(format!("decals.setMax: at least 1 (at most {MAX_MAX})")));
            }
            let mut store = s.store.borrow_mut();
            store.set_max(n as usize);
            publish_counts(&s, &store);
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    t.set("max", lua.create_function(move |_, ()| Ok(s.store.borrow().max))?)?;

    lua.globals().set("decals", t)?;
    Ok(())
}
