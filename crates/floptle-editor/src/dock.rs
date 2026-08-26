//! The dock-shell plumbing: which tabs exist ([`EditorTab`]), the default
//! layout, focus/query helpers over the `egui_dock` state, and the Game
//! viewport's aspect-ratio modes.

/// Which dockable panel a tab shows.
///
/// `Serialize`/`Deserialize` because the whole `DockState` is persisted — see
/// [`crate::layout`]. Variants are named in the file, so **renaming one drops
/// that tab out of everybody's saved layout**; adding one is free.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum EditorTab {
    Hierarchy,
    Inspector,
    Terrain,
    Assets,
    Console,
    Scene,
    Game,
    Scripting,
    /// The animation timeline (dopesheet): preview, scrub, record keys, events.
    Animation,
    /// The animation controller graph: states, transitions, fades, layers.
    AnimGraph,
    /// The particle-effect timeline: tracks, clips, bursts, live preview.
    Particles,
    /// The audio mixer: tracks, faders, effect chains, routing, meters.
    Mixer,
    /// The shader node-graph canvas (a `.flsl`'s box-and-wire view).
    ShaderGraph,
    /// The vertex-paint brush settings (color, radius, strength, falloff, channels).
    Paint,
    /// The image editor: one canvas for pixels, paint and vectors, exporting the
    /// PNG the rest of the engine already references (docs/image-editor.md).
    Image,
    /// The map-building suite: blockout shapes, sub-object mode, modeling ops,
    /// per-face material slots (docs/map-tools.md).
    Map,
    /// The tilemap suite: layers, tools, the palette, and the tileset editor
    /// (per-tile collision, tags, autotile groups, animation). See docs/tilemaps.md.
    Tiles,
    /// The UI authoring canvas: one game-UI layer at design resolution, with
    /// rulers, guides, snapping, align/distribute, state preview and an
    /// element outline (docs/ui-styles.md, phase C).
    UiDesign,
    /// 🎓 Learn: follow-along tutorials whose steps tick themselves off as the
    /// project comes to match them (see `learn.rs`).
    Learn,
    /// Project Settings: game/rendering/layers/input, searchable by section.
    /// Opened from Edit ⏵ Project Settings; a real dock tab, so it can be
    /// dragged anywhere, split beside the viewport, or left closed.
    Settings,
    /// 📦 Packages: what this project has installed, how to add one, and the
    /// catalogue to browse.
    ///
    /// A tab rather than the floating window it started as. Browsing a
    /// catalogue is not a modal errand — you look at a package, look at the
    /// scene, look back — and a window that floats over the scene you are
    /// judging it against is a window you close to think and reopen to act.
    Packages,
    /// A tab a **package** registered with `ed.tab`. The `u64` is
    /// [`crate::ext::tab_key`] — a hash of `<package id>::<title>`, so the
    /// saved layout survives a reload, a restart, and the package being
    /// temporarily absent.
    ///
    /// The title is not stored: it is asked of the package at draw time, so
    /// renaming a tab renames it rather than orphaning it.
    Package(u64),
}

impl EditorTab {
    pub(crate) fn title(self) -> &'static str {
        match self {
            EditorTab::Hierarchy => "Hierarchy",
            EditorTab::Inspector => "Inspector",
            EditorTab::Terrain => "Δ Terrain",
            EditorTab::Map => "▦ Model",
            EditorTab::Tiles => "◫ Tiles",
            EditorTab::Assets => "Assets",
            EditorTab::Console => "Console",
            EditorTab::Scene => "⌖ Scene",
            EditorTab::Game => "⏵ Game",
            EditorTab::Scripting => "Scripting",
            EditorTab::Animation => "⏱ Animating",
            EditorTab::AnimGraph => "◎ Controller",
            EditorTab::Particles => "✱ Particles",
            EditorTab::Mixer => "≣ Mixer",
            EditorTab::ShaderGraph => "◈ Shaders",
            EditorTab::Paint => "◨ Paint",
            EditorTab::Image => "🖼 Image",
            EditorTab::UiDesign => "◫ UI",
            EditorTab::Learn => "🎓 Learn",
            EditorTab::Settings => "⚙ Settings",
            EditorTab::Packages => "📦 Packages",
            // Replaced by the package's own title in the tab bar — this is only
            // the fallback for a tab whose package is not loaded.
            EditorTab::Package(_) => "Package",
        }
    }

    /// Every tab, for the tab-title glyph test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const ALL: &'static [EditorTab] = &[
        EditorTab::Hierarchy,
        EditorTab::Inspector,
        EditorTab::Terrain,
        EditorTab::Assets,
        EditorTab::Console,
        EditorTab::Scene,
        EditorTab::Game,
        EditorTab::Scripting,
        EditorTab::Animation,
        EditorTab::AnimGraph,
        EditorTab::Particles,
        EditorTab::Mixer,
        EditorTab::ShaderGraph,
        EditorTab::Paint,
        EditorTab::Image,
        EditorTab::Map,
        EditorTab::Tiles,
        EditorTab::UiDesign,
        EditorTab::Learn,
        EditorTab::Settings,
        EditorTab::Packages,
    ];
}

