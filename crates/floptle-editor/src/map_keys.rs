//! Keybinds for the ⬢ Map tool: every control it has, bound to a chord you can
//! change, with conflicts made impossible rather than merely unlikely.
//!
//! Two rules make "doesn't interfere with anything else" a property of the
//! design instead of a promise:
//!
//! 1. **Context.** Map chords are only ever consulted while the Map TOOL is
//!    active, the cursor isn't in a text field, and Ctrl is up. Every
//!    application-wide shortcut is either Ctrl-modified (undo/save/copy…) or
//!    lives outside that context, so nothing the map binds can reach them.
//! 2. **Reservation.** Inside that context the editor still owns some keys —
//!    the fly camera, the tool digits, focus/grid/gizmo toggles. Those arms
//!    don't test modifiers, so a reserved key is reserved with EVERY modifier;
//!    [`reserved`] knows them by name and [`MapKeys::conflict`] refuses to bind
//!    onto one. The same check catches a chord another map command already
//!    holds.
//!
//! Bindings persist per user next to the other preferences (one
//! `command chord` line each), so a rebind survives a restart and an unknown
//! command in the file is ignored rather than fatal.

use winit::keyboard::KeyCode;

/// Everything the Map tool can do from the keyboard. One entry per control in
/// the Map tab, so "every control has a hotkey" is checkable by eye.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MapCmd {
    // Draw
    DrawBox,
    DrawPlane,
    DrawWedge,
    DrawCylinder,
    DrawSphere,
    DrawStairs,
    DrawArch,
    ResolutionDown,
    ResolutionUp,
    TurnLeft,
    TurnRight,
    TurnAround,
    // Select
    ModeCycle,
    ModeVertex,
    ModeEdge,
    ModeFace,
    SelectAll,
    SelectNone,
    SelectGrow,
    SelectConnected,
    SelectCoplanar,
    SelectLoop,
    ToggleSelectHidden,
    // Transform
    GizmoCycle,
    GizmoMove,
    GizmoRotate,
    GizmoScale,
    OrientCycle,
    // Modify
    Extrude,
    Inset,
    Subdivide,
    Bridge,
    DeleteFaces,
    SplitOff,
    Flip,
    FlipAll,
    Weld,
    SnapToGrid,
    CenterPivot,
    PivotToSelection,
    NewMaterialFromSelection,
}

impl MapCmd {
    pub(crate) const ALL: [MapCmd; 41] = [
        MapCmd::DrawBox,
        MapCmd::DrawPlane,
        MapCmd::DrawWedge,
        MapCmd::DrawCylinder,
        MapCmd::DrawSphere,
        MapCmd::DrawStairs,
        MapCmd::DrawArch,
        MapCmd::ResolutionDown,
        MapCmd::ResolutionUp,
        MapCmd::TurnLeft,
        MapCmd::TurnRight,
        MapCmd::TurnAround,
        MapCmd::ModeCycle,
        MapCmd::ModeVertex,
        MapCmd::ModeEdge,
        MapCmd::ModeFace,
        MapCmd::SelectAll,
        MapCmd::SelectNone,
        MapCmd::SelectGrow,
        MapCmd::SelectConnected,
        MapCmd::SelectCoplanar,
        MapCmd::SelectLoop,
        MapCmd::ToggleSelectHidden,
        MapCmd::GizmoCycle,
        MapCmd::GizmoMove,
        MapCmd::GizmoRotate,
        MapCmd::GizmoScale,
        MapCmd::OrientCycle,
        MapCmd::Extrude,
        MapCmd::Inset,
        MapCmd::Subdivide,
        MapCmd::Bridge,
        MapCmd::DeleteFaces,
        MapCmd::SplitOff,
        MapCmd::Flip,
        MapCmd::FlipAll,
        MapCmd::Weld,
        MapCmd::SnapToGrid,
        MapCmd::CenterPivot,
        MapCmd::PivotToSelection,
        MapCmd::NewMaterialFromSelection,
    ];

