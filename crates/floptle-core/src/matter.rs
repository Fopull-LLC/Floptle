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
#[derive(Clone, Debug, PartialEq)]
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
    /// **Lights only.** Full brightness out to this radius in world units, and
    /// only then falling away to nothing at `range`. `0` — the default — starts
    /// the ramp at the light itself, which is what every light did before.
    ///
    /// This is the knob a **posterized** game needs, and it is not a nicety.
    /// Quantising a smooth radial ramp to N levels draws N concentric rings; the
    /// way out is to shape the ramp so that the whole of it falls inside one
    /// band, and you cannot do that when the ramp always spans the full radius.
    /// An inner radius of `0.8 × range` puts the entire falloff in the outer
    /// fifth (`floptle/0126`).
    pub inner: f32,
    /// **Lights only.** The exponent of that ramp. `2` — the default — is the
    /// curve every light has always had; below 1 holds the brightness out and
    /// drops it late, above 2 dives away from the core.
    pub falloff: f32,
    /// **Lights only.** Whether casters stop this light. On by default, because
    /// a light that passes through walls reads as a decal rather than as light
    /// (`floptle/0125`) — and because the per-node `blocks light` control, which
    /// is what actually decides *what* casts, has always said it would.
    pub shadows: bool,
}

impl Default for Lighting2D {
    fn default() -> Self {
        // Every one of these is "what a light did before this component
        // existed". A scene that has never heard of it must be unchanged.
        Self { mode: Lit2D::default(), layers: Vec::new(), inner: 0.0, falloff: 2.0, shadows: true }
    }
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

    /// This light's shaping, in the lane the 2D accumulation reads:
    /// `[inner radius, exponent, casts-are-honoured, spare]`.
    ///
    /// Clamped here rather than in the shader so that one place decides what a
    /// nonsense value means. An inner radius at or past the range would divide
    /// by zero and light the whole disc flat; an exponent of zero would do the
    /// same. Both are things a slider can reach and a script can type.
    pub fn falloff_lane(&self, range: f32) -> [f32; 4] {
        let r = range.max(1e-4);
        [
            self.inner.clamp(0.0, r * 0.999),
            self.falloff.max(0.01),
            if self.shadows { 1.0 } else { 0.0 },
            0.0,
        ]
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
    /// Surface friction, as a **Coulomb coefficient**: a ramp holds while
    /// `tan(angle) ≤ friction`, so 0 is ice, 1 holds exactly 45°, and a grippier
    /// surface than that goes above 1 (rubber on rubber is about 1.5).
    pub friction: f32,
    /// The steepest surface, in **degrees** from "up", this body can stand on.
    ///
    /// Past it the body is not `grounded`, the surface reads as
    /// `node.wallNormal` rather than `node.groundNormal`, and — the part that
    /// makes it a design knob rather than a label — the surface stops holding
    /// the body up, so it slides off however high the friction is. 60° is the
    /// default, and is what this was fixed at before it was a field.
    pub slope_limit: f32,
    /// Whether the scene's gravity field pulls on this body (false = floats; it still
    /// collides and can be driven by a script).
    pub gravity: bool,
    /// Freeze world-axis translation (x, y, z) — e.g. lock Z for a 2.5D game.
    pub lock_pos: [bool; 3],
    /// Freeze the entity's rotation about each axis (keeps a body upright during play).
    pub lock_rot: [bool; 3],
    /// **2D**: keep this body in the XY plane. One switch instead of four.
    ///
    /// It freezes Z translation and rotation about X and Y, which is exactly and
    /// only what "this is a 2D object" means to a solver — the body keeps its
    /// authored depth, can never drift out of the layer, and can still spin the
    /// one way a 2D object spins. Gravity, collision and every query are
    /// unchanged; a 2D body collides with the same world a 3D one does, which is
    /// what makes a tilemap's colliders work for it without a second physics
    /// engine.
    ///
    /// It composes with [`Self::lock_pos`] / [`Self::lock_rot`] rather than
    /// replacing them — see [`Self::locks_pos`]. Ticking it can only ever ADD a
    /// freeze, so a body that was already locking something keeps doing it, and
    /// unticking it cannot silently release an axis the author locked by hand.
    ///
    /// This is deliberately not a `BodyMode`: the modes are about whether the
    /// solver simulates a body at all, and a 2D body is fully simulated. Making
    /// it a mode would have meant no 2D kinematic platforms and no 2D static
    /// props, which is most of a platformer.
    pub two_d: bool,
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
            slope_limit: 60.0,
            gravity: true,
            lock_pos: [false; 3],
            lock_rot: [false; 3],
            two_d: false,
            align_up: false,
            mass: 1.0,
            assembly: false,
            pushbox_only: false,
        }
    }
}

