//! The editor's catalog of node "types" (`Matter` kinds): default constructors
//! for the spawn menus, human labels + Inspector glyphs, and the Add Component
//! type-switch list.

use floptle_core::{Matter, Shape};
use floptle_render::MeshData;
use floptle_scene::{MatterDoc, ShapeDoc};

/// The CPU geometry behind each built-in primitive — the ONE definition.
///
/// The renderer registers these at startup (`Editor::init`, mapping `Shape as usize`
/// → `MeshId`) and the vertex-paint brush caches them for raycasting. Both MUST get
/// byte-identical geometry: paint is indexed by `vertex_index`, so if these two ever
/// disagreed on vertex count or order, the brush would paint the wrong vertices.
/// Hence one function, called twice — never two copies of the parameters.
pub(crate) fn primitive_mesh(shape: Shape) -> MeshData {
    match shape {
        Shape::Cube => floptle_render::cube(PRIMITIVE_HALF),
        Shape::Sphere => floptle_render::uv_sphere(0.85, 24, 36),
        Shape::Capsule => floptle_render::capsule(0.5, 0.5, 16, 24),
        Shape::Plane => floptle_render::plane(PRIMITIVE_HALF),
    }
}

/// Half the edge of the built-in box and quad — so a Cube or Plane at scale 1
/// is **1.4 units across**, not 1.
///
/// A historical choice, and harmless for a primitive you size by eye in the
/// Inspector. It stops being harmless the moment something quotes a size in
/// world units and builds it out of one of these: a sprite batch's `size` is
/// documented as the edge length, so it has to divide this back out
/// (`floptle/0070`, where every bullet in a game came out 40% too big and read
/// as a tuning change). Anything else measuring against these meshes should
/// name this constant rather than paste the number.
pub(crate) const PRIMITIVE_HALF: f32 = 0.7;

pub(crate) fn new_cube() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.8, 0.5, 0.4] }
}
pub(crate) fn new_plane() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Plane, color: [0.8, 0.8, 0.8] }
}

/// The default node name for a matter kind.
pub(crate) fn matter_doc_name(m: &MatterDoc) -> &'static str {
    match m {
        MatterDoc::Primitive { shape: ShapeDoc::Cube, .. } => "Cube",
        MatterDoc::Primitive { shape: ShapeDoc::Sphere, .. } => "Sphere",
        MatterDoc::Primitive { shape: ShapeDoc::Capsule, .. } => "Capsule",
        MatterDoc::Primitive { shape: ShapeDoc::Plane, .. } => "Plane",
        MatterDoc::Blob { .. } => "Blob",
        MatterDoc::Mesh { .. } => "Mesh",
        MatterDoc::Empty => "Empty",
        MatterDoc::MapMesh { .. } => "Model Mesh",
        MatterDoc::Terrain { .. } => "Terrain",
        MatterDoc::Camera { .. } => "Camera",
        MatterDoc::PointLight { .. } => "Point Light",
        MatterDoc::GravityVolume { .. } => "Gravity Volume",
        MatterDoc::NavMesh { .. } => "Nav Mesh",
        MatterDoc::NavLink { .. } => "Nav Link",
        MatterDoc::NavArea { .. } => "Nav Area",
        MatterDoc::WaterVolume { .. } => "Water Volume",
        MatterDoc::LightProbes { .. } => "Light Probes",
        MatterDoc::ReflectionProbe { .. } => "Reflection Probe",
        MatterDoc::FieldShape { .. } => "Field Shape",
        MatterDoc::Tilemap { .. } => "Tilemap",
        MatterDoc::SpriteBatch { .. } => "Sprite Batch",
        MatterDoc::Sprite { .. } => "Sprite",
        MatterDoc::Skybox { .. } => "Skybox",
        MatterDoc::PostProcess { .. } => "Post Processing",
    }
}
pub(crate) fn new_sphere() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.6, 0.9] }
}
pub(crate) fn new_capsule() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Capsule, color: [0.5, 0.85, 0.6] }
}

/// A short human label for a node's runtime `Matter` "type".
pub(crate) fn matter_kind_label(m: &Matter) -> &'static str {
    match m {
        Matter::Primitive { shape: Shape::Cube, .. } => "Cube",
        Matter::Primitive { shape: Shape::Sphere, .. } => "Sphere",
        Matter::Primitive { shape: Shape::Capsule, .. } => "Capsule",
        Matter::Primitive { shape: Shape::Plane, .. } => "Plane",
        Matter::Blob { .. } => "Blob",
        Matter::Mesh { .. } => "Mesh",
        Matter::Empty => "Empty",
        Matter::MapMesh { .. } => "Model Mesh",
        Matter::Terrain { .. } => "Terrain",
        Matter::Camera { .. } => "Camera",
        Matter::PointLight { .. } => "Point Light",
        Matter::GravityVolume { .. } => "Gravity Volume",
        Matter::NavMesh { .. } => "Nav Mesh",
        Matter::NavLink { .. } => "Nav Link",
        Matter::NavArea { .. } => "Nav Area",
        Matter::WaterVolume { .. } => "Water Volume",
        Matter::FieldShape { .. } => "Field Shape",
        Matter::LightProbes { .. } => "Light Probes",
        Matter::ReflectionProbe { .. } => "Reflection Probe",
        Matter::Tilemap { .. } => "Tilemap",
        Matter::SpriteBatch { .. } => "Sprite Batch",
        Matter::Sprite { .. } => "Sprite",
        Matter::Skybox { .. } => "Skybox",
        Matter::PostProcess { .. } => "Post Processing",
    }
}