    /// Section this command is listed under, matching the Map tab's sections.
    pub(crate) fn group(self) -> &'static str {
        use MapCmd::*;
        match self {
            DrawBox | DrawPlane | DrawWedge | DrawCylinder | DrawSphere | DrawStairs
            | DrawArch | ResolutionDown | ResolutionUp | TurnLeft | TurnRight | TurnAround => {
                "Draw"
            }
            ModeCycle | ModeVertex | ModeEdge | ModeFace | SelectAll | SelectNone | SelectGrow
            | SelectConnected | SelectCoplanar | SelectLoop | ToggleSelectHidden => "Select",
            GizmoCycle | GizmoMove | GizmoRotate | GizmoScale | OrientCycle => "Transform",
            _ => "Modify",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        use MapCmd::*;
        match self {
            DrawBox => "Draw box",
            DrawPlane => "Draw plane",
            DrawWedge => "Draw wedge",
            DrawCylinder => "Draw cylinder",
            DrawSphere => "Draw sphere",
            DrawStairs => "Draw stairs",
            DrawArch => "Draw arch",
            ResolutionDown => "Resolution −",
            ResolutionUp => "Resolution +",
            TurnLeft => "Turn 90° left",
            TurnRight => "Turn 90° right",
            TurnAround => "Turn around (180°)",
            ModeCycle => "Cycle vertex/edge/face",
            ModeVertex => "Vertex mode",
            ModeEdge => "Edge mode",
            ModeFace => "Face mode",
            SelectAll => "Select all",
            SelectNone => "Select none",
            SelectGrow => "Grow selection",
            SelectConnected => "Select connected",
            SelectCoplanar => "Select coplanar",
            SelectLoop => "Select edge loop",
            ToggleSelectHidden => "Select through surface",
            GizmoCycle => "Cycle move/rotate/scale",
            GizmoMove => "Gizmo: move",
            GizmoRotate => "Gizmo: rotate",
            GizmoScale => "Gizmo: scale",
            OrientCycle => "Cycle handle orientation",
            Extrude => "Extrude",
            Inset => "Inset",
            Subdivide => "Subdivide",
            Bridge => "Bridge",
            DeleteFaces => "Delete faces",
            SplitOff => "Split off",
            Flip => "Flip faces",
            FlipAll => "Flip all",
            Weld => "Weld",
            SnapToGrid => "Snap to grid",
            CenterPivot => "Center pivot",
            PivotToSelection => "Pivot to selection",
            NewMaterialFromSelection => "New material for selection",
        }
    }

    /// Stable name for the preferences file (never localise this).
    pub(crate) fn key_name(self) -> &'static str {
        use MapCmd::*;
        match self {
            DrawBox => "draw_box",
            DrawPlane => "draw_plane",
            DrawWedge => "draw_wedge",
            DrawCylinder => "draw_cylinder",
            DrawSphere => "draw_sphere",
            DrawStairs => "draw_stairs",
            DrawArch => "draw_arch",
            ResolutionDown => "resolution_down",
            ResolutionUp => "resolution_up",
            TurnLeft => "turn_left",
            TurnRight => "turn_right",
            TurnAround => "turn_around",
            ModeCycle => "mode_cycle",
            ModeVertex => "mode_vertex",
            ModeEdge => "mode_edge",
            ModeFace => "mode_face",
            SelectAll => "select_all",
            SelectNone => "select_none",
            SelectGrow => "select_grow",
            SelectConnected => "select_connected",
            SelectCoplanar => "select_coplanar",
            SelectLoop => "select_loop",
            ToggleSelectHidden => "select_hidden",
            GizmoCycle => "gizmo_cycle",
            GizmoMove => "gizmo_move",
            GizmoRotate => "gizmo_rotate",
            GizmoScale => "gizmo_scale",
            OrientCycle => "orient_cycle",
            Extrude => "extrude",
            Inset => "inset",
            Subdivide => "subdivide",
            Bridge => "bridge",
            DeleteFaces => "delete_faces",
            SplitOff => "split_off",
            Flip => "flip",
            FlipAll => "flip_all",
            Weld => "weld",
            SnapToGrid => "snap_to_grid",
            CenterPivot => "center_pivot",
            PivotToSelection => "pivot_to_selection",
            NewMaterialFromSelection => "new_material",
        }
    }

    fn from_key_name(s: &str) -> Option<Self> {
        MapCmd::ALL.into_iter().find(|c| c.key_name() == s)
    }
}

/// A key plus the modifiers that must be held with it. Ctrl is deliberately
/// absent: every Ctrl chord belongs to the application (undo, save, copy…), so
/// keeping it out of reach is what stops a map bind from ever shadowing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chord {
    pub(crate) key: KeyCode,
    pub(crate) shift: bool,
}