impl RigidBody {
    /// The translation freezes the solver actually runs with.
    ///
    /// A derived answer rather than a field the editor writes, because the two
    /// inputs — the hand-set axes and the 2D switch — are both authored state
    /// and either one can change without the other. Baking their union into
    /// `lock_pos` on the way in would mean unticking 2D released a Z lock the
    /// author had set themselves, and the author would have no way to tell which
    /// of the two put it there.
    pub fn locks_pos(&self) -> [bool; 3] {
        let z = self.lock_pos[2] || self.two_d;
        [self.lock_pos[0], self.lock_pos[1], z]
    }

    /// …and the rotation freezes. 2D adds X and Y, leaving the one spin a flat
    /// object has.
    pub fn locks_rot(&self) -> [bool; 3] {
        [
            self.lock_rot[0] || self.two_d,
            self.lock_rot[1] || self.two_d,
            self.lock_rot[2],
        ]
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
    /// CONTACT shadows: a short screen-space trace that catches what the marched
    /// field cannot. A dynamic mesh casts through a collider PROXY — a box or a
    /// capsule — so a character's shadow is a capsule's, and the place that reads
    /// worst is the contact between a foot and the floor. This shadows from the
    /// real silhouette of whatever is on screen instead.
    pub contact_shadows: bool,
    /// How far the contact trace reaches, in world units. Short is the point:
    /// this is the shadow under a foot, in a seam, behind a bolt.
    pub contact_length: f32,
    /// Samples along that trace.
    pub contact_steps: u32,
    /// How dark a contact shadow gets (0..1), before the shared shadow tint and
    /// strength are applied on top.
    pub contact_strength: f32,

    /// SCREEN-SPACE REFLECTIONS: reflective surfaces show the scene itself, and
    /// not only the captured sky.
    ///
    /// Every physical material with some reflectivity picks this up at once —
    /// there is no per-material switch, because "does a mirror show the room"
    /// is a fact about the renderer rather than about one surface. What each
    /// material still decides is how much it reflects
    /// ([`Material::reflectivity`](crate::Material::reflectivity)) and how
    /// sharply ([`Material::roughness`](crate::Material::roughness)).
    ///
    /// Off by default, and deliberately: it costs a march per reflective pixel
    /// and a copy of the frame, and a scene that never wanted mirrors should not
    /// pay for them. What it CANNOT do is show anything the camera cannot —
    /// whatever is off-screen, behind the viewer or hidden behind something
    /// nearer falls back to the sky, which is why this is a complement to the
    /// environment map and not a replacement for it.
    pub reflections: bool,
    /// How far a reflected ray travels, in world units, before giving up. This
    /// is the reflection's reach: a puddle showing a building across the street
    /// needs more of it than a polished floor showing the table on it. Costs
    /// nothing extra to raise — the step count is fixed — but a long reach
    /// spreads the same samples thinner, so raise the steps with it.
    pub reflection_distance: f32,
    /// Samples along that ray (quality against cost).
    pub reflection_steps: u32,
    /// How thick the surfaces in the depth buffer are assumed to be, in world
    /// units. The depth buffer records where each surface IS and says nothing
    /// about how solid it is, so this is the window in which a ray that has gone
    /// behind a surface counts as having HIT it rather than having passed by
    /// somewhere behind it. Too small and reflections come out speckled with
    /// holes; too large and thin objects — railings, leaves, grates — smear
    /// their colour over whatever is truly behind them.
    pub reflection_thickness: f32,

    /// How many depth layers of **glass** a frame draws, 1..=4.
    ///
    /// A see-through surface refracts by sampling the picture of everything
    /// behind it, and that picture has to be taken before the surface is drawn.
    /// One picture therefore gives one correct layer — the nearest — and the
    /// pane behind it shows nothing of what is behind *it*. Raising this takes
    /// the picture again between groups of glass, working from the back
    /// forwards, so a fish tank can be six panes and a window can have a bottle
    /// standing behind it.
    ///
    /// Each extra layer costs one more capture of the scene and one more pass,
    /// and only while something see-through is actually in view. 1 is the
    /// cheapest and matches how the engine behaved before this existed.
    pub refraction_layers: u32,

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
    /// VOLUMETRIC only — how much of the scene's own light scatters in the media.
    /// 0 is the flat fog colour (the pre-injection look, reached exactly); 1 lights
    /// the fog by the sun, the point lights and the baked bounce; past 1
    /// exaggerates rather than blending further.
    pub fog_light: f32,
    /// Scattering anisotropy (-0.9..0.9). Positive scatters FORWARD, so the media
    /// blooms toward the light and shafts read; 0 is an even glow. A mote of fog
    /// has no normal — this is the knob that does the job `N·L` does elsewhere.
    pub fog_anisotropy: f32,
    /// Steps the per-pixel fog march takes (quality against cost).
    pub fog_steps: u32,
    /// March the sun shadow at every fog step. This is what turns lit fog into
    /// actual beams, and it is essentially the entire cost of lit fog.
    pub fog_shafts: bool,
}

impl Light {
    /// The ceiling on [`refraction_layers`](Self::refraction_layers).
    ///
    /// Lives here rather than in the renderer because the scene format clamps
    /// against it while loading, and a second copy of the number over there is
    /// how a `.ron` written by one version stops meaning the same thing in the
    /// next. Past four, a scene wants a renderer that sorts glass per fragment,
    /// which is a different technique rather than a bigger number.
    pub const MAX_REFRACTION_LAYERS: u32 = 4;
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
            contact_shadows: false,
            contact_length: 0.35,
            contact_steps: 12,
            contact_strength: 0.9,
            reflections: false,
            reflection_distance: 30.0,
            reflection_steps: 32,
            reflection_thickness: 0.5,
            // Two, not one: a fish tank, a window with something behind it and a
            // pair of doors are the ordinary cases, and all three are wrong at
            // one. The second layer costs nothing in a scene with no glass in it
            // and one extra capture in a scene that has some.
            refraction_layers: 2,
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
            fog_light: 1.0,
            fog_anisotropy: 0.6,
            fog_steps: 16,
            fog_shafts: true,
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

/// The **shape a light emits from**, which is the difference between a highlight
/// that is a pinprick and one that is a window.
///
/// `Point` is the zero-size case and the default, so a light that never touches
/// this shades exactly as it always did. Everything else is oriented by the
/// node's own rotation: a `Rect` and a `Disk` face the node's **forward** (its
/// local -Z, the same direction a camera looks), and a `Tube` lies along the
/// node's local X.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum LightShape {
    /// A dimensionless point — the light every engine starts with.
    #[default]
    Point,
    /// A glowing sphere: a bulb with actual size. The cheapest step away from a
    /// point, and the one that softens a highlight into a disc and a shadow
    /// terminator into a gradient.
    Sphere { radius: f32 },
    /// A rectangle in the node's local XY plane — a window, a softbox, a screen.
    /// Emits out of its front face unless `two_sided`.
    Rect { width: f32, height: f32, two_sided: bool },
    /// A disc in the node's local XY plane — a downlight, a porthole.
    Disk { radius: f32, two_sided: bool },
    /// A capsule along the node's local X: a strip light, a neon bar, a sabre.
    /// Emits in every direction, and streaks its highlight along its length.
    Tube { length: f32, radius: f32 },
}

impl LightShape {
    /// The largest half-dimension of the emitter, in world units — how far the
    /// light's own surface reaches from the node. Zero for a point.
    ///
    /// This is what the renderer widens a specular lobe and softens a terminator
    /// by, so a shape with no size falls back to the point-light response
    /// numerically and not just conceptually.
    pub fn extent(&self) -> f32 {
        match self {
            LightShape::Point => 0.0,
            LightShape::Sphere { radius } | LightShape::Disk { radius, .. } => radius.max(0.0),
            LightShape::Rect { width, height, .. } => (width.max(*height) * 0.5).max(0.0),
            LightShape::Tube { length, radius } => (length * 0.5).max(*radius).max(0.0),
        }
    }