/// Bring `tab` to the front, **re-adding it if it is not open**.
///
/// The second half is the part that matters. Tabs arrive after layouts do:
/// anybody upgrading has a saved dock with no 📦 Packages in it, and a menu item
/// that silently does nothing because the tab was closed once, six months ago,
/// is indistinguishable from a broken menu item.
pub(crate) fn focus(dock: &mut egui_dock::DockState<EditorTab>, tab: EditorTab) {
    if let Some(path) = dock.find_tab(&tab) {
        let _ = dock.set_active_tab(path);
    } else {
        dock.push_to_focused_leaf(tab);
    }
}

/// Focus the ⚙ Settings dock tab — creating it if it isn't open. Project
/// Settings is a TAB, not a modal window: it can be dragged into any panel,
/// split beside the viewport, or closed like anything else. It is deliberately
/// absent from the default layout — you open it when you need it.
pub(crate) fn focus_settings_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::Settings);
}

/// Focus the 📦 Packages dock tab. Like ⚙ Settings it is absent from the
/// default layout and appears where you are working when you ask for it.
pub(crate) fn focus_packages_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::Packages);
}

/// Show or hide a package's own tab. Showing adds it where the user is working
/// and brings it forward; hiding takes it out of the layout entirely, which is
/// what the ✕ on the tab does too.
pub(crate) fn set_package_tab_open(
    dock: &mut egui_dock::DockState<EditorTab>,
    key: u64,
    open: bool,
) {
    let tab = EditorTab::Package(key);
    if open {
        focus(dock, tab);
    } else if let Some(path) = dock.find_tab(&tab) {
        dock.remove_tab(path);
    }
}

/// True when the Game tab is the front (active) tab of its dock leaf — i.e. the game
/// (active-camera) view should drive the full-window 3D render this frame. (When
/// false the editor free-fly camera renders, for the Scene tab.)
pub(crate) fn game_tab_active(dock: &egui_dock::DockState<EditorTab>) -> bool {
    tab_is_front(dock, EditorTab::Game)
}

/// True when `tab` is the front (active) tab of some dock leaf — i.e. it's actually
/// visible (egui_dock only runs the active tab's `ui` per leaf).
pub(crate) fn tab_is_front(dock: &egui_dock::DockState<EditorTab>, tab: EditorTab) -> bool {
    dock.main_surface()
        .iter()
        .any(|n| n.get_leaf().and_then(|l| l.tabs.get(l.active.0)) == Some(&tab))
}

/// True when BOTH the Scene and Game tabs are visible at once (split into separate
/// leaves), so they must render independent camera views rather than sharing one.
pub(crate) fn scene_and_game_split(dock: &egui_dock::DockState<EditorTab>) -> bool {
    tab_is_front(dock, EditorTab::Scene) && tab_is_front(dock, EditorTab::Game)
}