/// The little glyph shown beside a node's type in the Inspector header.
pub(crate) fn matter_icon(m: &Matter) -> &'static str {
    match m {
        Matter::Primitive { shape: Shape::Cube, .. } => "■",
        Matter::Primitive { shape: Shape::Sphere, .. } => "○",
        Matter::Primitive { shape: Shape::Capsule, .. } => "▪",
        Matter::Primitive { shape: Shape::Plane, .. } => "▭",
        Matter::Blob { .. } => "◑",
        Matter::Mesh { .. } => "✳",
        Matter::Empty => "🗀",
        Matter::MapMesh { .. } => "▦",
        Matter::Terrain { .. } => "Δ",
        Matter::Camera { .. } => "⌖",
        Matter::PointLight { .. } => "●",
        Matter::GravityVolume { .. } => "⬇",
        Matter::NavMesh { .. } => "⬚",
        Matter::NavLink { .. } => "⇄",
        Matter::NavArea { .. } => "▨",
        Matter::WaterVolume { .. } => "≈",
        Matter::FieldShape { .. } => "◈",
        Matter::LightProbes { .. } => "☀",
        Matter::ReflectionProbe { .. } => "◍",
        Matter::Tilemap { .. } => "▩",
        Matter::SpriteBatch { .. } => "▧",
        Matter::Sprite { .. } => "▫",
        Matter::Skybox { .. } => "◎",
        Matter::PostProcess { .. } => "✨",
    }
}

/// The set of node "types" the Inspector's Add Component menu can switch a node to
/// (icon-labeled). Mutually exclusive — picking one replaces the node's current
/// `Matter`.
///
/// What is left out, and why, is `NOT_IN_THE_TYPE_MENU` beside the test that
/// holds this list against `Matter`'s own variants. It is a test rather than a
/// comment because this list had silently fallen four types behind: a node could
/// be made into a Sprite, a Tilemap or a Sprite Batch from ✚ New and never
/// switched into one afterwards, so the flat half of the engine was reachable
/// only by creating a fresh node and moving the work across.
pub(crate) fn type_catalog() -> Vec<(&'static str, Matter)> {
    use floptle_core::GravityMode;
    vec![
        ("■  Cube", Matter::Primitive { shape: Shape::Cube, color: [0.8, 0.5, 0.4] }),
        ("○  Sphere", Matter::Primitive { shape: Shape::Sphere, color: [0.4, 0.6, 0.9] }),
        ("▪  Capsule", Matter::Primitive { shape: Shape::Capsule, color: [0.5, 0.85, 0.6] }),
        ("▭  Plane", Matter::Primitive { shape: Shape::Plane, color: [0.8, 0.8, 0.8] }),
        ("◑  Blob", Matter::Blob { scale: 1.0 }),
        ("🗀  Empty", Matter::Empty),
        ("⌖  Camera", Matter::Camera {
            fov_y: 60f32.to_radians(),
            active: false,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: Matter::TARGET_W,
            target_h: Matter::TARGET_H,
            target_hz: 0.0,
            ortho: false,
            ortho_height: Matter::ORTHO_HEIGHT,
        }),
        ("●  Point Light", Matter::PointLight {
            color: [1.0, 0.95, 0.85],
            intensity: 1.0,
            range: 10.0,
            shape: floptle_core::LightShape::Point,
            shadows: false,
            spot_angle: floptle_core::OMNI_ANGLE,
            spot_softness: 0.25,
        }),
        // The same component, aimed. Two entries rather than two node types
        // because a spot IS a point light with a cone — it takes the same
        // emitter shapes, the same range, the same local shadows and the same
        // slot in the sixteen — and somebody looking for "spot light" in a menu
        // should not have to know that.
        //
        // Shadows ON by default here, unlike the point light. A spot is aimed at
        // something, which means somebody placed it to light one thing and not
        // its neighbours, and a spot that shines through the wall it is pointed
        // at is the first thing they would file.
        ("◤  Spot Light", Matter::PointLight {
            color: [1.0, 0.95, 0.85],
            intensity: 4.0,
            range: 14.0,
            shape: floptle_core::LightShape::Point,
            shadows: true,
            spot_angle: 45.0,
            spot_softness: 0.25,
        }),
        ("⬇  Gravity Volume", Matter::GravityVolume { mode: GravityMode::Down, strength: 9.81, radius: 20.0 }),
        ("≈  Water Volume", Matter::default_water()),
        ("◈  Field Shape", Matter::FieldShape { radius: 1.5 }),
        ("⬚  Nav Mesh", Matter::default_nav_mesh(1)),
        ("☀  Light Probes", Matter::default_light_probes()),
        ("◍  Reflection Probe", Matter::default_reflection_probe()),
        ("◎  Skybox", Matter::default_skybox()),
        ("⇄  Nav Link", Matter::default_nav_link(1)),
        ("▨  Nav Area", Matter::default_nav_area()),
        // The flat three. Same defaults as ✚ New builds, so a node switched into
        // a Sprite is the node ✚ New would have made — 32 pixels per unit, a
        // centred pivot, cell 0.
        ("▫  Sprite", Matter::Sprite {
            ppu: 32.0,
            size: 1.0,
            cell: 0,
            flip_x: false,
            flip_y: false,
            pivot: [0.5, 0.5],
        }),
        ("▩  Tilemap", Matter::Tilemap {
            cols: 16,
            rows: 16,
            tile: 1.0,
            data: Vec::new(),
            tileset: String::new(),
        }),
        ("▧  Sprite Batch", Matter::SpriteBatch { size: 1.0 }),
    ]
}