    /// A short name for menus and the Inspector.
    pub fn label(&self) -> &'static str {
        match self {
            LightShape::Point => "point",
            LightShape::Sphere { .. } => "sphere",
            LightShape::Rect { .. } => "rect",
            LightShape::Disk { .. } => "disk",
            LightShape::Tube { .. } => "tube",
        }
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
    PointLight {
        color: [f32; 3],
        intensity: f32,
        range: f32,
        shape: LightShape,
        /// Cast local shadows: this lamp stops at the walls between it and what
        /// it is lighting, instead of shining through them.
        ///
        /// Per-lamp and off by default, because it costs a march per lit pixel
        /// per casting light and most lamps in a level do not need it — a strip
        /// under a counter, a glow inside a sign, a fill light placed exactly so
        /// it has nothing to be blocked by. The ones that do are the ones a
        /// player can walk around: a torch on a wall, a lamp in a doorway.
        ///
        /// It shadows from what is ON SCREEN. An occluder the camera cannot see
        /// cannot cast, so a wall shadows correctly while it is in frame and
        /// stops when you look away from it. The scene-wide quality and darkness
        /// are on the Lighting node, not here.
        shadows: bool,
    },
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
        /// Quantize **brightness** and carry the colour along, instead of
        /// quantizing each channel on its own (`floptle/0126`).
        ///
        /// Per channel is a real look and stays the default, but it is not what
        /// anybody expects from a *light*: a smooth radial ramp crosses each
        /// channel's band boundary at a different radius, so a warm white lamp
        /// draws concentric rings in colours nobody chose — olive where red and
        /// green have stepped and blue has not, maroon where only red has.
        /// Measured on a real game, a light at `{1.0, 0.86, 0.62}` produced
        /// **no** clean brightness step anywhere in its radius, only hue rings.
        ///
        /// Preserving chroma cannot do that, because chroma is never quantized.
        /// An exactly grey pixel takes the identical path it always did.
        posterize_chroma: bool,

