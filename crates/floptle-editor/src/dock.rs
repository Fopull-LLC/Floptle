//! The dock-shell plumbing: which tabs exist ([`EditorTab`]), the default
//! layout, focus/query helpers over the `egui_dock` state, and the Game
//! viewport's aspect-ratio modes.

/// Which dockable panel a tab shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
}

impl EditorTab {
    pub(crate) fn title(self) -> &'static str {
        match self {
            EditorTab::Hierarchy => "Hierarchy",
            EditorTab::Inspector => "Inspector",
            EditorTab::Terrain => "Δ Terrain",
            EditorTab::Assets => "Assets",
            EditorTab::Console => "Console",
            EditorTab::Scene => "⌖ Scene",
            EditorTab::Game => "⏵ Game",
            EditorTab::Scripting => "Scripting",
            EditorTab::Animation => "✎ Animating",
            EditorTab::AnimGraph => "◉ Controller",
        }
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

/// The default layout: Hierarchy left, Inspector right, Assets bottom, with the
/// Scene + Scripting tabs filling the center. Users can drag/re-dock freely.
pub(crate) fn default_dock() -> egui_dock::DockState<EditorTab> {
    use egui_dock::{DockState, NodeIndex};
    // Scene (editor view), Game (active-camera view), and Scripting share the central
    // leaf — only the front tab renders, and which of Scene/Game is front picks the
    // camera. Scene first so the editor view is the default on launch.
    let mut dock = DockState::new(vec![EditorTab::Scene, EditorTab::Game, EditorTab::Scripting]);
    let surface = dock.main_surface_mut();
    let [central, _] = surface.split_left(NodeIndex::root(), 0.18, vec![EditorTab::Hierarchy]);
    // Inspector + Terrain tabs share the right dock (Inspector shown first).
    let [central, _] =
        surface.split_right(central, 0.78, vec![EditorTab::Inspector, EditorTab::Terrain]);
    // Console + the animation tabs sit beside Assets in the bottom dock.
    let [_, _] = surface.split_below(
        central,
        0.72,
        vec![
            EditorTab::Assets,
            EditorTab::Console,
            EditorTab::Animation,
            EditorTab::AnimGraph,
        ],
    );
    dock
}

/// Focus the Scripting tab (used after double-click-to-open-a-script).
pub(crate) fn focus_scripting_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    let surface = dock.main_surface_mut();
    if let Some((node, tab)) = surface.find_tab(&EditorTab::Scripting) {
        let _ = surface.set_active_tab(node, tab);
    }
}

/// Focus the Terrain dock tab — re-adding it if the user closed it. Used when the
/// Sculpt tool is selected or "Open Terrain tools" is clicked.
pub(crate) fn focus_terrain_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    if let Some(path) = dock.find_tab(&EditorTab::Terrain) {
        let _ = dock.set_active_tab(path);
    } else {
        dock.push_to_focused_leaf(EditorTab::Terrain);
    }
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
