//! Render-agnostic "what an entity is made of" components — the data a scene file
//! places and the editor edits. The render loop interprets these (plus the
//! entity's [`Transform`](crate::transform::Transform)) into draw commands; the
//! components themselves hold no GPU handles, so they serialize cleanly and the
//! same world can be authored, saved, and replayed.

/// A human-facing name for an entity (shown in the editor hierarchy).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name(pub String);

/// The named collision/query **layer** a node is on. Layers are project-defined
/// (Project Settings, up to 32) and referenced BY NAME everywhere — scene files,
/// scripts (`node.layer`), the Inspector — so reordering the project's layer
/// list never silently re-layers a scene. A node with no `Layer` component is
/// on `"Default"`. Resolved to a bit index once per Play by
/// [`crate::layers::Layers`]; physics filters contacts through the project's
/// collision matrix and raycasts filter with the same bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer(pub String);

/// What draws in front of what, for a flat scene.
///
/// A 2D game in Floptle is a 3D scene that refuses the third axis, so ordering
/// is depth — and hand-nudging Z is how it had to be done: the floor at
/// `z = 0.001`, the player at `0.002`, and a hundred numbers nobody can read as
/// an intention. Reorder two things and you edit both. Add a layer between them
/// and you edit everything above.
///
/// So: **a named sorting layer, plus an order within it.** Named for the same
/// reason [`Layer`] is (see [`crate::layers`]) — reordering the project's list
/// can never silently re-sort a scene, which an index can. `order` breaks ties
/// inside one layer: higher draws in front, and negatives are fine.
///
/// A node with no `Sorting` is on `"Default"` at order 0, so nothing about an
/// existing scene changes until something opts in. This is resolved to a small
/// Z offset **on the draw matrix only** — the node's transform, its physics and
/// what a script reads are all untouched, because "what draws on top" is not
/// supposed to move anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sorting {
    /// The project sorting layer's NAME. Empty = the default layer.
    pub layer: String,
    /// Position within the layer. Higher is nearer the camera.
    pub order: i32,
}

/// The default sorting layer, which always exists and cannot be removed.
pub const DEFAULT_SORTING_LAYER: &str = "Default";

/// Z given to one step of sorting layer, and to one step of `order`.
///
/// Both are exact powers of two, so a rank-and-order pair converts to a float
/// with no rounding at all and two different pairs can never collapse onto the
/// same Z through accumulated error — which would put the tie back exactly where
/// this exists to remove it. The layer step is 64 orders wide, and `order` is
/// clamped into that so a big order can never climb into the next layer up: a
/// sorting layer that leaks is worse than no sorting layer, because it is only
/// visible in the one scene that happens to trip it.
pub const SORT_LAYER_STEP: f32 = 1.0 / 64.0;
pub const SORT_ORDER_STEP: f32 = 1.0 / 4096.0;

/// The Z offset for a resolved `(layer rank, order)`.
///
/// Rank 0 order 0 is exactly 0.0, so a scene where nothing opts in is drawn at
/// precisely the Z it always was.
pub fn sorting_offset(rank: u32, order: i32) -> f32 {
    let span = (SORT_LAYER_STEP / SORT_ORDER_STEP) as i32; // orders per layer
    let order = order.clamp(-span / 2, span / 2 - 1);
    rank as f32 * SORT_LAYER_STEP + order as f32 * SORT_ORDER_STEP
}

/// Three-valued opt-in for the 2D lighting path: nobody has said, yes, or no.
///
/// **Three values and not a bool**, because a bool cannot tell "nobody has said"
/// from "somebody said false". An inference that wrote into a bool would
/// overwrite a deliberate `false` the moment the scene changed shape — a light
/// you had explicitly made 3D quietly becoming 2D again because somebody added
/// an orthographic camera. `Auto` is a distinct state that the engine answers
/// and never writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lit2D {
    /// Let the engine decide. See [`infers_2d`].
    #[default]
    Auto,
    /// This is 2D, whatever the scene looks like.
    Yes,
    /// This is 3D, whatever the scene looks like.
    No,
}

impl Lit2D {
    pub const ALL: [Lit2D; 3] = [Lit2D::Auto, Lit2D::Yes, Lit2D::No];

    pub fn name(self) -> &'static str {
        match self {
            Lit2D::Auto => "auto",
            Lit2D::Yes => "2d",
            Lit2D::No => "3d",
        }
    }

    /// Every spelling accepted from Lua / a `.ron`, and the list an error
    /// message prints. One list, one parser (`floptle/0082`).
    pub const ACCEPTS: &'static [&'static str] = &["auto", "2d", "3d"];

    pub fn parse(s: &str) -> Option<Lit2D> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Lit2D::Auto),
            "2d" | "yes" | "flat" => Some(Lit2D::Yes),
            "3d" | "no" => Some(Lit2D::No),
            _ => None,
        }
    }
}

/// What the 2D lighting inference is allowed to look at.
///
/// A struct rather than loose arguments so the answer is a pure function of a
/// stated set of facts, and so a test can pose a scene without building one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lit2DFacts {
    /// This node emits light (a `PointLight`, or the scene's key light).
    pub emits: bool,
    /// This node's `Matter` is one of the flat kinds — a tilemap or a sprite
    /// batch.
    pub flat_matter: bool,
    /// The scene's active camera is orthographic.
    pub flat_camera: bool,
}

/// Whether `Auto` means 2D for a node, and the one-line reason.
///
/// The reason is returned, not just the verdict, because the whole design rests
/// on trusting this: an inference you cannot see is one you cannot trust, and
/// the Inspector prints exactly this string beside `Auto`.
///
/// The two rules, and why they are the two:
///
/// * **A light is 2D when the active camera is orthographic.** A 3D scene does
///   not have an orthographic active camera, so this cannot flip a 3D scene's
///   lights by accident — which is the failure that matters. A technical or
///   isometric shot that *is* orthographic and wants 3D lighting says so once.
/// * **A receiver is 2D when it is a tilemap or a sprite batch.** Those kinds
///   exist for flat games. A mesh in a 2D scene stays 3D-lit unless it is told
///   otherwise, which is what makes mixing the two deliberate rather than
///   something you discover.
///
/// Deliberately NOT part of it: how near the node is to the camera plane, and
/// whether the project has named sorting layers. Both are true of scenes that
/// want nothing to do with 2D lighting, and an inference that is *usually*
/// right is worse than none — it fails in the scenes least able to explain it.
pub fn infers_2d(facts: Lit2DFacts) -> (bool, &'static str) {
    if facts.emits {
        return if facts.flat_camera {
            (true, "the active camera is orthographic")
        } else {
            (false, "the active camera is perspective")
        };
    }
    if facts.flat_matter {
        (true, "a tilemap and a sprite batch are flat")
    } else {
        (false, "only tilemaps and sprite batches are lit flat by default")
    }
}

/// Whether a node takes part in 2D lighting, and why.
///
/// `Yes`/`No` answer for themselves and the inference is not consulted at all,
/// so a scene reshaping around a node cannot change what an author stated.
pub fn resolve_2d(mode: Lit2D, facts: Lit2DFacts) -> (bool, &'static str) {
    match mode {
        Lit2D::Yes => (true, "set to 2D"),
        Lit2D::No => (false, "set to 3D"),
        Lit2D::Auto => infers_2d(facts),
    }
}

/// A node's place in the 2D lighting system: whether it is on that path, and —
/// for a light — which sorting layers it reaches.
///
/// Absent means [`Lit2D::Auto`] with no layer restriction, so a scene that has
/// never heard of this component behaves exactly as it did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lighting2D {
    pub mode: Lit2D,
    /// **Lights only.** The sorting layers this light reaches, by name. Empty —
    /// the default — means every layer.
    ///
    /// This is how a 2D artist thinks about a light: *this torch lights Terrain
    /// and Characters, not Background*, and the background staying flat while a
    /// torch passes over it is the single most common thing a 2D lighting system
    /// is asked for. It reuses the sorting layers a 2D scene already names, so
    /// there is no second list to keep in step.
    ///
    /// It is **not** the collision layer mask. A background that collides with
    /// nothing and a player that does sort — and light — independently of that.
    pub layers: Vec<String>,
}