        // ---- the look chain -------------------------------------------------
        //
        // Everything below is OFF at its default, and each is skipped by the
        // renderer when it is: a scene that touches none of it renders exactly
        // the frames it rendered before. See `floptle_render::PostSettings` for
        // the pass order and why it is that order.

        /// How the scene's linear light lands on a display that stops at white.
        ///
        /// `0` clip (the default, and what the engine did before there was a
        /// choice), `1` Reinhard, `2` ACES, `3` AgX. A plain number rather than
        /// an enum here because `Matter` carries no renderer types; the renderer
        /// reads it through `floptle_render::Tonemap`.
        tonemap: u32,
        /// Colour grade — exposure in STOPS (0 = unchanged, +1 = twice the light).
        exposure: f32,
        /// Contrast about 18% grey. 1 = unchanged.
        contrast: f32,
        /// Saturation against Rec.709 luma. 1 = unchanged, 0 = greyscale.
        saturation: f32,
        /// White balance, blue ↔ amber. 0 = unchanged.
        temperature: f32,
        /// White balance, green ↔ magenta — the axis `temperature` cannot reach,
        /// and the one that fixes a scene that has gone subtly sickly.
        tint: f32,
        /// Lift the black floor. 0 = unchanged.
        lift: f32,
        /// Midtone gamma. 1 = unchanged.
        grade_gamma: f32,
        /// Scale the highlights. 1 = unchanged.
        gain: f32,