impl Chord {
    pub(crate) const fn new(key: KeyCode) -> Self {
        Self { key, shift: false }
    }

    pub(crate) const fn shifted(key: KeyCode) -> Self {
        Self { key, shift: true }
    }

    pub(crate) fn label(self) -> String {
        let k = key_label(self.key);
        if self.shift { format!("Shift+{k}") } else { k }
    }

    fn parse(s: &str) -> Option<Self> {
        let (shift, rest) = match s.strip_prefix("Shift+") {
            Some(r) => (true, r),
            None => (false, s),
        };
        Some(Self { key: key_from_label(rest)?, shift })
    }
}

/// Printable name for a key. Only the keys a binding may use need to round
/// trip; anything else shows its debug name and simply won't be bindable.
pub(crate) fn key_label(key: KeyCode) -> String {
    use KeyCode as K;
    let s = match key {
        K::Tab => "Tab",
        K::Space => "Space",
        K::Delete => "Delete",
        K::Backspace => "Backspace",
        K::BracketLeft => "[",
        K::BracketRight => "]",
        K::Comma => ",",
        K::Period => ".",
        K::Semicolon => ";",
        K::Quote => "'",
        K::Slash => "/",
        K::Backslash => "\\",
        K::Backquote => "`",
        K::Minus => "-",
        K::Equal => "=",
        _ => {
            let d = format!("{key:?}");
            return d
                .strip_prefix("Key")
                .or_else(|| d.strip_prefix("Digit"))
                .unwrap_or(&d)
                .to_string();
        }
    };
    s.to_string()
}

fn key_from_label(s: &str) -> Option<KeyCode> {
    use KeyCode as K;
    // Everything a chord may legally use, so parsing can't resurrect a key the
    // running build no longer understands.
    const NAMED: [(&str, KeyCode); 15] = [
        ("Tab", K::Tab),
        ("Space", K::Space),
        ("Delete", K::Delete),
        ("Backspace", K::Backspace),
        ("[", K::BracketLeft),
        ("]", K::BracketRight),
        (",", K::Comma),
        (".", K::Period),
        (";", K::Semicolon),
        ("'", K::Quote),
        ("/", K::Slash),
        ("\\", K::Backslash),
        ("`", K::Backquote),
        ("-", K::Minus),
        ("=", K::Equal),
    ];
    if let Some((_, k)) = NAMED.iter().find(|(n, _)| *n == s) {
        return Some(*k);
    }
    letters().into_iter().chain(function_keys()).find(|&k| key_label(k) == s)
}

/// Every key this build can name — the pool a binding may draw from, and what
/// the UI walks to show which of them the editor keeps for itself.
pub(crate) fn known_keys() -> Vec<KeyCode> {
    use KeyCode::*;
    let mut v = letters();
    v.extend(function_keys());
    v.extend([
        Tab, Space, Delete, Backspace, BracketLeft, BracketRight, Comma, Period, Semicolon,
        Quote, Slash, Backslash, Backquote, Minus, Equal, Escape, ArrowUp, ArrowDown, Enter,
        Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    ]);
    v
}

fn letters() -> Vec<KeyCode> {
    use KeyCode::*;
    vec![
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO,
        KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ]
}

fn function_keys() -> Vec<KeyCode> {
    use KeyCode::*;
    vec![F3, F4, F5, F6, F7, F8, F9, F10, F11, F12]
}

/// Keys the editor itself consumes in the very context map chords run in, and
/// what they do. Their handlers don't test modifiers, so a reserved key is
/// reserved with every modifier — `Shift+W` still flies the camera.
///
/// `Delete`/`Backspace` are deliberately absent: the map's delete-faces bind
/// falls through to "delete node" whenever no face is selected, which is a
/// deliberate share rather than a clash.
pub(crate) fn reserved(key: KeyCode) -> Option<&'static str> {
    use KeyCode as K;
    Some(match key {
        K::KeyW | K::KeyA | K::KeyS | K::KeyD | K::Space | K::KeyC => "the fly camera",
        K::KeyF => "focus selection",
        K::KeyQ => "clear selection",
        K::KeyG => "toggle grid",
        K::KeyH => "toggle gizmos",
        K::Escape => "cancel / quit",
        K::F1 => "play",
        K::F2 => "pause",
        K::ArrowUp | K::ArrowDown => "step through the hierarchy",
        K::Enter | K::NumpadEnter => "open/close folder",
        K::Digit1
        | K::Digit2
        | K::Digit3
        | K::Digit4
        | K::Digit5
        | K::Digit6
        | K::Digit7
        | K::Digit8
        | K::Digit9 => "the tool switcher",
        _ => return None,
    })
}