impl Lighting2D {
    /// Whether a light with this component reaches a receiver in `layer`.
    ///
    /// An empty list reaches everything, which is what a light dropped into a
    /// scene should do — a new light that lit nothing until you filled in a list
    /// would read as a broken light.
    pub fn reaches(&self, layer: &str) -> bool {
        if self.layers.is_empty() {
            return true;
        }
        let layer = if layer.trim().is_empty() { DEFAULT_SORTING_LAYER } else { layer };
        self.layers.iter().any(|l| {
            let l = if l.trim().is_empty() { DEFAULT_SORTING_LAYER } else { l.as_str() };
            l == layer
        })
    }
}

/// Whether a 2D node blocks light, three-valued for the same reason as
/// [`Lit2D`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cast2D {
    /// Let the engine decide: a tilemap casts from the colliders its tileset
    /// already declares, and nothing else casts.
    #[default]
    Auto,
    Yes,
    No,
}

impl Cast2D {
    pub const ALL: [Cast2D; 3] = [Cast2D::Auto, Cast2D::Yes, Cast2D::No];

    pub fn name(self) -> &'static str {
        match self {
            Cast2D::Auto => "auto",
            Cast2D::Yes => "on",
            Cast2D::No => "off",
        }
    }

    pub const ACCEPTS: &'static [&'static str] = &["auto", "on", "off"];

    pub fn parse(s: &str) -> Option<Cast2D> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Cast2D::Auto),
            "on" | "yes" | "true" => Some(Cast2D::Yes),
            "off" | "no" | "false" => Some(Cast2D::No),
            _ => None,
        }
    }
}

/// Whether a node blocks 2D light. Absent = [`Cast2D::Auto`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shadow2D(pub Cast2D);

/// Whether a node casts a 2D shadow, and why.
///
/// Under `Auto` a **tilemap casts exactly where it is solid** — the colliders
/// its tileset already declares. A wall marked solid occludes light with no
/// second authoring step, and a level's collision *is* its light occlusion, so
/// the two can never drift apart. That is the same argument that made per-tile
/// collision a property of the tileset rather than of the map.
///
/// `collidable` is whether the node's collision is actually switched on, so a
/// tilemap with its Collidable switch off does not cast from colliders the sim
/// is not using either.
pub fn resolve_shadow_2d(cast: Cast2D, flat_matter: bool, collidable: bool) -> (bool, &'static str) {
    match cast {
        Cast2D::Yes => (true, "set to cast"),
        Cast2D::No => (false, "set not to cast"),
        Cast2D::Auto if flat_matter && collidable => (true, "casts where it is solid"),
        Cast2D::Auto if flat_matter => (false, "not collidable, so nothing to cast from"),
        Cast2D::Auto => (false, "only tilemaps cast by default"),
    }
}

/// Free-form string **tags** on a node — mark it `"enemy"`, `"checkpoint"`,
/// `"breakable"` and find/compare cheaply from scripts (`node:hasTag`,
/// `findTagged`). A node holds any number of tags (no single-tag straitjacket);
/// order is authoring order, duplicates are rejected on add.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags(pub Vec<String>);

impl Tags {
    /// Whether the exact tag is present (case-sensitive).
    pub fn has(&self, tag: &str) -> bool {
        self.0.iter().any(|t| t == tag)
    }
}

/// The cell value that leaves a tilemap square EMPTY.
///
/// Not `0`: zero is a perfectly good first cell of a sheet, and a grid that
/// cannot express "nothing here" without giving up its first tile is a grid
/// that makes every artist renumber their sheet.
pub const EMPTY_TILE: u32 = u32::MAX;

/// One sprite in a [`Matter::SpriteBatch`].
///
/// Positions are LOCAL to the batch node, so the node's transform still places
/// and orients the whole thing — a batch is a node like any other, it just draws
/// more than one quad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sprite {
    /// Local position.
    pub pos: [f32; 3],
    /// Roll about the view axis, radians. Sprites face +Z like the quad does.
    pub rot: f32,
    /// Multiplies the batch's `size`, per axis.
    ///
    /// Two numbers rather than one because squash-and-stretch is how a 2D game
    /// telegraphs an attack, and a single factor made that the one effect a
    /// batch could not do — so games kept their enemies on scene nodes and
    /// maintained two rendering paths. `b:draw` still takes one number when one
    /// is all you want.
    pub scale: [f32; 2],
    /// Cell of the Material's sheet.
    pub cell: u32,
    /// Multiplied into the texture, RGBA. This is the thing a shared Material
    /// could never give one sprite on its own.
    pub tint: [f32; 4],
}

impl Default for Sprite {
    fn default() -> Self {
        Self { pos: [0.0; 3], rot: 0.0, scale: [1.0; 2], cell: 0, tint: [1.0; 4] }
    }
}

/// This frame's sprites for a [`Matter::SpriteBatch`] node.
///
/// Runtime-only and never serialised, for the same reason as [`Made`]: these are
/// rebuilt every frame from whatever the game is doing, and a saved scene
/// containing them would describe last session's bullets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sprites(pub Vec<Sprite>);

/// Which row of a UI **repeater** this node is: 0-based, in flow order.
///
/// Runtime-only and never serialised — a repeater's rows are spawned by the
/// engine while the game runs, so an index in a saved scene would describe a
/// node that no longer exists. A row's script reads it as `node.index` and
/// fills itself in; that read is the entire interface between "there are seven
/// of these" and "this one is the third".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepeatIndex(pub u32);

/// A node built by `ui.make`, and the identity the next call matches it by.
///
/// Runtime-only and never serialised, for the same reason as [`RepeatIndex`]:
/// these nodes are conjured from data while the game runs, and a saved scene
/// that contained them would describe last session's roster.
///
/// The marker is also what keeps a made subtree from touching its neighbours.
/// Reconciliation only ever considers a container's children that carry this,
/// so an element you placed by hand under the same parent is never matched,
/// never patched, and never destroyed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Made {
    /// The description's `key`, or empty when it was matched by position.
    pub key: String,
    /// Position among its made siblings as of the last reconcile — the stable
    /// order the next diff reads them in.
    pub slot: u32,
    /// The described kind that built it (`"row"`, `"text"`, …).
    ///
    /// Recorded rather than inferred from the element itself: a `text` with a
    /// background fill and a `box` with a label are the same ElementSpec, so a
    /// diff that guessed would see the kind change every call and rebuild the
    /// screen from scratch each time.
    pub kind: String,
}

/// A scene-graph parent link: this entity's [`Transform`](crate::transform::Transform)
/// is **local** (relative to the parent), and its world transform is the parent's
/// world transform composed with it. Moving/rotating/scaling a parent therefore
/// carries all of its descendants. A node without a `Parent` is a root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parent(pub crate::ecs::Entity);

/// Rides a **bone / sub-object** of a rigged (node-preserving) `Matter::Mesh`, so a
/// weapon, emitter, or pickup follows a character's hand/arm — including under
/// animation. Lives ALONGSIDE [`Parent`]`(target)` (which keeps the node in the
/// hierarchy and serializable): `Parent` says *under which mesh*, `BoneAttach` says
/// *which bone under it*. Each frame `resolve_attachments` sets this node's LOCAL
/// transform to `bone_local · offset` (both in the mesh's model space), and the
/// ordinary [`world_transform`] parent-walk re-applies the mesh's f64 world — so the
/// attachment stays jitter-free far from the origin and every consumer (render,
/// physics, gizmo, particles) follows the bone through the one choke point.
#[derive(Clone, Debug, PartialEq)]
pub struct BoneAttach {
    /// The rigged Mesh entity this rides (kept equal to `Parent(target)`).
    pub target: crate::ecs::Entity,
    /// The skeleton node NAME (portable across re-import; resolved to an index each
    /// frame via `Skeleton::index_of`, like animation clips).
    pub bone: String,
    /// The child's transform IN THE BONE'S LOCAL SPACE — seeded on attach so the node
    /// doesn't jump, then editable to position it on the bone.
    pub offset: crate::transform::Transform,
}

/// A procedural primitive shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Cube,
    Sphere,
    Capsule,
    // Keep new shapes LAST: the renderer indexes meshes by `shape as usize`,
    // so appending preserves the existing 0/1/2 discriminants.
    Plane,
}