        /// Chromatic aberration: how far the red and blue channels drift apart
        /// toward the edges of the frame. 0 = off.
        aberration: f32,
        /// Lens distortion: positive barrels (fisheye), negative pincushions.
        /// 0 = off.
        distortion: f32,

        /// Unsharp mask amount. 0 = off.
        sharpen: f32,
        /// Bilateral denoise, 0..1 — averages within flat regions and refuses to
        /// average across an edge. 0 = off.
        denoise: f32,

        /// Film grain amount. 0 = off.
        grain: f32,
        /// Grain cell size in pixels. 1 is per-pixel; 2+ clumps it, which is
        /// what it needs to be visible at all under a retro upscale.
        grain_size: f32,

        /// Depth of field: the distance from the camera, in world units, that is
        /// in focus. 0 = off.
        dof_focus: f32,
        /// How far BEYOND `dof_focus` stays sharp, in world units.
        dof_range: f32,
        /// How far IN FRONT of `dof_focus` stays sharp. 0 = half of `dof_range`,
        /// which is what the effect always did and what a lens roughly does —
        /// the near side goes soft much sooner than the far side.
        ///
        /// Split from `dof_range` because they are the two halves people
        /// actually reach for: a portrait wants a near side that falls away
        /// immediately and a far side that keeps some shape, and one number
        /// cannot say that.
        dof_near_range: f32,
        /// The widest the out-of-focus blur gets, in pixels.
        dof_max_blur: f32,
        /// Aperture blades: 0 (or 1, 2) is a round iris, 3+ gives the polygonal
        /// bokeh of a real lens — six is the classic hexagon.
        dof_blades: u32,
        /// Turn the blade polygon, in degrees. Only means anything with blades.
        dof_blade_rotation: f32,
        /// How much brighter-than-white pixels dominate the blur, so a highlight
        /// spreads into a visible disc instead of averaging into grey mush. 0 =
        /// off. It reads the scene's real light, which is why it only became
        /// possible once the frame stopped being 8-bit.
        dof_highlight: f32,
        /// Taps in the blur kernel. 0 = the default 16. More is smoother bokeh
        /// and linearly more expensive; fewer is the chunky look on purpose.
        dof_quality: u32,
        /// MOTION BLUR: the shutter, as a fraction of the step between frames.
        /// 0 = off. 0.5 is the 180° shutter a film camera has, and is the value
        /// that reads as footage rather than as a smear; 1 leaves the shutter
        /// open for the whole frame.
        ///
        /// Reconstructed from depth, so it blurs **camera** motion — a pan, a
        /// whip, a dolly, a roll. An object crossing a locked-off shot stays
        /// sharp; that half needs a velocity buffer written by every draw path
        /// in the engine, and is not what this is.
        motion_blur: f32,
        /// Taps along the streak. 0 = the default 12; clamped to 4..32.
        motion_samples: u32,
        /// Tint the frame by what is in focus — cool where the near side is
        /// blurring, warm where the far side is, plain where it is sharp. A
        /// tuning aid: the focus band is otherwise something you infer from a
        /// picture, and inferring it is how an hour goes.
        dof_show_focus: bool,
        /// Focus on a NODE by name instead of at a fixed distance: the focus
        /// distance becomes the camera's distance to it, every frame. Empty =
        /// use `dof_focus`.
        ///
        /// This is the setting a rack focus is made of, and doing it by hand
        /// means writing a script to measure a distance the engine already
        /// knows. A name that resolves to nothing falls back to `dof_focus`
        /// rather than to zero, so a renamed node softens nothing.
        dof_focus_node: String,

