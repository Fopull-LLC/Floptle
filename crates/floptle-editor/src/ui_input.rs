//! Text fields, drag & drop, and tooltips at run time (docs/ui-system-2-proposal.md §D).
//!
//! The three of them share one idea: **the engine runs the mechanism and the
//! game owns the look.** A field has a caret but no border of its own; a drag
//! reports where it is but moves nothing; a tooltip is one of your elements
//! that the engine points at the right thing at the right moment.
//!
//! The pure halves live elsewhere — `floptle_ui::field` does the string
//! surgery, `floptle_ui::nav` does the geometry — because both are testable
//! without a window and this file is not.

use floptle_core::{Entity, Transform};
use floptle_ui::field::{self, Cursor, Edit};
use floptle_ui::{ElementSpec, Placed};
use winit::keyboard::KeyCode;

use crate::Editor;

/// One text-editing keystroke, decoded from a physical key at the moment it
/// arrived (so auto-repeat is preserved) and applied later, in the interact
/// pass, where the focused field is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextOp {
    /// A value edit or caret move; the bool is shift (extend the selection).
    Edit(Edit, bool),
    /// Put the selection on the OS clipboard.
    Copy,
    /// …and remove it.
    Cut,
}

/// A drag in flight.
///
/// The engine tracks it and reports it; it does not move the dragged element
/// and does not draw a ghost. A card that tilts, an item that snaps to a grid
/// and a wire that stretches out of its socket are all "drag", and none of
/// them is a translated copy of the source — so the source stays exactly where
/// the layout put it and `dragMove` tells your script where the pointer is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct UiDrag {
    /// The `draggable` element that was grabbed.
    pub source: u32,
    /// Where the press landed, in the layer's design units.
    pub from: [f32; 2],
    /// The drop target under the pointer right now, if any.
    pub over: Option<u32>,
    /// Whether the pointer has moved far enough to count as a drag. A click
    /// and a drag start identically, and a button that fired `dragStart`
    /// every time you pressed it would be unusable.
    pub started: bool,
}

/// How far the pointer must travel (design units) before a press becomes a drag.
const DRAG_SLOP: f32 = 5.0;