impl Shape {
    /// Every spelling [`Shape::parse`] accepts (`floptle/0082`), for an error
    /// message that names what it takes.
    pub const ACCEPTS: &'static [&'static str] = &["Cube", "Sphere", "Capsule", "Plane"];

    /// Parse a shape name, case-insensitively. `None` for anything else —
    /// `node:setPrimitive("Sphre")` used to make a CUBE and say nothing, which
    /// is a whole different object silently standing where you put it.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cube" | "box" => Some(Self::Cube),
            "sphere" | "ball" => Some(Self::Sphere),
            "capsule" => Some(Self::Capsule),
            "plane" | "quad" => Some(Self::Plane),
            _ => None,
        }
    }
}

/// How fast an entity spins about Y (radians/sec) — a tiny demo behavior.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spin {
    pub speed: f32,
}

/// The collision shape of a [`RigidBody`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    Sphere,
    Capsule,
    /// A box, sized by [`RigidBody::half_extents`].
    Box,
}

/// How a [`RigidBody`] participates in the simulation — the one dropdown that
/// replaces hand-freezing axes and disabling gravity:
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BodyMode {
    /// Fully simulated: gravity, velocity, collisions push it around.
    #[default]
    Dynamic,
    /// TRANSFORM-DRIVEN: never falls or gets pushed — scripts/animation move
    /// the node and the body follows. Dynamic bodies collide WITH it (moving
    /// platforms, elevators, doors that shove the player), raycasts hit it,
    /// and touch events fire. Costs almost nothing per tick (no integration).
    Kinematic,
    /// Baked STATIC geometry: no body at all — just an immovable collider in
    /// the shape below (walls, floors, props). Zero per-tick cost; the
    /// cheapest way to make something solid. (Same as Collidable, but sized
    /// by the body shape instead of the node's visual geometry.)
    Static,
}

/// Puts a node ON RAILS as a celestial body (solar demo S2, `frames` module):
/// during Play the engine assembles all `CelestialBody` nodes into a
/// [`crate::frames::System`], advances space time each tick, and WRITES this
/// node's translation from its Kepler elements — exact analytic orbits, stable
/// at any time-warp. The node also becomes an inverse-square gravity source
/// (µ/r²) with patched-conic SOI dominance.
///
/// `parent` names another CelestialBody NODE; empty = the system root (which
/// stays where the scene put it). Angles are radians; `soi = 0` auto-derives
/// the Laplace radius from the parent's µ.
#[derive(Clone, Debug, PartialEq)]
pub struct CelestialBody {
    /// Gravitational parameter µ = GM (units³/s²). 0 = massless marker.
    pub mu: f64,
    /// Physical (surface) radius, for altitude readouts + impostor scale.
    pub body_radius: f64,
    /// Sphere-of-influence radius; 0 = auto (Laplace) from the parent.
    pub soi: f64,
    /// Name of the parent body's NODE (empty = system root).
    pub parent: String,
    /// Kepler elements around the parent: semi-major axis (negative =
    /// hyperbolic), eccentricity, inclination, longitude of ascending node,
    /// argument of periapsis, mean anomaly at t = 0. Radians.
    pub a: f64,
    pub e: f64,
    pub i: f64,
    pub lan: f64,
    pub arg_pe: f64,
    pub m0: f64,
    /// S8 atmosphere: the sky color seen from inside it. Black + height 0 = none.
    pub atmo_color: [f32; 3],
    /// Atmosphere shell height above the surface (world units); 0 = airless.
    pub atmo_height: f64,
    /// How opaque the sky gets at full depth, 0..1.
    pub atmo_density: f32,
    /// Cloud coverage inside the atmosphere, 0..1 (0 = clear skies).
    pub clouds: f32,
    /// STAR: this body emits light (Lighting `stars` mode). Irradiance at
    /// distance d = `luminosity × 1e6 / d²` — ~36 lights a body 6000 units
    /// out at full strength. 0 = not a star.
    pub luminosity: f32,
    /// The star's light color (only meaningful with `luminosity > 0`).
    pub star_color: [f32; 3],
    /// OCCLUSION CULLING: radius of a solid sphere at this body's center that
    /// geometry is guaranteed never to pierce (a planet's core below its
    /// deepest cave). When > 0, the renderer skips terrain chunks fully hidden
    /// behind it — the far side of a planet stops costing draw calls. 0 = off.
    /// Conservative by contract: set it BELOW anything diggable/carvable.
    pub occluder_radius: f64,
}

impl Default for CelestialBody {
    fn default() -> Self {
        Self {
            mu: 1.0e6,
            body_radius: 30.0,
            soi: 0.0,
            parent: String::new(),
            a: 0.0,
            e: 0.0,
            i: 0.0,
            lan: 0.0,
            arg_pe: 0.0,
            m0: 0.0,
            atmo_color: [0.0, 0.0, 0.0],
            atmo_height: 0.0,
            atmo_density: 1.0,
            clouds: 0.0,
            luminosity: 0.0,
            star_color: [1.0, 0.97, 0.9],
            occluder_radius: 0.0,
        }
    }
}

/// Marks an entity as a physics body, centered on the entity's world
/// translation. Read by `floptle-physics` to build the sim each Play.
/// [`BodyMode`] picks how it simulates (dynamic / kinematic / static).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigidBody {
    pub kind: BodyKind,
    /// Dynamic (simulated) / Kinematic (transform-driven) / Static (baked).
    pub mode: BodyMode,
    pub radius: f32,
    /// Total capsule height (ignored for a sphere).
    pub height: f32,
    /// Half-extents for a `Box` body (ignored for sphere/capsule).
    pub half_extents: [f32; 3],
    /// Bounciness 0..1 (0 = no bounce).
    pub restitution: f32,
    /// Surface friction 0..1 (0 = frictionless).
    pub friction: f32,
    /// Whether the scene's gravity field pulls on this body (false = floats; it still
    /// collides and can be driven by a script).
    pub gravity: bool,
    /// Freeze world-axis translation (x, y, z) — e.g. lock Z for a 2.5D game.
    pub lock_pos: [bool; 3],
    /// Freeze the entity's rotation about each axis (keeps a body upright during play).
    pub lock_rot: [bool; 3],
    /// Rotate the NODE so its local +Y tracks the body's up (−gravity) — characters
    /// walking a radial-gravity planet stand on it visually, and their children
    /// (cameras, held items) inherit the tilt. Smoothed; visual-only (the physics
    /// capsule already follows −gravity regardless). Overrides `lock_rot` when set.
    pub align_up: bool,
    /// Mass in kilogram-ish sim units. Plain bodies ignore it today (the
    /// translational solver is mass-free); it is the mass SHARE of a shape
    /// inside an [`Self::assembly`] compound, where composed mass/CoM/inertia
    /// are what make off-center thrust and contacts behave.
    pub mass: f32,
    /// This node is the ROOT of a COMPOUND ASSEMBLY: one 6-DOF rigid body
    /// built from every descendant node that carries a `RigidBody` (each
    /// becomes an oriented shape at its offset, weighted by its `mass` —
    /// the root's own shape fields are ignored). Multi-part vehicles,
    /// decoupling rockets, breakable structures. Requires `Dynamic` mode.
    pub assembly: bool,
    /// **PUSHBOX ONLY** — the solver never resolves this body's contacts. It
    /// integrates its velocity and nothing else: no gravity, no depenetration
    /// against colliders or terrain, no ground detection, no position locks.
    /// It still exists for raycasts, hulls and overlap queries, which is the
    /// point — it is a box you can hit, not a box the physics engine moves.
    ///
    /// This is the supported profile for rollback
    /// (`docs/rollback-netcode-design.md` §3, §8). Determinism across builds
    /// and platforms is only *expected*, not proven, and the iterative
    /// depenetration relaxation — order-dependent, sampling SDF terrain — is
    /// the part most likely to disagree in the last bit and turn a match into
    /// two different matches. A fighting game does not want it anyway: the
    /// floor, gravity, walls and pushout are integer frame data owned by the
    /// controller script, which is both exact and the genre's actual design.
    ///
    /// The script therefore owns separation. Pair it with
    /// `node.tickX/tickY/tickZ/tickPos` — a position channel that is NOT the
    /// interpolated render transform.
    pub pushbox_only: bool,
}

impl Default for RigidBody {
    fn default() -> Self {
        Self {
            kind: BodyKind::Sphere,
            mode: BodyMode::Dynamic,
            radius: 0.5,
            height: 2.0,
            half_extents: [0.5, 0.5, 0.5],
            restitution: 0.0,
            friction: 0.3,
            gravity: true,
            lock_pos: [false; 3],
            lock_rot: [false; 3],
            align_up: false,
            mass: 1.0,
            assembly: false,
            pushbox_only: false,
        }
    }
}