// ---------------------------------------------------------------------------
// The ✚ New catalog
// ---------------------------------------------------------------------------

/// What picking an entry in the ✚ New menu does.
///
/// Most entries are just a `MatterDoc` to spawn; a few need the editor to do
/// something first (a terrain needs a fresh field, a camera needs the active-
/// camera bookkeeping) and so record an intent instead.
#[derive(Clone, Copy)]
pub(crate) enum NewNode {
    /// Spawn this matter.
    Matter(fn() -> MatterDoc),
    /// `cmd.open_new_terrain` — a terrain is more than a doc.
    Terrain,
    /// `cmd.add_camera` — carries the parent.
    Camera,
    /// `cmd.add_map_shape` — geometry goes to the map store, not the scene doc.
    MapShape(crate::map_edit::MapShape),
    /// `cmd.add_ui` — the UI elements build a small subtree.
    Ui(crate::ui_game::AddUi),
}

/// One line in the ✚ New menu.
pub(crate) struct NewEntry {
    pub(crate) label: &'static str,
    pub(crate) hover: &'static str,
    pub(crate) make: NewNode,
}

/// A submenu of the ✚ New menu.
pub(crate) struct NewGroup {
    pub(crate) title: &'static str,
    pub(crate) hover: &'static str,
    pub(crate) items: &'static [NewEntry],
}