        /// The scene's **screen shaders**: authored `stage post` `.flsl` passes,
        /// run over the finished frame in this order.
        ///
        /// A list rather than a fixed menu of effects, because the point is that
        /// the look is the project's to write: an ink outline, a CRT, a heat
        /// haze and a colour ramp are four passes, not four engine features. See
        /// `floptle_shader::ir::Stage::Post`.
        ///
        /// Empty on every scene that has never used one, so it costs nothing and
        /// writes nothing to the `.ron`.
        screen_shaders: Vec<ScreenShader>,
    },
    /// A **light probe volume**: the box that baked global illumination is
    /// gathered over, and the box it lights inside.
    ///
    /// Direct light tells a surface about the sun. Everything else a real room
    /// looks like is light that already bounced — off a red wall, off a bright
    /// floor — and no amount of material work invents it. The engine's answer
    /// before this node was a single flat ambient colour, which lifts the inside
    /// of a sealed box exactly as much as it lifts an open field.
    ///
    /// Baking renders the scene from a lattice of points inside the box and
    /// keeps, at each one, the light arriving from every direction. Inside the
    /// box that replaces the flat ambient; outside, the flat ambient carries on
    /// as before. So a scene with no volume renders exactly what it always did,
    /// and a scene with one is lit by its own surfaces.
    ///
    /// The node's transform positions and *scales* the box: `half_extents` is
    /// its size at scale 1, and moving the node moves the volume. The bake
    /// itself lives in a `.fgi` file beside the scene, because it is a build
    /// artefact measured in hundreds of kilobytes and a `.ron` is a thing people
    /// read.
    LightProbes {
        /// Half the box, in local units, before the node's scale.
        half_extents: [f32; 3],
        /// Requested distance between probes, in world units. The real spacing
        /// is whatever divides the box evenly; a spacing that would ask for more
        /// probes than the engine bakes is coarsened, not truncated.
        spacing: f32,
        /// Master switch. Off keeps the volume, its settings and its bake, and
        /// stops it lighting anything — the same shape as a screen shader's own
        /// switch, and the fastest way to see what the GI is actually doing.
        enabled: bool,
        /// How much of the baked light to apply. 1 = as measured. This is the
        /// artistic knob: physically-correct bounce is often a little too polite
        /// for a game, and dialling it past 1 is a legitimate look rather than a
        /// mistake.
        intensity: f32,
        /// How many times light is allowed to bounce. 1 is direct light coming
        /// off surfaces once — the difference between a black corner and a lit
        /// one. 2 and 3 fill in soft interiors and cost a full re-render of
        /// every probe each.
        bounces: u32,
        /// Cube face resolution for the bake, in pixels per side. Higher resolves
        /// smaller bright things (a lamp, a window) at a quadratic cost in bake
        /// time, and does **not** change overall brightness.
        quality: u32,
        /// How hard to reject probes that are buried inside geometry, in
        /// multiples of the probe spacing. 0 = off.
        ///
        /// This is the knob for the one artefact everybody recognises: light
        /// from the lit room next door glowing faintly through the wall. Turning
        /// it up costs contact bounce in tight spaces, which is why it is a knob
        /// and not a constant.
        leak: f32,
        /// How far along its own normal a surface steps before looking the light
        /// up, in multiples of the probe spacing. A shading point sits exactly
        /// on the geometry, which is the one place where "which side of this
        /// wall am I on" is genuinely ambiguous.
        normal_bias: f32,
        /// Layers excluded from the bake, by name. Anything that moves — a
        /// character, a door, a lift — should not be baked into the light it
        /// stands in, because it will still be lit by that light after it walks
        /// away.
        exclude_layers: Vec<String>,
    },
    /// A **reflection probe**: what the surfaces in one room reflect.
    ///
    /// A reflective surface asks two things in turn — "is what I am reflecting
    /// on screen?", and if not, "then what is out there?". Outdoors the second
    /// answer is the sky and that is genuinely right. Indoors it is daylight
    /// arriving through a sealed ceiling, which is the most conspicuous way an
    /// interior can fail to look like one.
    ///
    /// This node is the other answer. It captures the view from its own
    /// position, once, and every reflective surface inside its box uses that
    /// instead of the sky. A polished floor shows the room it is in.
    ///
    /// **The box is the room, and it does two jobs.** It says which surfaces
    /// this probe speaks for, and it is what makes a reflected wall land *on*
    /// the wall: an environment map on its own is a picture at infinity and
    /// slides as the camera moves. Sized to the room, reflections sit still.
    ///
    /// Nothing is written to disk. A capture is fast enough to take on load and
    /// whenever the probe is moved, which is better than a bake that can go
    /// stale without saying so — and it means a `.ron` stays a thing people read.
    ReflectionProbe {
        /// Half the box, in local units, before the node's scale.
        half_extents: [f32; 3],
        /// Master switch. Off keeps the node and its box and stops it reflecting
        /// anything, which is the fastest way to see what it is actually doing.
        enabled: bool,
        /// How much of the capture to apply. 1 is as measured; this is the
        /// artistic knob, for when a room reads too busy or too dim in the
        /// reflections and the answer is not to relight the room.
        intensity: f32,
        /// How far outside the box the probe fades out before the sky takes
        /// over, in world units. A doorway wants a metre or two of it so a
        /// surface walking out of the room crosses over smoothly instead of
        /// switching environments in one step.
        fade: f32,
    },
}