/// The default layout, grouped by what each dock is FOR:
///
/// - **left** — what the scene contains and what you build it out of:
///   Hierarchy, ▦ Model.
/// - **centre** — the viewports and the full-canvas editors that replace them:
///   Scene / Game, Scripting, ◈ Shaders, ◎ Controller.
/// - **right** — properties of whatever is selected: Inspector, Δ Terrain,
///   ◨ Paint — and 🎓 Learn, which is read beside the viewport you're working
///   in rather than instead of it.
/// - **bottom** — the project and the timelines that scrub it: Assets,
///   Console, ⏱ Animating, ✱ Particles, ≣ Mixer.
///
/// Users can drag/re-dock freely; **Window ▸ Reset layout** comes back here.
pub(crate) fn default_dock() -> egui_dock::DockState<EditorTab> {
    use egui_dock::{DockState, NodeIndex};
    // Scene (editor view) and Game (active-camera view) share the central leaf
    // with the graph/text editors — only the front tab renders, and which of
    // Scene/Game is front picks the camera. Scene first so the editor view is
    // the default on launch.
    let mut dock = DockState::new(vec![
        EditorTab::Scene,
        EditorTab::Game,
        EditorTab::Scripting,
        EditorTab::ShaderGraph,
        EditorTab::AnimGraph,
        EditorTab::Image,
        EditorTab::UiDesign,
    ]);
    let surface = dock.main_surface_mut();
    let [central, _] =
        surface.split_left(
            NodeIndex::root(),
            0.19,
            vec![EditorTab::Hierarchy, EditorTab::Map, EditorTab::Tiles],
        );
    let [central, _] = surface.split_right(
        central,
        0.78,
        vec![EditorTab::Inspector, EditorTab::Terrain, EditorTab::Paint, EditorTab::Learn],
    );
    let [_, _] = surface.split_below(
        central,
        0.72,
        vec![
            EditorTab::Assets,
            EditorTab::Console,
            EditorTab::Animation,
            EditorTab::Particles,
            EditorTab::Mixer,
        ],
    );
    dock
}

/// Focus the 🎓 Learn dock tab — creating it if it isn't open.
///
/// Needed because the tab arrived after the layout did: anyone upgrading has a
/// saved dock with no Learn in it, and a tutorial nobody can find teaches
/// nothing. Help ▸ 🎓 Learn comes here.
pub(crate) fn focus_learn_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::Learn);
}

/// Focus the Scripting tab (used after double-click-to-open-a-script).
pub(crate) fn focus_scripting_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    let surface = dock.main_surface_mut();
    if let Some((node, tab)) = surface.find_tab(&EditorTab::Scripting) {
        let _ = surface.set_active_tab(node, tab);
    }
}

/// Focus the ◈ Shaders (graph) tab — re-adding it if the user closed it. Used
/// by double-clicking a `.flsl` asset and the Inspector's shader row.
pub(crate) fn focus_shader_graph_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::ShaderGraph);
}

/// Focus the Terrain dock tab — re-adding it if the user closed it. Used when the
/// Sculpt tool is selected or "Open Terrain tools" is clicked.
pub(crate) fn focus_terrain_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::Terrain);
}

/// Focus the ◫ Tiles dock tab, re-adding it if it was closed.
pub(crate) fn focus_tiles_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::Tiles);
}

/// Focus the Paint dock tab — re-adding it if the user closed it. Used when the
/// Paint tool is selected, so the brush settings are never a tab-hunt away.
pub(crate) fn focus_paint_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::Paint);
}

/// Focus the 🖼 Image dock tab — re-adding it if the user closed it. Used when
/// an image asset is opened, so the canvas is never a tab-hunt away.
pub(crate) fn focus_image_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::Image);
}

/// Focus the ◫ UI dock tab — re-adding it if the user closed it. Used by
/// Add ⏵ UI and the Inspector, so building a screen never means hunting tabs.
pub(crate) fn focus_ui_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::UiDesign);
}

/// Focus the ▦ Model dock tab — re-adding it if the user closed it. Used when the
/// Map tool is selected, so the shape/op controls are never a tab-hunt away.
pub(crate) fn focus_map_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    focus(dock, EditorTab::Map);
}

/// Viewport framing presets for the in-Scene resolution simulator.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AspectMode {
    #[default]
    Free,
    Desktop,
    Mobile,
    Square,
}

impl AspectMode {
    pub(crate) const ALL: [AspectMode; 4] =
        [AspectMode::Free, AspectMode::Desktop, AspectMode::Mobile, AspectMode::Square];
    pub(crate) fn label(self) -> &'static str {
        match self {
            AspectMode::Free => "Free",
            AspectMode::Desktop => "Desktop · 16:9",
            AspectMode::Mobile => "Mobile · 9:16",
            AspectMode::Square => "Square · 1:1",
        }
    }
    /// Width / height, or `None` for "fill the panel".
    pub(crate) fn ratio(self) -> Option<f32> {
        match self {
            AspectMode::Free => None,
            AspectMode::Desktop => Some(16.0 / 9.0),
            AspectMode::Mobile => Some(9.0 / 16.0),
            AspectMode::Square => Some(1.0),
        }
    }
}