/// **The catalog is data, and this is the only copy of it.**
///
/// It was a hundred and eighty lines of `if ui.button(..)` in one flat list, and
/// a flat list is what a menu becomes when nobody decides it is a menu: twenty
/// items deep, every node type the engine has ever had, with the four a 2D game
/// wants scattered between a nav link and a reflection probe. Every new node
/// type made it worse, and there was no way to say so — a list has no shape to
/// check.
///
/// So: **grouped, and the groups are the ones a project thinks in.** 2D and 3D
/// are the top-level split because that is the first thing true about a game and
/// the last thing that changes about it; the rest are the systems you reach for
/// when you are already thinking about that system.
///
/// Two rules that hold it together, both testable and both tested:
///
/// * **The order is fixed.** It is not sorted by what the scene looks like, not
///   promoted by what you used last, and the 2D group does not jump to the top
///   because the camera is orthographic. A menu whose contents move is a menu
///   you have to read every time; muscle memory is worth more than relevance.
/// * **Nothing is hidden.** Grouping is not filtering. Every node type the
///   engine can spawn is in here exactly once, which
///   `every_node_type_is_in_the_new_menu` asserts against `MatterDoc` itself, so
///   adding a variant without giving it a home is a build failure rather than a
///   node nobody can create.
pub(crate) const NEW_CATALOG: &[NewGroup] = &[
    NewGroup {
        title: "▦ 2D",
        hover: "flat things: sprites, tile levels, and the batch that draws a \
                thousand of them at once",
        items: &[
            NewEntry {
                label: "▩ Tilemap",
                hover: "a grid of spritesheet cells as ONE mesh — the 2D level primitive. \
                        Give it a Material with a sheet, then paint it in the ◫ Tiles tab \
                        or fill it from a script (node:setTilemap{…} / tm:set). \
                        Neighbouring tiles share an exact edge, so no seams open up as \
                        the camera moves",
                make: NewNode::Matter(new_tilemap),
            },
            NewEntry {
                label: "▫ Sprite",
                hover: "one flat quad wearing a cell of its Material's sheet. Sizes itself \
                        from the image in pixels per unit, flips without a negative node \
                        scale, and carries a PIVOT — put it at the feet and a Y-sorted \
                        character sorts by where it is standing rather than by its waist",
                make: NewNode::Matter(new_sprite),
            },
            NewEntry {
                label: "▧ Sprite Batch",
                hover: "N sprites from one node, each with its own cell AND tint, drawn \
                        per frame from a script (node:sprites() / b:draw(…)) — no scene \
                        node per bullet and no pool to grow",
                make: NewNode::Matter(new_sprite_batch),
            },
        ],
    },
    NewGroup {
        title: "■ 3D",
        hover: "solid geometry: the primitives, editable blockout shapes, and SDF matter",
        items: &[
            NewEntry {
                label: "■ Cube",
                hover: "a box primitive — the go-to building block (floors, walls, crates)",
                make: NewNode::Matter(new_cube),
            },
            NewEntry {
                label: "○ Sphere",
                hover: "a sphere primitive",
                make: NewNode::Matter(new_sphere),
            },
            NewEntry {
                label: "▪ Capsule",
                hover: "a capsule primitive (ideal for a physics character body)",
                make: NewNode::Matter(new_capsule),
            },
            NewEntry {
                label: "▭ Plane",
                hover: "a flat double-sided quad — add a Material to texture it, drop \
                        opacity below 1 for transparency",
                make: NewNode::Matter(new_plane),
            },
            NewEntry {
                label: "◑ Blob",
                hover: "an SDF metaball — nearby blobs melt together (organic/surreal shapes)",
                make: NewNode::Matter(new_blob),
            },
            NewEntry {
                label: "◈ Field Shape",
                hover: "an authored SDF shape: assign an sdf-stage .flsl on its Material \
                        and the shader IS the geometry, raymarched into the scene field \
                        (up to 4 per scene)",
                make: NewNode::Matter(new_field_shape),
            },
        ],
    },
    NewGroup {
        title: "▦ Model shape",
        hover: "editable blockout geometry — the ▦ Model tool (key 8) edits its \
                faces/edges/verts, extrudes, and assigns per-face materials",
        items: &[
            NewEntry { label: "▦ Box", hover: "a box you then push faces around on — the usual starting point for a room", make: NewNode::MapShape(crate::map_edit::MapShape::Box) },
            NewEntry { label: "▦ Plane", hover: "a single editable quad — a floor or a wall to extrude from. For a 2D sprite use ▦ 2D ▸ ▫ Sprite.", make: NewNode::MapShape(crate::map_edit::MapShape::Plane) },
            NewEntry { label: "▦ Wedge", hover: "a box with one sloped face — a ramp you can walk up", make: NewNode::MapShape(crate::map_edit::MapShape::Wedge) },
            NewEntry { label: "▦ Cylinder", hover: "an editable tube — a pillar, a pipe, a tower", make: NewNode::MapShape(crate::map_edit::MapShape::Cylinder) },
            NewEntry { label: "▦ Sphere", hover: "an editable ball — a dome once you cut it in half", make: NewNode::MapShape(crate::map_edit::MapShape::Sphere) },
            NewEntry { label: "▦ Stairs", hover: "a run of steps as real geometry, so it collides step by step", make: NewNode::MapShape(crate::map_edit::MapShape::Stairs) },
            NewEntry { label: "▦ Arch", hover: "a curved opening — a doorway, a bridge span, a window", make: NewNode::MapShape(crate::map_edit::MapShape::Arch) },
        ],
    },
    NewGroup {
        title: "● Lighting",
        hover: "lamps, the sky, and the bake that puts bounced light in a room",
        items: &[
            NewEntry {
                label: "● Point Light",
                hover: "a placeable omni light (color / intensity / range). Under an \
                        orthographic camera this is also the 2D light",
                make: NewNode::Matter(new_point_light),
            },
            NewEntry {
                label: "◤ Spot Light",
                hover: "a point light aimed down the node's forward — rotate it to aim",
                make: NewNode::Matter(new_spot_light),
            },
            NewEntry {
                label: "☀ Light Probes",
                hover: "baked bounce light: a box you place over the space, then Bake in \
                        the Inspector. Inside it, flat ambient is replaced by what the \
                        surrounding surfaces actually reflect. Saved as a .fgi beside the \
                        scene.",
                make: NewNode::Matter(new_light_probes),
            },
            NewEntry {
                label: "◍ Reflection Probe",
                hover: "captures the room around it so nearby glossy surfaces reflect \
                        THIS space rather than the sky",
                make: NewNode::Matter(new_reflection_probe),
            },
        ],
    },
    NewGroup {
        title: "Δ World",
        hover: "the ground, the sea, and where gravity points",
        items: &[
            NewEntry {
                label: "Δ Terrain",
                hover: "a sculptable SDF terrain node",
                make: NewNode::Terrain,
            },
            NewEntry {
                label: "≈ Water Volume",
                hover: "a body of water — a planet's sea, or a lake/tank",
                make: NewNode::Matter(new_water),
            },
            NewEntry {
                label: "⬇ Gravity Volume",
                hover: "physics gravity: Down (level) or Radial (planet)",
                make: NewNode::Matter(new_gravity),
            },
        ],
    },
    NewGroup {
        title: "⬚ Navigation",
        hover: "where characters can walk, what it costs them, and the ways across \
                that are not walking",
        items: &[
            NewEntry {
                label: "⬚ Nav Mesh",
                hover: "where characters can walk. Bakes what a character would collide \
                        with, filtered by layer, and works its own bounds out. Saved as a \
                        .fnav beside the scene.",
                make: NewNode::Matter(new_nav_mesh),
            },
            NewEntry {
                label: "⇄ Nav Link",
                hover: "a way across that is not walking: a ladder, a jump down, a vault, \
                        a door. This node is one end and the far end is an offset from it; \
                        bake the navmesh again to join them up.",
                make: NewNode::Matter(new_nav_link),
            },
            NewEntry {
                label: "▨ Nav Area",
                hover: "ground that means something: water, mud, a road, or nothing \
                        walkable at all. Routes cost more (or less) through it, and one \
                        character can refuse it while another wades in.",
                make: NewNode::Matter(new_nav_area),
            },
        ],
    },
    NewGroup {
        title: "🖼 UI",
        hover: "screen-space elements — a layer to hold them, then the parts",
        items: &[
            NewEntry { label: "Layer", hover: "a screen-space UI canvas — elements go inside it", make: NewNode::Ui(crate::ui_game::AddUi::Layer) },
            NewEntry { label: "Panel", hover: "a rounded-rect shape (radius 0 = sharp, high = pill)", make: NewNode::Ui(crate::ui_game::AddUi::Panel) },
            NewEntry { label: "Text", hover: "a text label (your fonts later; neutral fallback for now)", make: NewNode::Ui(crate::ui_game::AddUi::Text) },
            NewEntry { label: "Image", hover: "any texture from your assets — the engine ships no UI art", make: NewNode::Ui(crate::ui_game::AddUi::Image) },
            NewEntry { label: "Slider", hover: "a value-driven bar (health, progress…): track + Fill + Handle parts you retexture and arrange freely", make: NewNode::Ui(crate::ui_game::AddUi::Slider) },
            NewEntry { label: "Button", hover: "a clickable element — its scripts get hoverStart/pressed/clicked hooks", make: NewNode::Ui(crate::ui_game::AddUi::Button) },
            NewEntry { label: "Scroll View", hover: "a wheel-scrollable viewport — put more content inside than fits and it clips + scrolls", make: NewNode::Ui(crate::ui_game::AddUi::Scroll) },
            NewEntry { label: "Text Field", hover: "the player types into it — caret, selection, clipboard, and a `submitted` hook. Its value IS its text.", make: NewNode::Ui(crate::ui_game::AddUi::Field) },
            NewEntry { label: "Tooltip", hover: "this layer's tooltip box: the engine fills it and follows the pointer, you decide what it looks like", make: NewNode::Ui(crate::ui_game::AddUi::Tooltip) },
        ],
    },
];