/// One authored full-screen pass on a [`Matter::PostProcess`] node.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenShader {
    /// Project-relative path to a `stage post` `.flsl`.
    pub shader: String,
    /// Switched off keeps the pass in the list, and its knobs, without running
    /// it — which is what you want while deciding whether an effect helps, and
    /// is not the same thing as deleting it.
    pub enabled: bool,
    /// Overrides for the shader's exposed uniforms, by name. A name the shader
    /// no longer declares is ignored, not an error: renaming a uniform must not
    /// fail to load a scene.
    pub params: std::collections::BTreeMap<String, [f32; 4]>,
}

impl ScreenShader {
    /// A freshly added pass: on, and using the shader's own defaults.
    pub fn new(shader: impl Into<String>) -> Self {
        Self { shader: shader.into(), enabled: true, params: Default::default() }
    }
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

    /// A fresh light probe volume: a room-sized box, one probe per metre, one
    /// bounce.
    ///
    /// One bounce and a 16-pixel face because the first thing anyone does with a
    /// new volume is bake it, and a bake that takes four minutes on the first
    /// try teaches the wrong lesson about the feature. Both knobs go up.
    pub fn default_light_probes() -> Self {
        Matter::LightProbes {
            half_extents: [8.0, 4.0, 8.0],
            spacing: 2.0,
            enabled: true,
            intensity: 1.0,
            bounces: 1,
            quality: 16,
            leak: 1.0,
            normal_bias: 0.5,
            exclude_layers: Vec::new(),
        }
    }