/// Marks a node (and everything under it) as SWITCHED OFF: it doesn't draw, doesn't
/// collide, and its scripts don't run.
///
/// A marker rather than an `enabled: bool` field, so the common case — every node in
/// every scene ever written — costs nothing and serializes to nothing. Presence = off.
///
/// **The whole subtree goes with it.** Disabling a folder is the useful operation
/// (turn off a room, a variant, a debug rig), and a child that kept drawing under a
/// disabled parent would be the same trap as a hidden parent with visible children.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Disabled;

/// Marks a node (and everything under it) as surviving a scene swap — the
/// DontDestroyOnLoad equivalent. A persistent node keeps its entity, its
/// components, its physics body AND its running scripts: `start` does not
/// re-fire, because the node never stopped existing.
///
/// A marker for the same reason [`Disabled`] is one: presence = persistent, and
/// the ordinary node (which is every node in every scene written so far) costs
/// nothing and serializes to nothing.
///
/// **The whole subtree goes with it.** Carrying a music player across a scene
/// change and leaving its audio source behind would be a trap, and the useful
/// unit — a HUD, a party, a save-game manager — is a folder, not a leaf.
///
/// This is a RUNTIME flag: it is set from a script (`node.persistent = true`),
/// not authored in a scene file. A node is only ever persistent relative to a
/// swap that happens while the game runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Persistent;

/// The scene an additively-loaded node came from — what `scene.unload(name)`
/// removes. Absence means "belongs to the base scene", so the nodes the editor
/// opened are never candidates for unloading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneTag(pub String);

/// Marks a `Matter::Mesh` node as a STATIC collider you can walk on — the editor bakes
/// its triangles (in world space) into the physics sim at Play. The model isn't a
/// dynamic body; it's environment geometry (a level/map). Presence = collidable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MeshCollider;

/// Marks ANY node as a STATIC collider auto-shaped from its geometry — the "collidable"
/// switch. At Play the editor builds the matching static collision shape sized to the
/// node's `Matter` + world transform (Cube → box, Sphere → sphere, Capsule → capsule,
/// Mesh → triangle mesh), so a primitive is collidable WITHOUT a dynamic rigidbody (just
/// like a mesh collider). Resize/reshape it by scaling/rotating the node — the collider
/// tracks the geometry. Presence = collidable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collidable;

/// On-demand terrain generation spec (G2 galaxy streaming): the RON-serialized
/// `PlanetFill` recipe for this Terrain node's field. A body carrying this needs
/// no `.cfield` on disk at all — when something first approaches, the engine
/// generates the field from this spec on a background thread (deterministic per
/// seed), and player edits saved to a save-slot dir take priority over
/// regeneration. Written by the Lua construction API (`node:setTerrainGen{...}`);
/// the payload stays an opaque string here (core doesn't know `PlanetFill`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerrainGen(pub String);

/// Makes a [`Collidable`] node's static collider a **trigger**: bodies pass
/// straight through it (no blocking, no push-out), but overlap still fires the
/// `onTriggerEnter` / `onTriggerStay` / `onTriggerExit` script hooks — the
/// portal / pickup-zone / checkpoint primitive. Lives ALONGSIDE `Collidable`
/// (the Inspector's "trigger" switch on the Collider component); on its own it
/// does nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trigger;

/// Marks a node as carrying **vertex paint** — per-vertex color the brush authored,
/// stored outside the scene (`<project>/paint/<scene>.vpaint`) because per-vertex
/// arrays have no business in a `.ron`.
///
/// `id` is a STABLE per-node key, not an `Entity`: undo respawns the whole `World`, so
/// entity handles don't survive it — the same reason `Matter::Terrain { id }` exists.
/// The paint file keys off this id, and the renderer resolves it to a base offset in
/// the `vpaint` store.
///
/// This is an ADDITIVE component rather than a `Matter` field on purpose. Paint is
/// orthogonal to what a node *is* (a Mesh and a Primitive are both paintable), and
/// every primitive of a shape shares ONE `MeshId` — so paint cannot live on the
/// geometry. See `docs/vertex-paint-proposal.md` §3.1/§9.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexPaint {
    pub id: u32,
}

/// TEXTURE painting: this node carries a hand-painted texture (per-part paint images on a
/// unique per-triangle atlas — see the editor's `paint_tex`). A stable id (not `Entity`,
/// which `restore()` invalidates) keys the editor's image store, exactly like
/// [`VertexPaint`] — so undo survives a World rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexturePaint {
    pub id: u32,
}

/// Attaches a layered animation controller asset (`*.actl.ron`) to a node. The
/// runtime it drives lives editor/runtime-side; this is just the reference —
/// the same discipline as `Matter::Mesh { asset_path }`. On a rigged Mesh node
/// it poses the model's parts; on any other node it animates the node itself +
/// its descendants (matched by scene `Name`) — cutscenes, doors, platforms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnimController {
    /// Controller asset key: project-relative path without extension
    /// (`animation_controllers/Player`).
    pub asset: String,
}

/// Attaches a particle effect asset (`*.vfx.ron`) to a node — the node becomes
/// the effect's emitter transform. Same reference discipline as [`AnimController`]:
/// the timeline/sim runtime lives editor/runtime-side (`floptle-vfx`); this is
/// just the asset key plus how playback starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticleSystem {
    /// Effect asset key: project-relative path without extension (`vfx/360Slash`).
    pub asset: String,
    /// Start playing the moment Play begins (`false` = a script triggers it).
    pub play_on_start: bool,
}

/// Whether a node's geometry is drawn. A node with **no** `Visible` component renders
/// normally (visible is the default); attaching `Visible(false)` hides its mesh/shape
/// (it still has a transform, physics, and children). Scripts toggle it with
/// `node.visible = true/false` to show/hide visuals on the fly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visible(pub bool);

impl Default for Visible {
    fn default() -> Self {
        Visible(true)
    }
}

/// A scene's lighting, held on a single mandatory "Lighting" node every scene
/// carries: a directional key light plus flat ambient. These are plain fields a
/// script can read and write to drive game-time light changes; the renderer turns
/// them into the frame's light. `direction` need not be unit — the renderer
/// normalizes it.
///
/// `stars` switches the key light to STARS MODE: the directional light turns
/// off and every [`CelestialBody`] with `luminosity > 0` becomes a real point
/// light source — light radiates from each star's world position with
/// inverse-square falloff, so terminators wrap planets, shadow directions
/// line up radially, far sides go genuinely dark, and a binary system just
/// works (up to 4 stars reach the shaders, brightest-at-camera first).
/// `direction`/`color` are ignored while it's on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Light {
    pub direction: [f32; 3],
    /// Stars mode: luminous celestial bodies ARE the key lights.
    pub stars: bool,
    pub color: [f32; 3],
    pub ambient: [f32; 3],
    /// **The base light every 2D surface gets**, before any 2D light is added.
    ///
    /// Separate from `ambient` above, which is the 3D one, because the two want
    /// opposite defaults. 3D ambient is a dim fill under a key light. This is
    /// the whole light a flat scene has until you place one — so it defaults to
    /// WHITE, and adding a light to a 2D scene can only ever make it brighter.
    ///
    /// Turning it down is how you get a dark room for a torch to carve a circle
    /// out of. That has to be the deliberate act: a first light that blacked out
    /// the level would read as the feature being broken, which is exactly how it
    /// was reported.
    pub ambient_2d: [f32; 3],
    /// Brightness multiplier on the key (directional) light color.
    pub intensity: f32,
    /// Sun shadows: the field is marched from each shaded point toward the light
    /// (SDF soft shadows), so terrain/blobs cast on everything and meshes cast via
    /// their collider proxy shapes. All the knobs below only apply when `true`.
    pub shadows: bool,
    /// 0 = razor-hard edge (PS1) … 1 = dreamy-soft penumbra. Maps to the penumbra
    /// sharpness `k` in the shadow march (analytic softness — no blur kernels).
    pub shadow_softness: f32,
    /// How dark full shadow gets, 0..1 (1 = the directional light fully blocked;
    /// ambient still fills, so the scene never goes pitch black).
    pub shadow_strength: f32,
    /// Shadows darken *toward this color* instead of plain black — purple dusk,
    /// sepia, horror green. Black = neutral darkening.
    pub shadow_tint: [f32; 3],
    /// 0 = smooth penumbra; 2..=8 = posterize it into that many bands (toon/retro).
    pub shadow_quantize: u32,
    /// Bayer-dither the penumbra (pairs with `shadow_quantize` + retro mode for the
    /// classic PS1 dithered shadow edge).
    pub shadow_dither: bool,
    /// Max distance (world units) a shadow ray marches before giving up — a perf
    /// fence; far geometry simply stops casting past it.
    pub shadow_distance: f32,

    /// Depth fog: blend everything toward `fog_color` between `fog_start` and
    /// `fog_end` world units from the camera. Dirt-cheap (one mix per fragment) and
    /// off by default. The skybox stays crisp, so match `fog_color` to the horizon
    /// (or the background color) to avoid a seam.
    pub fog: bool,
    pub fog_color: [f32; 3],
    /// World distance where fog begins (fully clear nearer than this).
    pub fog_start: f32,
    /// World distance where fog is full (fully `fog_color` past this).
    pub fog_end: f32,
    /// Dither the fog gradient to hide 8-bit banding on long, slow ramps.
    pub fog_dither: bool,
    /// Dither amplitude (0..1); scaled to a sub-percent nudge of the fog factor.
    pub fog_dither_strength: f32,
    /// VOLUMETRIC mode: instead of a flat distance ramp, march real fog media —
    /// a height-bounded layer with drifting noise, so hills poke out of ground
    /// mist and beams of distance thicken naturally. Uses `fog_color` and the
    /// dither settings; `fog_start`/`fog_end` don't apply.
    pub fog_volumetric: bool,
    /// Media density per world unit (how quickly things disappear into it).
    pub fog_density: f32,
    /// World height (y) of the fog layer's top — media fills below this.
    pub fog_height: f32,
    /// Softness of the layer's top edge in world units (bigger = mistier boundary).
    pub fog_falloff: f32,
    /// How much drifting noise breaks up the media (0 = uniform, 1 = patchy).
    pub fog_noise: f32,
    /// Noise feature size, world units per pattern repeat (bigger = broader wisps).
    pub fog_noise_scale: f32,
}