/// The two entries that stay at the TOP level rather than going in a group.
///
/// A group is worth its extra click when it holds things you reach for while
/// already thinking about that system. These two are not that: a Camera is
/// wanted by every scene of either kind, and an Empty is the answer to "I want a
/// node" before you know what kind. Burying either behind a submenu makes the
/// two most-used entries the two slowest.
pub(crate) const NEW_TOP_LEVEL: &[NewEntry] = &[
    NewEntry {
        label: "🗀 Empty",
        hover: "a blank node — just a transform. Build it up with the Inspector's \
                ➕ Add Component (also groups / parents children).",
        make: NewNode::Matter(new_empty),
    },
    NewEntry {
        label: "⌖ Camera",
        hover: "a viewpoint you can give play-mode authority. Set its projection to \
                orthographic for a 2D game",
        make: NewNode::Camera,
    },
];

fn new_empty() -> MatterDoc {
    MatterDoc::Empty
}
fn new_blob() -> MatterDoc {
    MatterDoc::Blob { scale: 1.0 }
}
fn new_field_shape() -> MatterDoc {
    MatterDoc::FieldShape { radius: 1.5 }
}
fn new_tilemap() -> MatterDoc {
    MatterDoc::Tilemap { cols: 16, rows: 16, tile: 1.0, data: Vec::new(), tileset: String::new() }
}
fn new_sprite_batch() -> MatterDoc {
    MatterDoc::SpriteBatch { size: 1.0 }
}
fn new_sprite() -> MatterDoc {
    // Pixels per unit by default, at 32 — a pixel artist's usual cell, and the
    // setting that makes a dropped image come out the size it looks.
    MatterDoc::Sprite {
        ppu: 32.0,
        size: 1.0,
        cell: 0,
        flip_x: false,
        flip_y: false,
        pivot: [0.5, 0.5],
    }
}
fn new_point_light() -> MatterDoc {
    MatterDoc::PointLight {
        color: [1.0, 0.95, 0.85],
        intensity: 1.0,
        range: 10.0,
        shape: Default::default(),
        shadows: false,
        spot: None,
    }
}
fn new_spot_light() -> MatterDoc {
    MatterDoc::PointLight {
        color: [1.0, 0.95, 0.85],
        intensity: 4.0,
        range: 14.0,
        shape: Default::default(),
        // See above: a spot is aimed at one thing, so it shadows by default
        // where an omni lamp does not.
        shadows: true,
        spot: Some(floptle_scene::SpotDoc { angle: 45.0, softness: 0.25 }),
    }
}
fn new_light_probes() -> MatterDoc {
    MatterDoc::from(&Matter::default_light_probes())
}
fn new_reflection_probe() -> MatterDoc {
    MatterDoc::from(&Matter::default_reflection_probe())
}
fn new_water() -> MatterDoc {
    MatterDoc::from(&Matter::default_water())
}
fn new_gravity() -> MatterDoc {
    MatterDoc::GravityVolume { radial: false, strength: 9.81, radius: 20.0 }
}
fn new_nav_mesh() -> MatterDoc {
    // The id is replaced with a fresh one on the way in — see `cmd.add`.
    MatterDoc::from(&Matter::default_nav_mesh(1))
}
fn new_nav_link() -> MatterDoc {
    MatterDoc::from(&Matter::default_nav_link(1))
}
fn new_nav_area() -> MatterDoc {
    MatterDoc::from(&Matter::default_nav_area())
}