    /// A fresh reflection probe: a room-sized box, capturing at full strength.
    ///
    /// The same box a fresh light-probe volume gets, because they are sized for
    /// the same thing — the room you just placed one in.
    pub fn default_reflection_probe() -> Self {
        Matter::ReflectionProbe {
            half_extents: [8.0, 4.0, 8.0],
            enabled: true,
            intensity: 1.0,
            fade: 2.0,
        }
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
            // Today's look. Every project's posterize is built on per-channel
            // stepping, so this is opt-in or it is a silent change of art.
            posterize_chroma: false,
            tonemap: 0,
            // The look chain, at identity. Note which of these are 1.0: a
            // derived default would give a black, contrastless, greyscale
            // picture and read as the feature being broken.
            exposure: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            temperature: 0.0,
            tint: 0.0,
            lift: 0.0,
            grade_gamma: 1.0,
            gain: 1.0,
            aberration: 0.0,
            distortion: 0.0,
            sharpen: 0.0,
            denoise: 0.0,
            grain: 0.0,
            grain_size: 1.0,
            dof_focus: 0.0,
            dof_range: 5.0,
            dof_near_range: 0.0,
            dof_max_blur: 0.0,
            dof_blades: 0,
            dof_blade_rotation: 0.0,
            dof_highlight: 0.0,
            dof_quality: 0,
            motion_blur: 0.0,
            motion_samples: 0,
            dof_show_focus: false,
            dof_focus_node: String::new(),
            screen_shaders: Vec::new(),
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

/// The camera that holds play authority: `Matter::Camera { active: true }`, and
/// **not switched off**.
///
/// One answer, because there were ten. "Find the active camera" was written out
/// by hand at every site that needed one — the Game viewport, the audio
/// listener, the floating-origin focus, the terrain LOD eye, the input snapshot's
/// aim, the 2D-vs-3D inference — and only two of them remembered [`is_disabled`].
/// So switching a camera off in the Hierarchy hid it, stopped its scripts, took
/// it out of physics… and left it rendering the game, which reads as the disable
/// not working rather than as a missing filter in one of ten copies of the same
/// query.
///
/// Scene order breaks ties, the same as every other "first one wins" index, so a
/// scene with two active cameras behaves the same way it did before.
pub fn active_camera(world: &crate::ecs::World) -> Option<crate::ecs::Entity> {
    world.query::<Matter>().find_map(|(e, m)| {
        (matches!(m, Matter::Camera { active: true, .. }) && !is_disabled(world, e)).then_some(e)
    })
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
    /// The shaping lane, and the two values in it that a slider or a script can
    /// reach and that would break the shader if they arrived unclamped: an inner
    /// radius at or past the range divides by zero and lights the whole disc
    /// flat, and an exponent of zero does the same.
    #[test]
    fn a_lights_falloff_clamps_where_the_shader_would_divide_by_zero() {
        let d = Lighting2D::default();
        assert_eq!(d.falloff_lane(8.0), [0.0, 2.0, 1.0, 0.0], "the curve every light always had");

        let past = Lighting2D { inner: 99.0, ..Default::default() };
        assert!(past.falloff_lane(8.0)[0] < 8.0, "an inner radius past the range flattens the light");

        let zero = Lighting2D { falloff: 0.0, ..Default::default() };
        assert!(zero.falloff_lane(8.0)[1] > 0.0);

        // Negative, from a script that subtracted too far.
        let neg = Lighting2D { inner: -4.0, falloff: -1.0, ..Default::default() };
        let lane = neg.falloff_lane(8.0);
        assert_eq!(lane[0], 0.0);
        assert!(lane[1] > 0.0);

        // …and the shadow flag is the third lane, not a fourth field somewhere.
        let off = Lighting2D { shadows: false, ..Default::default() };
        assert_eq!(off.falloff_lane(8.0)[2], 0.0);
    }

    /// layer — a node that never picked one and a node that picked "Default"
    /// are the same node, so a light must not tell them apart.
    #[test]
    fn naming_layers_restricts_a_light_to_them() {
        let torch = Lighting2D {
            layers: vec!["Terrain".into(), DEFAULT_SORTING_LAYER.into()],
            ..Default::default()
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