impl Default for Light {
    fn default() -> Self {
        Self {
            direction: [0.4, 0.9, 0.45],
            stars: false,
            color: [1.0, 0.98, 0.92],
            ambient: [0.12, 0.12, 0.16],
            ambient_2d: [1.0, 1.0, 1.0],
            intensity: 1.0,
            shadows: true,
            shadow_softness: 0.35,
            shadow_strength: 1.0,
            shadow_tint: [0.0, 0.0, 0.0],
            shadow_quantize: 0,
            shadow_dither: false,
            shadow_distance: 150.0,
            fog: false,
            fog_color: [0.6, 0.65, 0.72],
            fog_start: 40.0,
            fog_end: 200.0,
            fog_dither: false,
            fog_dither_strength: 0.5,
            fog_volumetric: false,
            fog_density: 0.02,
            fog_height: 6.0,
            fog_falloff: 8.0,
            fog_noise: 0.5,
            fog_noise_scale: 24.0,
        }
    }
}

/// Whether a node's collider shape casts sun shadows (as a proxy occluder in the
/// shadow march). A node with **no** `CastShadow` component casts by default —
/// attach `CastShadow(false)` to opt a collider out of shadowing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CastShadow(pub bool);

impl Default for CastShadow {
    fn default() -> Self {
        CastShadow(true)
    }
}