/// A `MatterDoc`'s VARIANT name — not its human label.
///
/// Exhaustive on purpose: adding a variant to `MatterDoc` stops compiling here,
/// which is the first place the author is asked "and where does it live in the
/// ✚ New menu?".
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn matter_doc_variant(m: &MatterDoc) -> &'static str {
    match m {
        MatterDoc::Primitive { .. } => "Primitive",
        MatterDoc::Blob { .. } => "Blob",
        MatterDoc::Mesh { .. } => "Mesh",
        MatterDoc::Empty => "Empty",
        MatterDoc::Terrain { .. } => "Terrain",
        MatterDoc::MapMesh { .. } => "MapMesh",
        MatterDoc::Camera { .. } => "Camera",
        MatterDoc::PointLight { .. } => "PointLight",
        MatterDoc::GravityVolume { .. } => "GravityVolume",
        MatterDoc::WaterVolume { .. } => "WaterVolume",
        MatterDoc::NavMesh { .. } => "NavMesh",
        MatterDoc::NavLink { .. } => "NavLink",
        MatterDoc::NavArea { .. } => "NavArea",
        MatterDoc::LightProbes { .. } => "LightProbes",
        MatterDoc::ReflectionProbe { .. } => "ReflectionProbe",
        MatterDoc::FieldShape { .. } => "FieldShape",
        MatterDoc::Tilemap { .. } => "Tilemap",
        MatterDoc::SpriteBatch { .. } => "SpriteBatch",
        MatterDoc::Sprite { .. } => "Sprite",
        MatterDoc::Skybox { .. } => "Skybox",
        MatterDoc::PostProcess { .. } => "PostProcess",
    }
}