/// The user's map keybinds: one chord per command, defaults below.
#[derive(Clone, Debug)]
pub(crate) struct MapKeys {
    binds: Vec<(MapCmd, Chord)>,
}

impl Default for MapKeys {
    /// Defaults avoid every reserved key, so the shipped set is conflict-free
    /// by construction (asserted in the tests).
    fn default() -> Self {
        use KeyCode as K;
        use MapCmd::*;
        let b = |c: MapCmd, ch: Chord| (c, ch);
        Self {
            binds: vec![
                // Draw — the shape's initial, where the letter was free.
                b(DrawBox, Chord::new(K::KeyB)),
                b(DrawPlane, Chord::new(K::KeyL)),
                b(DrawWedge, Chord::new(K::KeyR)),
                b(DrawCylinder, Chord::new(K::KeyY)),
                b(DrawSphere, Chord::new(K::KeyO)),
                b(DrawStairs, Chord::new(K::KeyT)),
                b(DrawArch, Chord::new(K::KeyN)),
                b(ResolutionDown, Chord::new(K::BracketLeft)),
                b(ResolutionUp, Chord::new(K::BracketRight)),
                b(TurnLeft, Chord::new(K::Comma)),
                b(TurnRight, Chord::new(K::Period)),
                b(TurnAround, Chord::new(K::KeyZ)),
                // Select — J/K/M sit together under the right hand.
                b(ModeCycle, Chord::new(K::Tab)),
                b(ModeVertex, Chord::new(K::KeyJ)),
                b(ModeEdge, Chord::new(K::KeyK)),
                b(ModeFace, Chord::new(K::KeyM)),
                b(SelectAll, Chord::new(K::KeyU)),
                b(SelectNone, Chord::shifted(K::KeyU)),
                b(SelectGrow, Chord::new(K::KeyP)),
                b(SelectConnected, Chord::shifted(K::KeyP)),
                b(SelectCoplanar, Chord::shifted(K::KeyO)),
                b(SelectLoop, Chord::shifted(K::KeyL)),
                b(ToggleSelectHidden, Chord::shifted(K::KeyR)),
                // Transform.
                b(GizmoCycle, Chord::new(K::KeyX)),
                b(GizmoMove, Chord::shifted(K::KeyJ)),
                b(GizmoRotate, Chord::shifted(K::KeyK)),
                b(GizmoScale, Chord::shifted(K::KeyM)),
                b(OrientCycle, Chord::new(K::KeyV)),
                // Modify.
                b(Extrude, Chord::new(K::KeyE)),
                b(Inset, Chord::new(K::KeyI)),
                b(Subdivide, Chord::shifted(K::KeyI)),
                b(Bridge, Chord::shifted(K::KeyB)),
                b(DeleteFaces, Chord::new(K::Delete)),
                b(SplitOff, Chord::shifted(K::KeyX)),
                b(Flip, Chord::shifted(K::KeyZ)),
                b(FlipAll, Chord::shifted(K::KeyY)),
                b(Weld, Chord::shifted(K::KeyE)),
                b(SnapToGrid, Chord::shifted(K::KeyN)),
                b(CenterPivot, Chord::shifted(K::KeyT)),
                b(PivotToSelection, Chord::shifted(K::KeyV)),
                b(NewMaterialFromSelection, Chord::new(K::Semicolon)),
            ],
        }
    }
}

impl MapKeys {
    pub(crate) fn chord(&self, cmd: MapCmd) -> Option<Chord> {
        self.binds.iter().find(|(c, _)| *c == cmd).map(|(_, ch)| *ch)
    }

    /// The chord's label for a tooltip, or `—` when the command is unbound.
    pub(crate) fn label(&self, cmd: MapCmd) -> String {
        self.chord(cmd).map_or_else(|| "—".to_string(), |c| c.label())
    }