/// What an entity is made of, interpreted by the renderer. Placed via the
/// entity's `Transform`; deliberately free of GPU handles.
#[derive(Clone, Debug, PartialEq)]
pub enum Matter {
    /// A lit, textured polygon primitive.
    Primitive { shape: Shape, color: [f32; 3] },
    /// Raymarched analytic SDF "blob" (morphing smin-blended spheres).
    Blob { scale: f32 },
    /// An imported polygon mesh (glTF), referenced by its asset path. The renderer
    /// (editor) maps the path to its registered GPU mesh parts.
    Mesh { asset_path: String },
    /// A group / "empty" — renders nothing, but has a transform and can parent other
    /// nodes (a folder for organizing the scene, or a rig root like a player).
    Empty,
    /// An editable map-building polygon mesh (blockout shapes, per-face
    /// materials, vertex/edge/face modeling — docs/map-tools-proposal.md).
    /// Like `Terrain`, the geometry does NOT live on the component: `id` is a
    /// stable per-mesh key into the editor's map store, persisted to a
    /// per-scene sidecar (`maps/<scene>.map.ron`), because Entity indices die
    /// on undo/reload and big data can't ride the per-frame scene snapshot.
    MapMesh { id: u32 },
    /// Editable SDF terrain — like a blob, but a sculptable/paintable voxel field.
    /// The transform places its volume; the field data lives alongside the scene.
    /// `id` is a stable per-terrain key (Entity indices aren't stable across load),
    /// so each terrain's field file + combine slot can be matched back on reload.
    Terrain { id: u32 },
    /// A camera viewpoint — its transform is the camera pose; `fov_y` is the vertical
    /// field of view in radians. One camera holds play-mode authority at a time
    /// (`active`); the gameplay view renders from it, switchable for cutscenes.
    ///
    /// A non-empty `target` turns the camera into a RENDER TARGET (A1): every
    /// frame it renders the world into a live texture addressable as
    /// `rt:<target>` from any material or UI image — cockpit screens, security
    /// monitors, mirrors. `cull_mask` is a bitmask over the project's layers
    /// (bit i = layer i visible; `u32::MAX` = everything) applied wherever
    /// this camera renders — the game view for the active camera, the target
    /// texture for a target camera.
    ///
    /// `target_w`/`target_h` are the target texture's size in pixels and
    /// `target_hz` how often it redraws (0 = every frame). A minimap that only
    /// needs 256×256 at 10 Hz costs a sixth of what it cost when every target
    /// was 480×270 every frame (`floptle/0078`). Use [`Matter::TARGET_W`],
    /// [`Matter::TARGET_H`] for the defaults.
    ///
    /// `ortho` switches the camera to an **orthographic** projection of
    /// `ortho_height` world units, top to bottom, at every distance — and
    /// `fov_y` then means nothing, because there is no angle. This is what a 2D
    /// game wants: under perspective a tilemap two units further back is drawn
    /// slightly smaller, so a parallax layer changes scale as well as speed and
    /// two tilemaps at different Z cannot line up. It is also what a strategy or
    /// isometric camera wants, and what a technical shot wants.
    ///
    /// The height is the FULL height, not a half-extent — the same number
    /// [`floptle_render::Projection::Orthographic`] takes, so there is no factor
    /// of two hiding at the boundary. Width follows from the viewport's aspect.
    Camera {
        fov_y: f32,
        active: bool,
        target: String,
        cull_mask: u32,
        target_w: u32,
        target_h: u32,
        target_hz: f32,
        /// Orthographic rather than perspective. `fov_y` is unused when set.
        ortho: bool,
        /// The world-space height the view covers when `ortho`. Ignored otherwise.
        ortho_height: f32,
    },
    /// A placeable point/omni light. Its world position is the node's transform
    /// translation; `range` is the radius at which its contribution falls to ~zero.
    /// (The scene's single directional/ambient key stays the special `Light` node.)
    PointLight { color: [f32; 3], intensity: f32, range: f32 },
    /// A gravity source for the physics sim — `Down` for normal-style level gravity,
    /// `Radial` for a planet (Mario-Galaxy) gravity well centered on the node.
    GravityVolume { mode: GravityMode, strength: f32, radius: f32 },
    /// A body of water (`floptle/0038`): a planet's sea (`Sea`, a sphere of
    /// `radius` about the node) or a lake / tank / flooded room (`Pool`, an
    /// oriented box of `half_extents` — the node's rotation orients it, so a
    /// tilted tank has a tilted surface).
    ///
    /// The node's transform places it. Everything else is what the water *is*:
    /// how dense (whether a given hull floats), how much it resists motion, how
    /// it looks from inside, and whether it is currently frozen — because a
    /// frozen sea should be a state of this node rather than a second kind of
    /// node the game has to keep in step with it.
    WaterVolume {
        kind: WaterKind,
        /// `Sea`: the sea's radius. Ignored by `Pool`.
        radius: f32,
        /// `Pool`: half-extents of the box. Ignored by `Sea`.
        half_extents: [f32; 3],
        /// kg/m³. Fresh water ≈ 1000, seawater ≈ 1025, and an alien ocean is
        /// whatever you say it is — a denser sea floats heavier hulls.
        density: f32,
        /// Quadratic drag coefficient — how hard the water resists moving
        /// through it. Quadratic is what makes a gentle touchdown float and a
        /// 60 m/s belly-flop stop hard, without either being a special case.
        drag: f32,
        /// Angular drag — what stops a dropped craft spinning forever.
        angular_drag: f32,
        /// FROZEN: no buoyancy, no drag, no underwater state. Pair with a
        /// collider for the surface and the sea becomes walkable ground.
        frozen: bool,
        /// The colour everything fades toward when the camera is under. This is
        /// where the wrongness of an alien ocean lives.
        tint: [f32; 3],
        /// How far you can see underwater, in metres. The scene's own fog is
        /// replaced by this while submerged, so meshes, terrain, SDF matter and
        /// particles all go murky together instead of one of them staying
        /// crisp.
        visibility: f32,
    },
    /// An authored SDF shape (ADR-0007 Sdf stage): its Material's `.flsl`
    /// shader IS the geometry, raymarched as part of the scene field (up to 4
    /// per scene). `radius` bounds the shape in LOCAL units — the march,
    /// shadows and spans all key off it, so keep it snug. Visual only for now
    /// (no collision until the CPU field evaluator lands — proposal §7.3).
    FieldShape { radius: f32 },
    /// A grid of spritesheet cells drawn as **one mesh, one draw call**
    /// (`floptle/0058`).
    ///
    /// The sheet comes from the node's [`crate::Material`] — its `texture`,
    /// `sheet_cols`/`sheet_rows` and `filter`. This component is only the grid,
    /// so a tilemap is dressed exactly like every other surface and a project
    /// does not learn a second way to say "this texture, chopped this way".
    /// (The Material's own `cell` is unused: each tile carries its own UVs.)
    ///
    /// **Why this exists at all.** A tilemap built from one quad per tile has a
    /// hairline of background between tiles that opens and closes as the camera
    /// moves — each quad's edge is computed through its own transform, so two
    /// touching edges land either side of a pixel boundary independently. Here
    /// every tile is a quad in one vertex buffer whose corners are computed by
    /// the same expression, so a shared edge is *bit-identical* on both sides
    /// and the rasterizer has no gap to fill. That is a structural fix; the
    /// alternative games reach for — overlapping tiles by a few percent — only
    /// hides it, and only for tiles that happen to be opaque at the edge.
    ///
    /// `data` is row-major, `rows * cols` long, from the top-left.
    /// [`EMPTY_TILE`] leaves a hole rather than drawing cell 0.
    /// `tileset` names the project-relative `.tileset.ron` that says what each
    /// cell of the sheet MEANS — whether it collides, what it is tagged, which
    /// autotile group it belongs to, whether it animates. Empty = none, and the
    /// tilemap is then art only.
    ///
    /// It is a path rather than inline data because those answers belong to the
    /// SHEET, not to this grid: tick "solid" on the brick once and every brick in
    /// every scene collides, including the ones already placed. Inline, the answer
    /// would be recorded per node and a level built last month would keep the old
    /// one.
    ///
    /// Each square of `data` is a packed cell index + orientation — see
    /// [`crate::tile`]. A grid written before orientations existed is a list of
    /// bare indices and still means exactly what it did.
    Tilemap {
        cols: u32,
        rows: u32,
        /// World size of one tile's edge.
        tile: f32,
        /// Row-major packed squares (cell index + orientation).
        data: Vec<u32>,
        /// Project-relative path to this map's `.tileset.ron`, or empty.
        tileset: String,
    },
    /// N sprites drawn from one node, each with its own position, rotation,
    /// scale, sheet cell **and tint** (`floptle/0058`).
    ///
    /// Like [`Tilemap`](Self::Tilemap) the sheet is the node's Material. The
    /// sprites themselves are a runtime-only [`Sprites`] component, written per
    /// frame from Lua — they are this frame's bullets, not scene content, and a
    /// saved scene full of them would describe a moment that has passed.
    ///
    /// The tint is the point. Colour otherwise lives on the Material, which a
    /// pool of quads shares, so a game cannot flash one enemy red — it blinks
    /// the sprite off instead, which is the wrong effect chosen because the
    /// right one was unreachable.
    SpriteBatch {
        /// World size of one sprite's edge, before its own scale.
        size: f32,
    },
    /// The scene's environment background — a face-inverted sphere of radius `size`
    /// drawn behind everything. `color` is the solid sky color (grey by default); when
    /// `texture` is set it's sampled equirectangularly (seamless loop) and multiplied by
    /// `tint`. The node's transform rotation orients the sky (a script can spin it).
    /// `shader` (a project-relative `.flsl` Sky-stage path) overrides the solid/texture
    /// look entirely: it computes the environment color per ray direction (a procedural
    /// sky). `shader_params` overrides that shader's exposed uniforms by name (the
    /// Inspector's sky knobs — absent names use the `.flsl` defaults), exactly like a
    /// Material's `shader_params`. Serialized via `MatterDoc::Skybox` (which
    /// `#[serde(default)]`s it so old scenes still load).
    Skybox {
        color: [f32; 3],
        size: f32,
        texture: Option<String>,
        tint: [f32; 3],
        shader: Option<String>,
        shader_params: std::collections::BTreeMap<String, [f32; 4]>,
    },
    /// The scene's post-processing chain — a mandatory scene node (self-healed on
    /// load, like the Skybox), so every scene tunes its own look. `enabled` gates
    /// the whole chain; each effect then has its own switch and knobs. `ao` picks
    /// how ambient occlusion is computed (screen-space by default; SDF samples the
    /// real distance field). The node's transform is unused.
    PostProcess {
        enabled: bool,
        bloom: bool,
        bloom_threshold: f32,
        bloom_intensity: f32,
        vignette: bool,
        vignette_strength: f32,
        vignette_radius: f32,
        ao: AoMode,
        /// How dark full occlusion gets (0 = off, 1 = black creases).
        ao_strength: f32,
        /// Occlusion reach in world units.
        ao_radius: f32,
        /// Posterize the final image to this many color levels per channel (a limited
        /// palette / banded retro look). 0 or 1 = off; 2.. = on. Runs last, at the
        /// composited (retro) resolution, so bands land on the same chunky pixels.
        posterize_bands: u32,
        /// Ordered-dither the posterize so smooth gradients don't hard-step.
        posterize_dither: bool,
    },
}

impl Matter {
    /// The height a fresh orthographic camera covers, in world units.
    ///
    /// Ten, because a tilemap's default tile is one unit: a new 2D camera frames
    /// ten tiles vertically, which is a room. A number derived from the tile size
    /// beats a round number that happens to look like a room on the machine it
    /// was chosen on.
    pub const ORTHO_HEIGHT: f32 = 10.0;
    /// The smallest orthographic height. Not zero: a zero-height projection
    /// matrix is singular, and every ray reconstructed through its inverse comes
    /// back NaN — which shows up as a black screen rather than as a small camera.
    pub const ORTHO_MIN: f32 = 1e-3;
    /// The largest. Past this the depth range a single-precision matrix can
    /// resolve is coarser than the things being drawn, so the picture z-fights
    /// instead of zooming out.
    pub const ORTHO_MAX: f32 = 1.0e6;