/// What one frame of a drag produced: hooks to fire, and the `(source, target)`
/// pair `ui.dragging()` / `ui.dropTarget()` should report.
type DragFrame = (Vec<(u32, &'static str)>, Option<(u32, Option<u32>)>);

/// Advance an armed drag one frame: the whole state machine, with no world.
///
/// Returns `(hooks to fire, what ui.dragging() should report)`. Pulled out of
/// the `Editor` because the interesting parts — the slop, "a drop on nothing is
/// still a drop", enter/leave pairing — are pure and are exactly the parts that
/// would otherwise only be testable by driving a window.
fn drag_step(
    drag: &mut Option<UiDrag>,
    drop_over: Option<u32>,
    pointer: Option<[f32; 2]>,
    down: bool,
) -> DragFrame {
    let mut ev: Vec<(u32, &'static str)> = Vec::new();
    if !down {
        let Some(d) = drag.take() else { return (ev, None) };
        if !d.started {
            // Never travelled: this was a click, and a click has already been
            // reported by the ordinary press/release path.
            return (ev, None);
        }
        match d.over {
            Some(t) => {
                ev.push((t, "dropped"));
                ev.push((d.source, "dropped"));
            }
            // A drop outside every target is STILL a drop as far as the source
            // is concerned: it has to hear something, or a half-finished
            // gesture leaves an item stuck to the cursor forever.
            None => ev.push((d.source, "dragCancel")),
        }
        // The pair stays readable for exactly this frame, so the `dropped`
        // hooks about to run can ask `ui.dragging()` what they were handed.
        return (ev, Some((d.source, d.over)));
    }
    let (Some(mut d), Some(p)) = (*drag, pointer) else { return (ev, None) };
    if !d.started {
        if (p[0] - d.from[0]).abs().max((p[1] - d.from[1]).abs()) < DRAG_SLOP {
            return (ev, None);
        }
        d.started = true;
        ev.push((d.source, "dragStart"));
    }
    // Dropping a thing on itself is a cancel, not a drop.
    let over = drop_over.filter(|t| *t != d.source);
    if over != d.over {
        if let Some(old) = d.over {
            ev.push((old, "dragLeave"));
        }
        if let Some(new) = over {
            ev.push((new, "dragEnter"));
        }
        d.over = over;
    }
    ev.push((d.source, "dragMove"));
    if let Some(t) = over {
        ev.push((t, "dragOver"));
    }
    *drag = Some(d);
    (ev, Some((d.source, d.over)))
}

impl Editor {
    /// Decode one physical key into a text-field operation, if it is one.
    ///
    /// Called from the window-event handler rather than sampled per frame so
    /// that **auto-repeat works**: holding backspace has to keep deleting, and
    /// the OS is the only thing that knows this machine's repeat rate.
    pub(crate) fn note_ui_text_key(&mut self, code: KeyCode) {
        let (shift, ctrl) = (self.shift, self.ctrl);
        let op = match code {
            KeyCode::ArrowLeft if ctrl => TextOp::Edit(Edit::WordLeft, shift),
            KeyCode::ArrowRight if ctrl => TextOp::Edit(Edit::WordRight, shift),
            KeyCode::ArrowLeft => TextOp::Edit(Edit::Left, shift),
            KeyCode::ArrowRight => TextOp::Edit(Edit::Right, shift),
            KeyCode::Home => TextOp::Edit(Edit::Home, shift),
            KeyCode::End => TextOp::Edit(Edit::End, shift),
            KeyCode::Backspace if ctrl => TextOp::Edit(Edit::BackspaceWord, false),
            KeyCode::Backspace => TextOp::Edit(Edit::Backspace, false),
            KeyCode::Delete => TextOp::Edit(Edit::Delete, false),
            KeyCode::KeyA if ctrl => TextOp::Edit(Edit::SelectAll, false),
            KeyCode::KeyC if ctrl => TextOp::Copy,
            KeyCode::KeyX if ctrl => TextOp::Cut,
            _ => return,
        };
        self.ui_text_ops.push(op);
    }

    /// True while a text field owns the keyboard — the navigation pass asks so
    /// it can leave the horizontal directions alone.
    pub(crate) fn ui_field_focused(&self) -> bool {
        self.ui_edit.is_some()
    }

    /// The `field` spec of an element, if it is one.
    pub(crate) fn ui_field_of(&self, id: u32) -> Option<floptle_ui::FieldSpec> {
        let e = self.ui_entity(id)?;
        self.world.get::<ElementSpec>(e)?.field.clone()
    }

    /// Resolve an element id back to its entity.
    pub(crate) fn ui_entity(&self, id: u32) -> Option<Entity> {
        self.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == id)
    }

    /// Start or stop editing, following focus.
    ///
    /// Focus and text entry are deliberately the same thing: a caret blinking
    /// in a box that doesn't have the ring is exactly how "my typing went
    /// somewhere else" happens.
    pub(crate) fn ui_sync_edit(&mut self) {
        let want = self.ui_focus.filter(|id| self.ui_field_of(*id).is_some());
        match (self.ui_edit.map(|e| e.id), want) {
            (a, b) if a == b => {}
            (_, Some(id)) => {
                // Entering a field selects nothing and parks the caret at the
                // end, which is where you want it when you tab into a value
                // you mean to extend.
                let n = self.ui_value(id).chars().count();
                self.ui_edit = Some(floptle_ui::EditState { id, caret: n, anchor: n, on: true });
                self.ui_caret_t = 0.0;
            }
            (_, None) => self.ui_edit = None,
        }
    }

    /// The current value of a field (its element's own text).
    fn ui_value(&self, id: u32) -> String {
        self.ui_entity(id)
            .and_then(|e| self.world.get::<ElementSpec>(e))
            .and_then(|s| s.text.as_ref())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    fn ui_set_value(&mut self, id: u32, value: String) {
        if let Some(e) = self.ui_entity(id)
            && let Some(spec) = self.world.get_mut::<ElementSpec>(e)
            && let Some(t) = &mut spec.text
        {
            t.text = value;
        }
    }

    /// Apply this frame's typed characters and editing keys to the focused
    /// field. Fires `changed` once, however many keystrokes landed.
    pub(crate) fn ui_edit_text(&mut self, dt: f32) {
        self.ui_sync_edit();
        let Some(mut state) = self.ui_edit else {
            // Nothing focused: the editing keys are dropped rather than banked
            // (clicking into a field would otherwise replay everything typed at
            // the menu before it), and `input_typed` is LEFT ALONE so it
            // reaches `input.typed()`. A focused field consumes typing; without
            // one, the game gets it. That is the whole suppression rule.
            self.ui_text_ops.clear();
            return;
        };
        let Some(spec) = self.ui_field_of(state.id) else { return };
        let ops = std::mem::take(&mut self.ui_text_ops);
        let typed = std::mem::take(&mut self.input_typed);
        // …and out of the per-tick accumulator too, or `fixedUpdate` would
        // receive the characters the field just ate.
        self.tick_typed.clear();
        let mut value = self.ui_value(state.id);
        let mut cur = Cursor { caret: state.caret, anchor: state.anchor };
        let before = value.clone();
        let mut acted = false;
        // Characters first: they arrived before the keys that follow them in a
        // frame only in the sense that both are "this frame", and putting text
        // first means a typed-then-backspaced pair behaves like typing.
        if !typed.is_empty() {
            field::apply(&mut value, &mut cur, &Edit::Insert(typed), false, &spec);
            acted = true;
        }
        for op in ops {
            acted = true;
            match op {
                TextOp::Edit(e, extend) => {
                    field::apply(&mut value, &mut cur, &e, extend, &spec);
                }
                TextOp::Copy | TextOp::Cut => {
                    let (a, b) = field::selection(cur.caret, cur.anchor);
                    // A masked field refuses both: a password that fills the
                    // clipboard is a bug, not a convenience.
                    if a != b && !spec.mask {
                        let text: String = value.chars().skip(a).take(b - a).collect();
                        self.ensure_os_clipboard();
                        if let Some(c) = self.os_clipboard.as_mut() {
                            c.set_text(text);
                        }
                        if op == TextOp::Cut {
                            field::apply(&mut value, &mut cur, &Edit::Delete, false, &spec);
                        }
                    }
                }
            }
        }
        // The caret is solid while you type and only blinks once you stop —
        // a caret that vanishes mid-word reads as a dropped keystroke.
        if acted {
            self.ui_caret_t = 0.0;
        } else {
            self.ui_caret_t += dt;
        }
        state.caret = cur.caret;
        state.anchor = cur.anchor;
        state.on = (self.ui_caret_t * 1.6) as i32 % 2 == 0;
        self.ui_edit = Some(state);
        if value != before {
            self.ui_set_value(state.id, value);
            self.ui_events.push((state.id, "changed"));
        }
    }

    /// Place the caret from a click, and set the anchor for a drag-select.
    ///
    /// `x` is the pointer's x in the layer's design units. Measuring prefixes
    /// through the renderer's own font is the only way to get this right; a
    /// character-width guess is off by a whole glyph on the first proportional
    /// font anybody loads.
    pub(crate) fn ui_caret_at(&mut self, id: u32, rect: [f32; 4], x: f32, extend: bool) {
        let Some(uir) = self.ui_render.as_ref() else { return };
        let Some(e) = self.ui_entity(id) else { return };
        let Some(spec) = self.world.get::<ElementSpec>(e) else { return };
        let (Some(t), Some(f)) = (spec.text.as_ref(), spec.field.as_ref()) else { return };
        let shown: String = if f.mask {
            std::iter::repeat_n(f.mask_char, t.text.chars().count()).collect()
        } else {
            t.text.clone()
        };
        let probe = std::cell::RefCell::new(t.clone());
        let width = |s: &str| {
            let mut p = probe.borrow_mut();
            p.text = s.to_string();
            uir.measure_spec(&p)[0] + t.tracking * s.chars().count() as f32
        };
        let full = width(&shown);
        let left = match t.align {
            floptle_ui::Align::Start | floptle_ui::Align::Stretch => rect[0],
            floptle_ui::Align::Center => rect[0] + (rect[2] - full) * 0.5,
            floptle_ui::Align::End => rect[0] + rect[2] - full,
        };
        // The mapping — including undoing the scroll the renderer drew with —
        // lives in the kernel, where it is tested against a fixed-width font
        // and shares `scroll_shift` with the renderer that applied it. The
        // caret that drew the frame we're clicking on is what decides how far
        // the run had slid; an unfocused field has no caret and hasn't slid.
        let drawn_caret = self.ui_edit.filter(|s| s.id == id).map(|s| s.caret);
        let caret = floptle_ui::field::caret_at(&shown, left, rect, drawn_caret, x, 2.0, &width);
        let anchor = if extend { self.ui_edit.map(|e| e.anchor).unwrap_or(caret) } else { caret };
        self.ui_edit = Some(floptle_ui::EditState { id, caret, anchor, on: true });
        self.ui_caret_t = 0.0;
    }

    // -----------------------------------------------------------------------
    // Drag and drop
    // -----------------------------------------------------------------------

    /// Advance a drag. `items` is this frame's interactive elements with the
    /// pointer already in their layer's design units.
    ///
    /// Hooks fire on BOTH ends, because both ends have something to say: the
    /// source knows what is being carried, the target knows whether it will
    /// take it. `dropped` reaches the target with the source's name, and the
    /// source hears `dropped` too so it can remove the item it gave away.
    pub(crate) fn ui_drag_step(
        &mut self,
        drop_over: Option<u32>,
        hover: Option<u32>,
        pointer: Option<[f32; 2]>,
        pressed_edge: bool,
        down: bool,
    ) {
        // Start: a press on a draggable element arms a drag, but doesn't begin
        // one until the pointer actually travels.
        if pressed_edge
            && let Some(h) = hover
            && let Some(p) = pointer
            && self.ui_flag(h, |s| s.draggable)
        {
            self.ui_drag = Some(UiDrag { source: h, from: p, over: None, started: false });
        }
        let (events, report) = drag_step(&mut self.ui_drag, drop_over, pointer, down);
        self.ui_events.extend(events);
        self.ui_drag_report = report;
    }

    /// Read one boolean off an element's spec.
    fn ui_flag(&self, id: u32, f: impl Fn(&ElementSpec) -> bool) -> bool {
        self.ui_entity(id).and_then(|e| self.world.get::<ElementSpec>(e)).is_some_and(f)
    }

    // -----------------------------------------------------------------------
    // Tooltips
    // -----------------------------------------------------------------------

    /// Show, hide and position the layer's tooltip element.
    ///
    /// The engine writes text into one of YOUR elements and moves it. It draws
    /// nothing: a tooltip's look is a panel, a shadow, a font and a delay, and
    /// four of those five are already yours.
    pub(crate) fn ui_tooltips(
        &mut self,
        layer: Entity,
        hover: Option<u32>,
        pointer: Option<[f32; 2]>,
        design_vp: [f32; 2],
        placed: &[Placed],
        delay: f32,
    ) {
        let Some(tip_box) = self.ui_tooltip_box(layer) else { return };
        // The element the pointer is resting on, how long it has rested, and
        // what it wants to say.
        let held = self.ui_tip_hover.filter(|(h, _)| Some(*h) == hover).map(|(_, t)| t);
        let tip = hover.and_then(|h| {
            let e = self.ui_entity(h)?;
            let s = self.world.get::<ElementSpec>(e)?;
            (!s.tooltip.is_empty()).then(|| s.tooltip.clone())
        });
        // A drag in flight suppresses tooltips: a label following the cursor
        // while you are already carrying something is noise on top of noise.
        let show = tip.filter(|_| held.is_some_and(|t| t >= delay) && self.ui_drag.is_none());
        if let Some(spec) = self.world.get_mut::<ElementSpec>(tip_box) {
            spec.visible = show.is_some();
        }
        let Some(text) = show else { return };
        // Write into the box's own text, or its first labelled descendant — so
        // a panel with a label inside works with no wiring.
        if let Some(label) = self.ui_first_text(tip_box)
            && let Some(spec) = self.world.get_mut::<ElementSpec>(label)
            && let Some(t) = &mut spec.text
            && t.text != text
        {
            t.text = text;
        }
        // Follow the pointer, nudged clear of it, and kept inside the canvas
        // so a tooltip near the right edge doesn't hang off it.
        if let Some(p) = pointer
            && let Some(rect) = placed.iter().find(|pl| pl.id == tip_box.index()).map(|pl| pl.rect)
            && let Some(spec) = self.world.get_mut::<ElementSpec>(tip_box)
        {
            let x = (p[0] + 16.0).min(design_vp[0] - rect[2] - 4.0).max(4.0);
            let y = (p[1] + 20.0).min(design_vp[1] - rect[3] - 4.0).max(4.0);
            crate::ui_game::set_place(&mut spec.place, [x, y]);
        }
    }

    // -----------------------------------------------------------------------
    // Repeaters
    // -----------------------------------------------------------------------

    /// Keep every repeater's children matching its `count`.
    ///
    /// Spawns and destroys only the DIFFERENCE. A list that gains one row
    /// keeps the other nine, with their scripts' state, their hover, their
    /// in-flight style transitions and their scroll position — rebuilding the
    /// lot every frame is what makes a hand-rolled list flicker and forget.
    ///
    /// Play only. The rows are runtime entities; conjuring them in edit mode
    /// would put engine-spawned nodes in a scene you are about to save. The
    /// cost is that the ◫ UI tab shows an empty container — put one row in the
    /// scene by hand to design against, and let the repeater fill the rest.
    pub(crate) fn ui_repeaters(&mut self) {
        if !self.playing {
            return;
        }
        // (container, template, wanted, current rows in index order)
        let mut work: Vec<(Entity, String, u32, Vec<Entity>)> = Vec::new();
        let mut rows_of: std::collections::HashMap<u32, Vec<(u32, Entity)>> = Default::default();
        for (e, idx) in self.world.query::<floptle_core::RepeatIndex>() {
            if let Some(p) = self.world.get::<floptle_core::Parent>(e) {
                rows_of.entry(p.0.index()).or_default().push((idx.0, e));
            }
        }
        for (e, spec) in self.world.query::<ElementSpec>() {
            let Some(r) = spec.repeater.as_ref() else { continue };
            if r.template.is_empty() {
                continue;
            }
            let mut rows = rows_of.remove(&e.index()).unwrap_or_default();
            rows.sort_by_key(|(i, _)| *i);
            work.push((e, r.template.clone(), r.count, rows.into_iter().map(|(_, e)| e).collect()));
        }
        let mut spawns: Vec<floptle_script::SpawnRequest> = Vec::new();
        let mut destroys: Vec<u32> = Vec::new();
        for (container, template, want, rows) in &work {
            let have = rows.len() as u32;
            for extra in rows.iter().skip(*want as usize) {
                destroys.push(extra.index());
            }
            for _ in have..*want {
                spawns.push(floptle_script::SpawnRequest {
                    prefab: template.clone(),
                    // No position: the container's layout places the row, and
                    // a world position on a UI element would be meaningless.
                    pos: None,
                    cb: None,
                    parent: Some(container.index()),
                });
            }
        }
        if !destroys.is_empty() {
            self.apply_destroys(destroys);
        }
        if !spawns.is_empty() {
            self.apply_spawn_batch(spawns, Vec::new());
        }
        // Re-number every surviving row in flow order, so `node.index` is
        // right the frame a row is added or removed rather than the frame
        // after — a list that renumbers late shows one wrong label per edit.
        self.ui_renumber_rows();
    }

    /// Stamp `RepeatIndex` on every repeater's children in scene order.
    fn ui_renumber_rows(&mut self) {
        let containers: Vec<Entity> = self
            .world
            .query::<ElementSpec>()
            .filter(|(_, s)| s.repeater.is_some())
            .map(|(e, _)| e)
            .collect();
        for c in containers {
            let kids: Vec<Entity> = self
                .world
                .query::<floptle_core::Parent>()
                .filter(|(_, p)| p.0 == c)
                .map(|(e, _)| e)
                .collect();
            for (i, k) in kids.into_iter().enumerate() {
                self.world.insert(k, floptle_core::RepeatIndex(i as u32));
            }
        }
    }

    /// Advance the hover dwell timer. Once per frame, after every layer's
    /// tooltip has been placed from it — a timer reset inside the per-layer
    /// loop would run at N times the frame rate on a screen with N layers.
    pub(crate) fn ui_tick_tooltip_timer(&mut self, hover: Option<u32>, dt: f32) {
        self.ui_tip_hover = match (self.ui_tip_hover, hover) {
            (Some((prev, t)), Some(h)) if prev == h => Some((h, t + dt)),
            (_, Some(h)) => Some((h, 0.0)),
            (_, None) => None,
        };
    }

    /// The `tooltip_box` element inside a layer, if it has one.
    fn ui_tooltip_box(&self, layer: Entity) -> Option<Entity> {
        self.ui_descendants(layer)
            .into_iter()
            .find(|e| self.world.get::<ElementSpec>(*e).is_some_and(|s| s.tooltip_box))
    }

    /// The first element in this subtree (itself included) that has text.
    fn ui_first_text(&self, root: Entity) -> Option<Entity> {
        std::iter::once(root)
            .chain(self.ui_descendants(root))
            .find(|e| self.world.get::<ElementSpec>(*e).is_some_and(|s| s.text.is_some()))
    }

    /// Every descendant entity of `root`, breadth-first.
    fn ui_descendants(&self, root: Entity) -> Vec<Entity> {
        let mut out = Vec::new();
        let mut frontier = vec![root];
        while let Some(cur) = frontier.pop() {
            for (e, p) in self.world.query::<floptle_core::Parent>() {
                if p.0 == cur {
                    out.push(e);
                    frontier.push(e);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(source: u32) -> Option<UiDrag> {
        Some(UiDrag { source, from: [100.0, 100.0], over: None, started: false })
    }

    #[test]
    fn a_press_that_does_not_travel_is_a_click_not_a_drag() {
        // A button that fired `dragStart` on every press would be unusable,
        // and the two gestures begin identically.
        let mut d = armed(1);
        let (ev, _) = drag_step(&mut d, None, Some([102.0, 101.0]), true);
        assert!(ev.is_empty(), "no hooks inside the slop, got {ev:?}");
        let (ev, report) = drag_step(&mut d, None, Some([102.0, 101.0]), false);
        assert!(ev.is_empty(), "…and releasing inside it reports nothing either");
        assert_eq!(report, None);
        assert!(d.is_none());
    }

    #[test]
    fn travelling_starts_the_drag_exactly_once() {
        let mut d = armed(1);
        let (ev, _) = drag_step(&mut d, None, Some([140.0, 100.0]), true);
        assert_eq!(ev, vec![(1, "dragStart"), (1, "dragMove")]);
        let (ev, _) = drag_step(&mut d, None, Some([160.0, 100.0]), true);
        assert_eq!(ev, vec![(1, "dragMove")], "dragStart does not repeat");
    }

    #[test]
    fn entering_and_leaving_a_target_pair_up() {
        let mut d = armed(1);
        drag_step(&mut d, None, Some([140.0, 100.0]), true);
        let (ev, _) = drag_step(&mut d, Some(7), Some([150.0, 100.0]), true);
        assert_eq!(ev, vec![(7, "dragEnter"), (1, "dragMove"), (7, "dragOver")]);
        let (ev, _) = drag_step(&mut d, Some(9), Some([160.0, 100.0]), true);
        assert_eq!(
            ev,
            vec![(7, "dragLeave"), (9, "dragEnter"), (1, "dragMove"), (9, "dragOver")],
            "moving straight from one slot to the next leaves the first"
        );
        let (ev, _) = drag_step(&mut d, None, Some([400.0, 400.0]), true);
        assert_eq!(ev, vec![(9, "dragLeave"), (1, "dragMove")]);
    }

    #[test]
    fn a_drop_reaches_both_ends() {
        // The target needs to know it received something; the source needs to
        // know it gave something away. One hook name, two listeners.
        let mut d = armed(1);
        drag_step(&mut d, Some(7), Some([140.0, 100.0]), true);
        let (ev, report) = drag_step(&mut d, Some(7), Some([140.0, 100.0]), false);
        assert_eq!(ev, vec![(7, "dropped"), (1, "dropped")]);
        assert_eq!(report, Some((1, Some(7))), "ui.dragging() still answers during `dropped`");
    }

    #[test]
    fn a_drop_on_nothing_still_tells_the_source() {
        let mut d = armed(1);
        drag_step(&mut d, None, Some([140.0, 100.0]), true);
        let (ev, report) = drag_step(&mut d, None, None, false);
        assert_eq!(ev, vec![(1, "dragCancel")]);
        assert_eq!(report, Some((1, None)));
    }

    #[test]
    fn dropping_a_thing_on_itself_is_a_cancel() {
        let mut d = armed(4);
        drag_step(&mut d, Some(4), Some([140.0, 100.0]), true);
        let (ev, _) = drag_step(&mut d, Some(4), Some([140.0, 100.0]), false);
        assert_eq!(ev, vec![(4, "dragCancel")]);
    }
}