    /// The command a physical key press triggers, if any.
    pub(crate) fn command(&self, key: KeyCode, shift: bool) -> Option<MapCmd> {
        self.binds.iter().find(|(_, ch)| ch.key == key && ch.shift == shift).map(|(c, _)| *c)
    }

    /// Why `chord` can't be given to `cmd`, or `None` when it's free. Checked
    /// before every rebind, so a conflicting binding can't come into being.
    pub(crate) fn conflict(&self, cmd: MapCmd, chord: Chord) -> Option<String> {
        if let Some(what) = reserved(chord.key) {
            return Some(format!(
                "{} is {} — the editor answers it whatever modifiers are held",
                key_label(chord.key),
                what
            ));
        }
        self.binds
            .iter()
            .find(|(c, ch)| *c != cmd && *ch == chord)
            .map(|(c, _)| format!("{} is already \"{}\"", chord.label(), c.label()))
    }

    /// Bind `cmd` to `chord`, or explain why not.
    pub(crate) fn set(&mut self, cmd: MapCmd, chord: Chord) -> Result<(), String> {
        if let Some(why) = self.conflict(cmd, chord) {
            return Err(why);
        }
        match self.binds.iter_mut().find(|(c, _)| *c == cmd) {
            Some(slot) => slot.1 = chord,
            None => self.binds.push((cmd, chord)),
        }
        Ok(())
    }

    /// Serialize as `command chord` lines.
    fn encode(&self) -> String {
        let mut out = String::new();
        for cmd in MapCmd::ALL {
            if let Some(ch) = self.chord(cmd) {
                out.push_str(&format!("{} {}\n", cmd.key_name(), ch.label()));
            }
        }
        out
    }

    /// Parse those lines over the defaults: an unknown command or an
    /// unparseable chord is skipped, never fatal, and a binding the file
    /// doesn't mention keeps its default.
    fn decode(text: &str) -> Self {
        let mut keys = MapKeys::default();
        for line in text.lines() {
            let mut it = line.split_whitespace();
            let (Some(name), Some(chord)) = (it.next(), it.next()) else { continue };
            let (Some(cmd), Some(ch)) = (MapCmd::from_key_name(name), Chord::parse(chord)) else {
                continue;
            };
            if reserved(ch.key).is_none()
                && let Some(slot) = keys.binds.iter_mut().find(|(c, _)| *c == cmd)
            {
                slot.1 = ch;
            }
        }
        // A file that double-bound one chord (hand-edited) would give two commands
        // the same key; keep the first and unbind the rest so dispatch stays
        // deterministic.
        let mut seen: Vec<Chord> = Vec::new();
        keys.binds.retain(|(_, ch)| {
            let fresh = !seen.contains(ch);
            if fresh {
                seen.push(*ch);
            }
            fresh
        });
        keys
    }
}

fn path() -> Option<std::path::PathBuf> {
    crate::prefs::floptle_config_dir().map(|d| d.join("map_keys"))
}

pub(crate) fn load_map_keys() -> MapKeys {
    match path().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(t) => MapKeys::decode(&t),
        None => MapKeys::default(),
    }
}