    /// Every spelling `projection = ...` accepts, and the list an error prints.
    ///
    /// One list, read by both [`parse_projection`](Self::parse_projection) and
    /// the message — `floptle/0082`'s rule, because the two drifting is how
    /// `pin = "topCenter"` ended up silently meaning top-left.
    pub const PROJECTION_ACCEPTS: &'static [&'static str] =
        &["perspective", "persp", "3d", "orthographic", "ortho", "2d"];

    /// Whether a projection name means orthographic. `None` for a name that is
    /// neither, so the caller can refuse it by name rather than default it.
    pub fn parse_projection(s: &str) -> Option<bool> {
        match s.trim().to_ascii_lowercase().as_str() {
            "perspective" | "persp" | "3d" => Some(false),
            "orthographic" | "ortho" | "orthogonal" | "2d" => Some(true),
            _ => None,
        }
    }

    /// An orthographic height clamped into what the projection can express.
    ///
    /// NaN is handled explicitly rather than by `clamp` (which propagates it):
    /// an ortho height that arrived as NaN through some arithmetic would make the
    /// whole view matrix NaN, and a camera that renders nothing at all is much
    /// harder to trace back than one that snapped to its default.
    pub fn clamp_ortho_height(h: f32) -> f32 {
        if !h.is_finite() {
            return Self::ORTHO_HEIGHT;
        }
        h.clamp(Self::ORTHO_MIN, Self::ORTHO_MAX)
    }

    /// Default render-target width, in pixels (`Matter::Camera`).
    pub const TARGET_W: u32 = 480;
    /// Default render-target height, in pixels (`Matter::Camera`).
    pub const TARGET_H: u32 = 270;
    /// Smallest render-target edge. Below this a target is not a picture, and a
    /// zero would be an invalid texture.
    pub const TARGET_MIN: u32 = 8;
    /// Largest render-target edge the engine will allocate for a camera. 4096²
    /// in the surface format plus depth is ~100 MB; anything past that is a
    /// mistake rather than a choice, and refusing beats a device-lost.
    pub const TARGET_MAX: u32 = 4096;
    /// How many live render targets one scene may hold. Past this the extras
    /// are dropped — loudly, by name (`floptle/0078`).
    pub const TARGET_LIMIT: usize = 8;

    /// A render target's size, clamped to what the engine will allocate.
    pub fn clamp_target_size(w: u32, h: u32) -> (u32, u32) {
        (w.clamp(Self::TARGET_MIN, Self::TARGET_MAX), h.clamp(Self::TARGET_MIN, Self::TARGET_MAX))
    }

    /// The default skybox: solid mid-grey, a large radius, no texture.
    pub fn default_skybox() -> Self {
        Matter::Skybox {
            color: [0.5, 0.5, 0.52],
            size: 500.0,
            texture: None,
            tint: [1.0, 1.0, 1.0],
            shader: None,
            shader_params: std::collections::BTreeMap::new(),
        }
    }

    /// A default body of water: a fresh-water pool you could swim in.
    ///
    /// `Pool` rather than `Sea` as the starting shape because a new water node
    /// is nearly always a test — dropping a sphere-sea into a level makes the
    /// whole scene wet, which is a confusing first thing to see.
    pub fn default_water() -> Self {
        Matter::WaterVolume {
            kind: WaterKind::Pool,
            radius: 10.0,
            half_extents: [5.0, 2.0, 5.0],
            density: 1000.0,
            drag: 1.0,
            angular_drag: 1.0,
            frozen: false,
            tint: [0.10, 0.32, 0.38],
            visibility: 28.0,
        }
    }

    /// The default post-processing node: chain on, screen-space ambient occlusion
    /// at a gentle strength, bloom and vignette off (matching the old project-wide
    /// defaults).
    pub fn default_post_process() -> Self {
        Matter::PostProcess {
            enabled: true,
            bloom: false,
            bloom_threshold: 1.0,
            bloom_intensity: 0.7,
            vignette: false,
            vignette_strength: 0.5,
            vignette_radius: 0.7,
            ao: AoMode::ScreenSpace,
            ao_strength: 0.7,
            ao_radius: 0.5,
            posterize_bands: 0,
            posterize_dither: false,
        }
    }
}

/// How a [`Matter::PostProcess`] node computes ambient occlusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AoMode {
    /// No ambient occlusion.
    Off,
    /// Screen-space AO (SSAO): a post pass over the depth buffer. Cheap, and it
    /// darkens everything on screen — meshes and SDF matter alike. The default.
    ScreenSpace,
    /// Geometric AO sampled from the actual SDF field along the surface normal —
    /// "true" occlusion with no screen-space artifacts. Everything receives it -
    /// the raster pass marches the same field for its mesh fragments - but only
    /// SDF matter (terrain/blobs) *occludes*; meshes aren't in the field.
    Sdf,
}

/// The shape of a [`Matter::WaterVolume`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WaterKind {
    /// A planet's sea: a sphere about the node. "Up" is different at every
    /// point on it, which is why this is not a very large flat pool.
    #[default]
    Sea,
    /// A lake, a tank, a flooded room: an oriented box with a flat top. Its
    /// sides are WALLS — standing beside a pool at the same height as its water
    /// is not standing in it.
    Pool,
}

impl WaterKind {
    pub fn label(self) -> &'static str {
        match self {
            WaterKind::Sea => "Sea (sphere)",
            WaterKind::Pool => "Pool (box)",
        }
    }
    pub const ALL: [WaterKind; 2] = [WaterKind::Sea, WaterKind::Pool];
}

/// How a [`Matter::GravityVolume`] pulls bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GravityMode {
    /// Constant downward (−Y) gravity everywhere — a normal-style game's level gravity.
    Down,
    /// Radial pull toward the node — a planet. `radius` bounds the gravity well.
    Radial,
}

/// The absolute (world) transform of `e`: its local [`Transform`] composed under
/// every ancestor's, so a parent's placement carries its descendants. Roots return
/// their own transform. The walk is bounded to guard against accidental cycles.
/// Is this node switched off — either itself, or because something above it is?
///
/// [`Disabled`] is inherited, and it is inherited HERE rather than pushed down into
/// children on toggle: a node that stored its own resolved state would need every
/// re-parent, spawn and paste to remember to recompute it, and the one that forgot
/// would leave an invisible node nobody could turn back on. Walking up is cheap
/// (scene depth, bounded like `world_transform`) and cannot go stale.
pub fn is_disabled(world: &crate::ecs::World, e: crate::ecs::Entity) -> bool {
    let mut cur = e;
    for _ in 0..64 {
        if world.get::<Disabled>(cur).is_some() {
            return true;
        }
        let Some(Parent(p)) = world.get::<Parent>(cur).copied() else { break };
        cur = p;
    }
    false
}

/// True if `e` or any ancestor is marked [`Persistent`] — the subtree rule, the
/// same walk [`is_disabled`] does and for the same reason: the useful unit is a
/// folder, and a child left behind when its parent survived would be a trap.
pub fn is_persistent(world: &crate::ecs::World, e: crate::ecs::Entity) -> bool {
    let mut cur = e;
    for _ in 0..64 {
        if world.get::<Persistent>(cur).is_some() {
            return true;
        }
        let Some(Parent(p)) = world.get::<Parent>(cur).copied() else { break };
        cur = p;
    }
    false
}