#[cfg(test)]
mod new_menu_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Node types the ✚ New menu deliberately does not offer, and why. A reason
    /// per line, because "it wasn't on the list" is not something anybody can
    /// check later.
    const NOT_IN_THE_MENU: &[(&str, &str)] = &[
        ("Mesh", "created by dragging a model asset in from Assets — there is nothing to pick"),
        ("Terrain", "offered, but as an intent (a terrain needs a field before it is a node)"),
        ("Camera", "offered at the top level, not inside a group"),
        ("MapMesh", "offered as the seven ▦ Model shape entries, which spawn geometry as well"),
        // Every scene already has one, self-healed on load — and `delete_node`
        // REFUSES to remove a PostProcess node. Offering it in the menu made an
        // undeletable duplicate one click away, which is a dead end rather than
        // a feature.
        ("PostProcess", "every scene already has one, and a second cannot be deleted"),
        ("Skybox", "every scene already has one, and a second cannot be deleted"),
    ];

    /// Every variant name declared on `MatterDoc`, read out of the scene crate's
    /// own source.
    ///
    /// Read from the source rather than from a list kept here, for the same
    /// reason the docs coverage tests do: a list maintained beside the thing it
    /// describes is a list that drifts, and the variant that goes missing is
    /// always the one nobody remembered to register.
    fn declared_matter_variants() -> BTreeSet<&'static str> {
        // Leaked so the parsed names can be `&'static str` like everything else
        // they are compared against. One allocation, in a test.
        let src: &'static str = Box::leak(
            floptle_vfs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../floptle-scene/src/lib.rs"),
            )
            .expect("read floptle-scene")
            .into_boxed_str(),
        );
        let body = {
            let at = src.find("pub enum MatterDoc {").expect("MatterDoc enum");
            let rest = &src[at..];
            let end = rest.find("\n}").expect("end of MatterDoc");
            &rest[..end]
        };
        // Variant names sit at ONE indent level (four spaces) and start with a
        // capital; the name is the leading identifier, whatever follows it on
        // the line (`Mesh { asset_path: String },` is one line, `Camera {` is
        // another). Attributes and doc comments do not start with a capital.
        let declared: BTreeSet<&str> = body
            .lines()
            .filter(|l| {
                let indent = l.len() - l.trim_start().len();
                indent == 4
            })
            .map(|l| l.trim())
            .filter(|l| l.starts_with(|c: char| c.is_ascii_uppercase()))
            .map(|l| {
                let n = l.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(l.len());
                &l[..n]
            })
            .filter(|l| !l.is_empty())
            .collect();
        assert!(declared.len() > 15, "parsed too few variants: {declared:?}");
        declared
    }

    fn offered() -> BTreeSet<&'static str> {
        NEW_CATALOG
            .iter()
            .flat_map(|g| g.items.iter())
            .chain(NEW_TOP_LEVEL.iter())
            .filter_map(|e| match e.make {
                NewNode::Matter(make) => Some(matter_doc_variant(&make())),
                _ => None,
            })
            .collect()
    }

    /// **Every node type has a home.** Grouping a menu is only safe if grouping
    /// cannot become hiding, so this reads `MatterDoc`'s own variant list out of
    /// the scene crate and fails naming anything the menu cannot create.
    ///
    /// Read from the source rather than from a list kept here, for the same
    /// reason the docs coverage tests do: a list maintained beside the thing it
    /// describes is a list that drifts, and the variant that goes missing is
    /// always the one nobody remembered to register.
    #[test]
    fn every_node_type_is_in_the_new_menu() {
        let declared = declared_matter_variants();
        let offered = offered();
        let exempt: BTreeSet<&str> = NOT_IN_THE_MENU.iter().map(|(k, _)| *k).collect();
        let missing: Vec<&str> =
            declared.iter().copied().filter(|k| !offered.contains(k) && !exempt.contains(k)).collect();
        assert!(
            missing.is_empty(),
            "these node types cannot be created from the ✚ New menu: {missing:?}\n\
             Put each one in a group in NEW_CATALOG, or list it in NOT_IN_THE_MENU with a reason."
        );

        // And the exemptions have to stay honest: one for a variant that no
        // longer exists is a note about nothing.
        let stale: Vec<&str> =
            exempt.iter().copied().filter(|k| !declared.contains(k)).collect();
        assert!(stale.is_empty(), "NOT_IN_THE_MENU names variants that are gone: {stale:?}");
    }

    /// Node types the Inspector's Add Component ▸ Type menu deliberately cannot
    /// switch a node into, and why. A reason per line, for the same reason as
    /// `NOT_IN_THE_MENU`: "it wasn't on the list" is not something anybody can
    /// check later.
    const NOT_IN_THE_TYPE_MENU: &[(&str, &str)] = &[
        ("Mesh", "needs a model asset — drag one in from Assets, there is nothing to pick"),
        ("Terrain", "an SDF field with volumes of its own; ✚ New builds one as an intent"),
        ("MapMesh", "blockout geometry the Map tools own, and switching to it has no shape to be"),
        ("PostProcess", "every scene already has exactly one, and it refuses to be deleted"),
    ];

    /// A `Matter`'s VARIANT name — the runtime twin of [`matter_doc_variant`],
    /// and exhaustive for the same reason: a new variant stops compiling here,
    /// which is where the author is asked "and can a node be switched into it?".
    fn matter_variant(m: &Matter) -> &'static str {
        match m {
            Matter::Primitive { .. } => "Primitive",
            Matter::Blob { .. } => "Blob",
            Matter::Mesh { .. } => "Mesh",
            Matter::Empty => "Empty",
            Matter::MapMesh { .. } => "MapMesh",
            Matter::Terrain { .. } => "Terrain",
            Matter::Camera { .. } => "Camera",
            Matter::PointLight { .. } => "PointLight",
            Matter::GravityVolume { .. } => "GravityVolume",
            Matter::NavMesh { .. } => "NavMesh",
            Matter::NavLink { .. } => "NavLink",
            Matter::NavArea { .. } => "NavArea",
            Matter::WaterVolume { .. } => "WaterVolume",
            Matter::FieldShape { .. } => "FieldShape",
            Matter::LightProbes { .. } => "LightProbes",
            Matter::ReflectionProbe { .. } => "ReflectionProbe",
            Matter::Tilemap { .. } => "Tilemap",
            Matter::SpriteBatch { .. } => "SpriteBatch",
            Matter::Sprite { .. } => "Sprite",
            Matter::Skybox { .. } => "Skybox",
            Matter::PostProcess { .. } => "PostProcess",
        }
    }

    /// **A node can be switched into every type it can be created as.** The two
    /// menus are written in different places and nothing connected them, so they
    /// drifted: ✚ New grew the flat three and Add Component ▸ Type did not, and
    /// the only way to turn an existing node into a Sprite was to make a new one
    /// and move the work across.
    ///
    /// Held against `Matter`'s own variants rather than against a list kept here,
    /// so the type that goes missing is the one nobody remembered to register.
    #[test]
    fn every_node_type_can_be_switched_into_or_is_named_as_left_out() {
        let declared = declared_matter_variants();
        let offered: BTreeSet<&str> =
            type_catalog().iter().map(|(_, m)| matter_variant(m)).collect();
        let exempt: BTreeSet<&str> = NOT_IN_THE_TYPE_MENU.iter().map(|(k, _)| *k).collect();
        let missing: Vec<&str> = declared
            .iter()
            .copied()
            .filter(|k| !offered.contains(k) && !exempt.contains(k))
            .collect();
        assert!(
            missing.is_empty(),
            "these node types cannot be reached from Add Component ▸ Type: {missing:?}\n\
             Add each to type_catalog(), or list it in NOT_IN_THE_TYPE_MENU with a reason."
        );
        let stale: Vec<&str> =
            exempt.iter().copied().filter(|k| !declared.contains(k)).collect();
        assert!(stale.is_empty(), "NOT_IN_THE_TYPE_MENU names variants that are gone: {stale:?}");
    }

    /// **One glyph, one node type.** The icon is how a node is identified at a
    /// glance in the Hierarchy and in the Inspector header, so two types wearing
    /// the same one is not a cosmetic clash: it makes the picture lie. Three
    /// pairs had collided — Capsule with Sprite Batch, Model Mesh with Tilemap,
    /// and Reflection Probe wore one glyph in the Inspector and another in the
    /// ✚ New menu, which reads as two different things.
    #[test]
    fn no_two_node_types_share_an_icon() {
        // A primitive is one variant with a shape inside it, and each shape has
        // its own glyph, so the key has to name the shape too.
        fn key(m: &Matter) -> String {
            match m {
                Matter::Primitive { shape, .. } => format!("Primitive::{shape:?}"),
                other => matter_variant(other).to_string(),
            }
        }
        // A type can wear more than one glyph on purpose — Spot Light IS a point
        // light, aimed, and the menu says so with its own arrow — so a type maps
        // to a SET. What must not happen is one glyph meaning two types.
        let mut icons_of: std::collections::BTreeMap<String, BTreeSet<&str>> = Default::default();
        for (label, m) in type_catalog() {
            let set = icons_of.entry(key(&m)).or_default();
            set.insert(label.split_whitespace().next().unwrap_or_default());
            // The Inspector header's glyph counts too, and it is per TYPE rather
            // than per menu entry: a spot light's row says ◤ and its header says
            // ●, and both have to be unique against every other type.
            set.insert(matter_icon(&m));
        }
        let mut by_icon: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (k, icons) in &icons_of {
            for icon in icons {
                by_icon.entry(icon).or_default().push(k.as_str());
            }
        }
        let clashes: Vec<String> = by_icon
            .iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(k, v)| format!("{k} is {}", v.join(" AND ")))
            .collect();
        assert!(clashes.is_empty(), "two node types share a glyph: {clashes:?}");

        // And the ✚ New menu has to agree with the Inspector about each one: the
        // same node called two different things in two menus is how somebody
        // concludes they are two features.
        for entry in NEW_CATALOG.iter().flat_map(|g| g.items.iter()).chain(NEW_TOP_LEVEL.iter()) {
            let NewNode::Matter(make) = entry.make else { continue };
            let doc = make();
            let k = match &doc {
                MatterDoc::Primitive { shape, .. } => format!("Primitive::{shape:?}"),
                other => matter_doc_variant(other).to_string(),
            };
            let Some(want) = icons_of.get(&k) else { continue };
            let icon = entry.label.split_whitespace().next().unwrap_or_default();
            assert!(
                want.contains(icon),
                "✚ New draws {k} as {icon} and Add Component ▸ Type as {want:?}"
            );
        }
    }

    /// **No group is a wall of text.** The whole point of grouping was that a
    /// twenty-item list is unreadable; a twelve-item group is the same list one
    /// level down. If a group grows past this it wants splitting, and the split
    /// is a decision to make deliberately rather than discover.
    #[test]
    fn no_group_is_longer_than_a_glance() {
        for g in NEW_CATALOG {
            assert!(
                g.items.len() <= 9,
                "the {} group has {} entries — split it",
                g.title,
                g.items.len()
            );
        }
        assert!(
            NEW_TOP_LEVEL.len() + NEW_CATALOG.len() <= 10,
            "the top level of ✚ New is back to being a list"
        );
    }

    /// Every entry says what it is. A menu of unexplained glyphs is the state
    /// this menu was in for the ▦ Model shapes, which had no hover text at all.
    #[test]
    fn every_group_explains_itself() {
        for g in NEW_CATALOG {
            assert!(!g.hover.is_empty(), "{} has no hover text", g.title);
            assert!(!g.title.is_empty());
        }
        for e in NEW_TOP_LEVEL {
            assert!(!e.hover.is_empty(), "{} has no hover text", e.label);
        }
        // …and every ITEM, which is what the ▦ Model shapes were missing while
        // this test passed: it checked the groups and the top level and never
        // the entries inside a group.
        for g in NEW_CATALOG {
            for e in g.items {
                assert!(!e.hover.is_empty(), "{} ▸ {} has no hover text", g.title, e.label);
            }
        }
    }
}