pub(crate) fn save_map_keys(keys: &MapKeys) {
    let Some(p) = path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(p, keys.encode());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped bindings must be conflict-free — with each other AND with
    /// everything the editor already answers in the same context.
    #[test]
    fn the_default_binds_are_all_distinct_and_unreserved() {
        let keys = MapKeys::default();
        for cmd in MapCmd::ALL {
            let ch = keys.chord(cmd).unwrap_or_else(|| panic!("{cmd:?} has no default bind"));
            assert!(
                reserved(ch.key).is_none() || ch.key == KeyCode::Delete,
                "{cmd:?} defaults to {}, which the editor already owns",
                ch.label()
            );
        }
        let mut seen: Vec<(Chord, MapCmd)> = Vec::new();
        for cmd in MapCmd::ALL {
            let ch = keys.chord(cmd).unwrap();
            if let Some((_, other)) = seen.iter().find(|(c, _)| *c == ch) {
                panic!("{cmd:?} and {other:?} both default to {}", ch.label());
            }
            seen.push((ch, cmd));
        }
    }

    /// Every control the tool has is reachable from the keyboard.
    #[test]
    fn every_command_is_bound() {
        let keys = MapKeys::default();
        assert_eq!(MapCmd::ALL.len(), 41);
        assert!(MapCmd::ALL.into_iter().all(|c| keys.chord(c).is_some()));
    }

    #[test]
    fn rebinding_refuses_conflicts_and_takes_free_chords() {
        let mut keys = MapKeys::default();
        // Onto a key the editor owns...
        let err = keys.set(MapCmd::Extrude, Chord::new(KeyCode::KeyW)).unwrap_err();
        assert!(err.contains("fly camera"), "{err}");
        // ...with any modifier, because those handlers ignore modifiers.
        assert!(keys.set(MapCmd::Extrude, Chord::shifted(KeyCode::KeyF)).is_err());
        // Onto another map command's chord.
        let err = keys.set(MapCmd::Extrude, Chord::new(KeyCode::KeyI)).unwrap_err();
        assert!(err.contains("Inset"), "{err}");
        // Extrude keeps its own chord through all that.
        assert_eq!(keys.chord(MapCmd::Extrude), Some(Chord::new(KeyCode::KeyE)));
        // A free chord takes, and re-binding the SAME command to what it
        // already holds is not a conflict with itself.
        keys.set(MapCmd::Extrude, Chord::shifted(KeyCode::KeyQ)).unwrap_err();
        keys.set(MapCmd::Extrude, Chord::new(KeyCode::F5)).unwrap();
        assert_eq!(keys.command(KeyCode::F5, false), Some(MapCmd::Extrude));
        assert_eq!(keys.command(KeyCode::KeyE, false), None);
        keys.set(MapCmd::Extrude, Chord::new(KeyCode::F5)).unwrap();
    }

    /// Shift is part of the chord: the plain key and the shifted one are
    /// different binds and must not answer each other.
    #[test]
    fn shift_is_part_of_the_chord() {
        let keys = MapKeys::default();
        assert_eq!(keys.command(KeyCode::KeyI, false), Some(MapCmd::Inset));
        assert_eq!(keys.command(KeyCode::KeyI, true), Some(MapCmd::Subdivide));
    }

    #[test]
    fn bindings_round_trip_through_the_prefs_file() {
        let mut keys = MapKeys::default();
        keys.set(MapCmd::Extrude, Chord::shifted(KeyCode::Semicolon)).unwrap();
        keys.set(MapCmd::DrawArch, Chord::new(KeyCode::F7)).unwrap();
        let back = MapKeys::decode(&keys.encode());
        for cmd in MapCmd::ALL {
            assert_eq!(back.chord(cmd), keys.chord(cmd), "{cmd:?} did not round trip");
        }
    }

    /// The two-way promise: nothing the map can bind is a key the editor
    /// answers in the same context, and every reserved key really is refused —
    /// with every modifier, because those handlers don't test them.
    #[test]
    fn no_bindable_chord_can_shadow_an_editor_key() {
        let keys = MapKeys::default();
        for key in known_keys() {
            let Some(what) = reserved(key) else { continue };
            for shift in [false, true] {
                let chord = Chord { key, shift };
                assert!(
                    keys.command(chord.key, chord.shift).is_none(),
                    "{} is bound by the map but belongs to {what}",
                    chord.label()
                );
                let mut k = keys.clone();
                assert!(
                    k.set(MapCmd::Extrude, chord).is_err(),
                    "{} should be refused ({what})",
                    chord.label()
                );
            }
        }
    }

    /// A hand-edited file must not be able to create a conflict.
    #[test]
    fn a_broken_file_falls_back_instead_of_breaking_dispatch() {
        // Unknown command, unparseable chord, a reserved key, and a duplicate.
        let text = "nonsense G\nextrude ???\ninset KeyW\ndraw_box Q\nsubdivide E\n";
        let keys = MapKeys::decode(text);
        assert_eq!(keys.chord(MapCmd::Inset), Some(Chord::new(KeyCode::KeyI)), "reserved refused");
        assert_eq!(keys.chord(MapCmd::DrawBox), Some(Chord::new(KeyCode::KeyB)), "reserved refused");
        // `subdivide E` collides with extrude's default E: one of them keeps
        // it, the other is dropped, and no chord answers twice.
        let mut seen: Vec<Chord> = Vec::new();
        for cmd in MapCmd::ALL {
            if let Some(ch) = keys.chord(cmd) {
                assert!(!seen.contains(&ch), "{cmd:?} duplicates {}", ch.label());
                seen.push(ch);
            }
        }
    }
}