pub fn world_transform(world: &crate::ecs::World, e: crate::ecs::Entity) -> crate::transform::Transform {
    use crate::transform::Transform;
    let mut t = world.get::<Transform>(e).copied().unwrap_or(Transform::IDENTITY);
    let mut cur = e;
    for _ in 0..64 {
        let Some(Parent(p)) = world.get::<Parent>(cur).copied() else { break };
        let plocal = world.get::<Transform>(p).copied().unwrap_or(Transform::IDENTITY);
        t = plocal.mul_transform(&t);
        cur = p;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::World;
    use crate::math::DVec3;
    use crate::transform::Transform;

    #[test]
    fn parent_carries_child() {
        let mut w = World::default();
        let p = w.spawn();
        w.insert(p, Transform::from_translation(DVec3::new(2.0, 0.0, 0.0)));
        let c = w.spawn();
        w.insert(c, Transform::from_translation(DVec3::new(0.0, 1.0, 0.0)));
        w.insert(c, Parent(p));
        // child's local (0,1,0) under parent at (2,0,0) -> world (2,1,0)
        let wt = world_transform(&w, c);
        assert!((wt.translation - DVec3::new(2.0, 1.0, 0.0)).length() < 1e-9, "{:?}", wt.translation);
        // grandchild stacks too
        let g = w.spawn();
        w.insert(g, Transform::from_translation(DVec3::new(0.0, 0.0, 3.0)));
        w.insert(g, Parent(c));
        let gt = world_transform(&w, g);
        assert!((gt.translation - DVec3::new(2.0, 1.0, 3.0)).length() < 1e-9, "{:?}", gt.translation);
    }

    #[test]
    fn parent_rotation_carries_child() {
        use crate::math::{Quat, Vec3};
        let mut w = World::default();
        let p = w.spawn();
        w.insert(
            p,
            Transform {
                rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                ..Transform::IDENTITY
            },
        );
        let c = w.spawn();
        w.insert(c, Transform::from_translation(DVec3::new(1.0, 0.0, 0.0)));
        w.insert(c, Parent(p));
        // +X spun 90° about Y → -Z, so the child orbits to ~(0,0,-1).
        let wt = world_transform(&w, c);
        assert!((wt.translation - DVec3::new(0.0, 0.0, -1.0)).length() < 1e-5, "{:?}", wt.translation);
        // and the child inherits the parent's orientation.
        assert!((wt.rotation * Vec3::Z - (Quat::from_rotation_y(std::f32::consts::FRAC_PI_2) * Vec3::Z)).length() < 1e-5);
    }
}

#[cfg(test)]
mod sorting_tests {
    use super::*;

    /// A scene that never opts in must draw at exactly the Z it always did —
    /// not nearly. A default that offsets by a hair would move every existing
    /// 2D scene by an amount too small to see and big enough to flip a tie.
    #[test]
    fn the_default_layer_at_order_zero_is_no_offset_at_all() {
        assert_eq!(sorting_offset(0, 0), 0.0);
        assert_eq!(Sorting::default().order, 0);
        assert!(Sorting::default().layer.is_empty());
    }

    /// Later layers draw in front, and order breaks ties inside a layer.
    #[test]
    fn a_later_layer_is_in_front_and_order_breaks_the_tie() {
        assert!(sorting_offset(1, 0) > sorting_offset(0, 0));
        assert!(sorting_offset(0, 1) > sorting_offset(0, 0));
        assert!(sorting_offset(0, 0) > sorting_offset(0, -1));
    }

    /// A big order must never climb into the next layer. A sorting layer that
    /// leaks is worse than none: it is correct until one scene trips it, and
    /// then it is a mystery.
    #[test]
    fn order_cannot_climb_out_of_its_layer() {
        for order in [i32::MAX, 100_000, 4096, 64, i32::MIN, -100_000] {
            let here = sorting_offset(3, order);
            assert!(
                here < sorting_offset(4, i32::MIN),
                "order {order} in layer 3 reached layer 4"
            );
            assert!(
                here > sorting_offset(2, i32::MAX),
                "order {order} in layer 3 fell into layer 2"
            );
        }
    }

    /// Two different (layer, order) pairs must never land on the same Z, or the
    /// tie is back exactly where this exists to remove it. Exact powers of two
    /// are what makes this hold rather than nearly hold.
    #[test]
    fn no_two_positions_collapse_onto_one_depth() {
        let mut seen = std::collections::HashSet::new();
        for rank in 0..8u32 {
            for order in -32..32i32 {
                assert!(
                    seen.insert(sorting_offset(rank, order).to_bits()),
                    "layer {rank} order {order} collides with something already placed"
                );
            }
        }
    }
}

#[cfg(test)]
mod lighting_2d_tests {
    use super::*;

    fn light(flat_camera: bool) -> Lit2DFacts {
        Lit2DFacts { emits: true, flat_matter: false, flat_camera }
    }
    fn tilemap(flat_camera: bool) -> Lit2DFacts {
        Lit2DFacts { emits: false, flat_matter: true, flat_camera }
    }
    fn mesh(flat_camera: bool) -> Lit2DFacts {
        Lit2DFacts { emits: false, flat_matter: false, flat_camera }
    }

    /// The requirement Ty stated: *"if I'm developing a 3D scene I shouldn't be
    /// worried about accidentally setting something as 2D because of an
    /// incorrect engine inference."* Nothing in an ordinary 3D scene infers 2D.
    #[test]
    fn nothing_in_a_perspective_scene_is_inferred_2d() {
        for facts in [light(false), mesh(false)] {
            assert!(!infers_2d(facts).0, "{facts:?} was inferred 2D in a 3D scene");
        }
    }

    /// …and the same in reverse: a flat scene does not drag a mesh onto the 2D
    /// path. Mixing the two is deliberate, never discovered.
    #[test]
    fn a_mesh_stays_3d_even_in_a_flat_scene() {
        assert!(!infers_2d(mesh(true)).0);
        assert!(infers_2d(light(true)).0, "the light in that scene IS 2D");
        assert!(infers_2d(tilemap(true)).0);
        assert!(infers_2d(tilemap(false)).0, "a tilemap is flat whatever the camera is");
    }

    /// A stated flag is never re-decided. This is the whole reason the mode has
    /// three values instead of two: a scene that changes shape around a node
    /// must not change what the author said about it.
    #[test]
    fn saying_so_beats_every_inference_in_both_directions() {
        for facts in [light(true), light(false), tilemap(true), mesh(false)] {
            assert!(resolve_2d(Lit2D::Yes, facts).0, "{facts:?} refused an explicit 2D");
            assert!(!resolve_2d(Lit2D::No, facts).0, "{facts:?} refused an explicit 3D");
            assert_eq!(resolve_2d(Lit2D::Auto, facts).0, infers_2d(facts).0);
        }
    }

    /// Every answer carries a reason, because the Inspector prints it and the
    /// design rests on the inference being inspectable rather than merely
    /// correct.
    #[test]
    fn every_answer_says_why() {
        for mode in Lit2D::ALL {
            for facts in [light(true), light(false), tilemap(true), mesh(true)] {
                let (_, why) = resolve_2d(mode, facts);
                assert!(!why.is_empty(), "{mode:?} on {facts:?} decided silently");
            }
        }
    }

    /// A light with no layer list reaches everything — a new light that lit
    /// nothing until a list was filled in would read as a broken light.
    #[test]
    fn a_light_that_names_no_layers_reaches_all_of_them() {
        let all = Lighting2D::default();
        for layer in ["", DEFAULT_SORTING_LAYER, "Background", "Characters"] {
            assert!(all.reaches(layer), "{layer:?}");
        }
    }

    /// Naming layers restricts it to those, and the empty name IS the default
    /// layer — a node that never picked one and a node that picked "Default"
    /// are the same node, so a light must not tell them apart.
    #[test]
    fn naming_layers_restricts_a_light_to_them() {
        let torch = Lighting2D {
            mode: Lit2D::Auto,
            layers: vec!["Terrain".into(), DEFAULT_SORTING_LAYER.into()],
        };
        assert!(torch.reaches("Terrain"));
        assert!(torch.reaches(DEFAULT_SORTING_LAYER));
        assert!(torch.reaches(""), "the unset layer is the default layer");
        assert!(!torch.reaches("Background"), "the background must stay flat");
    }

    /// Under `Auto` a tilemap casts from the collision it already has, so a
    /// level's collision IS its light occlusion and the two cannot drift.
    #[test]
    fn a_solid_tilemap_casts_without_a_second_authoring_step() {
        assert!(resolve_shadow_2d(Cast2D::Auto, true, true).0);
        assert!(!resolve_shadow_2d(Cast2D::Auto, true, false).0, "not collidable, nothing to cast");
        assert!(!resolve_shadow_2d(Cast2D::Auto, false, true).0, "a sprite does not cast by default");
        // …and anything can be made to cast, or stopped, in one tick.
        assert!(resolve_shadow_2d(Cast2D::Yes, false, false).0);
        assert!(!resolve_shadow_2d(Cast2D::No, true, true).0);
        for cast in Cast2D::ALL {
            assert!(!resolve_shadow_2d(cast, true, true).1.is_empty());
        }
    }

    /// An enum parser and the list of values it accepts have to be the same
    /// code, or the error message names spellings that do not work
    /// (`floptle/0082`).
    #[test]
    fn every_accepted_spelling_parses_and_every_value_round_trips() {
        for s in Lit2D::ACCEPTS {
            assert!(Lit2D::parse(s).is_some(), "Lit2D rejects its own {s:?}");
        }
        for v in Lit2D::ALL {
            assert_eq!(Lit2D::parse(v.name()), Some(v));
        }
        assert_eq!(Lit2D::parse("nonsense"), None);
        for s in Cast2D::ACCEPTS {
            assert!(Cast2D::parse(s).is_some(), "Cast2D rejects its own {s:?}");
        }
        for v in Cast2D::ALL {
            assert_eq!(Cast2D::parse(v.name()), Some(v));
        }
        assert_eq!(Cast2D::parse("nonsense"), None);
    }

    /// The default of every piece is "nothing has changed": a scene that has
    /// never heard of 2D lighting behaves exactly as it did.
    #[test]
    fn the_default_of_everything_is_what_a_scene_already_had() {
        assert_eq!(Lighting2D::default().mode, Lit2D::Auto);
        assert!(Lighting2D::default().layers.is_empty());
        assert_eq!(Shadow2D::default().0, Cast2D::Auto);
    }
}
