//! The strips that float over the Scene view — the tool palette and the gizmo
//! bar — and the state that says where each one sits.
//!
//! ## They belong to the view, not to the window
//!
//! The gizmo bar used to be an `Area` with `.anchor(Align2::RIGHT_TOP, …)`,
//! which is the top-right of the **whole egui screen**. In a one-panel layout
//! that happens to look right; in any real dock layout it does not, and the bar
//! sat over the Inspector, over the tab strip, over whatever else owned that
//! corner — covering things that had nothing to do with the viewport. Every
//! position here is measured from the Scene tab's own rect, which `scene_ui`
//! already caches for picking, and `constrain_to` keeps a panel inside it even
//! while the dock is being resized.
//!
//! ## Nothing moves on its own
//!
//! A docked panel stays welded to its corner. A floating one keeps the exact
//! offset it was dropped at, in points from the view's top-left — not a
//! fraction of the view, which would slide the panel every time the dock
//! divider moved. Shrinking the view can push a panel against an edge (that is
//! `constrain_to` doing its job) but the stored offset is untouched, so growing
//! the view back puts it where it was.
//!
//! ## The chrome is one row, not a title bar
//!
//! Both panels are a single line of controls, and a title bar above them would
//! double the height of the thing whose whole complaint is that it covers the
//! scene. So the grip, the dock menu and the collapse button sit *in* the row,
//! and a collapsed panel shrinks to grip + name + reopen — a small tab parked
//! in its corner, out of the way but never hidden somewhere you have to
//! remember a menu to find.

use serde::{Deserialize, Serialize};

/// Where an overlay panel sits inside the Scene view.
///
/// Corners rather than edges: these panels are small strips, and "the left
/// side" of a viewport is a 20-pixel-tall thing somewhere down a 900-pixel
/// edge — a corner says the whole answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum Dock {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// Dragged somewhere by hand; `PanelState::offset` is the position.
    Free,
}

impl Dock {
    /// The four docks offered in the menu, in reading order.
    pub(crate) const CORNERS: [Dock; 4] =
        [Dock::TopLeft, Dock::TopRight, Dock::BottomLeft, Dock::BottomRight];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Dock::TopLeft => "Top left",
            Dock::TopRight => "Top right",
            Dock::BottomLeft => "Bottom left",
            Dock::BottomRight => "Bottom right",
            Dock::Free => "Floating",
        }
    }

    /// Which of the panel's own corners the anchor position refers to. Pinning
    /// by pivot is what lets a right-docked panel keep its right edge fixed as
    /// its contents change width, instead of growing off the side of the view.
    fn pivot(self) -> egui::Align2 {
        match self {
            Dock::TopLeft | Dock::Free => egui::Align2::LEFT_TOP,
            Dock::TopRight => egui::Align2::RIGHT_TOP,
            Dock::BottomLeft => egui::Align2::LEFT_BOTTOM,
            Dock::BottomRight => egui::Align2::RIGHT_BOTTOM,
        }
    }
}

/// The margin between a docked panel and the edge of the view.
const MARGIN: f32 = 8.0;

/// One overlay panel's placement. Per-user and persisted — where somebody
/// keeps their tool palette is a fact about them, not about a project.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PanelState {
    pub(crate) dock: Dock,
    /// A floating panel's top-left, in points from the Scene view's top-left.
    /// Ignored unless `dock` is [`Dock::Free`], and kept across a dock so that
    /// going back to Floating returns it where it was.
    pub(crate) offset: [f32; 2],
    /// Folded down to grip + name + reopen.
    pub(crate) collapsed: bool,
}

impl Default for PanelState {
    fn default() -> Self {
        PanelState { dock: Dock::TopLeft, offset: [MARGIN, MARGIN], collapsed: false }
    }
}

/// Every Scene-view overlay panel's placement, as one persisted unit.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ViewportPanels {
    /// The tool strip: select / move / rotate / scale / sculpt / …
    pub(crate) tools: PanelState,
    /// The gizmo master switch, its type filter, and the view lock.
    pub(crate) gizmos: PanelState,
}

impl Default for ViewportPanels {
    fn default() -> Self {
        ViewportPanels {
            tools: PanelState { dock: Dock::TopLeft, ..Default::default() },
            gizmos: PanelState { dock: Dock::TopRight, ..Default::default() },
        }
    }
}

/// Draw one overlay panel over `view` and return the rect it occupied.
///
/// `body` runs inside the row, between the grip and the dock/collapse buttons,
/// and is skipped entirely while the panel is collapsed. The returned rect is
/// the panel's real on-screen rect after constraining, which is what lets a
/// second overlay (the ▦ Model strip) stack under this one wherever it ends up.
pub(crate) fn show(
    ctx: &egui::Context,
    view: egui::Rect,
    id: &str,
    name: &str,
    st: &mut PanelState,
    body: impl FnOnce(&mut egui::Ui),
) -> egui::Rect {
    let pos = match st.dock {
        Dock::Free => view.min + egui::vec2(st.offset[0], st.offset[1]),
        Dock::TopLeft => view.left_top() + egui::vec2(MARGIN, MARGIN),
        Dock::TopRight => view.right_top() + egui::vec2(-MARGIN, MARGIN),
        Dock::BottomLeft => view.left_bottom() + egui::vec2(MARGIN, -MARGIN),
        Dock::BottomRight => view.right_bottom() + egui::vec2(-MARGIN, -MARGIN),
    };

    let mut drag = egui::Vec2::ZERO;
    let mut set_dock: Option<Dock> = None;
    let mut toggle_collapse = false;
    let collapsed = st.collapsed;
    let dock = st.dock;

    let out = egui::Area::new(egui::Id::new(id))
        .fixed_pos(pos)
        .pivot(dock.pivot())
        .order(egui::Order::Middle)
        .constrain_to(view)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    // The grip. A Label rather than a Button: it is a surface to
                    // grab, and a button that does nothing when clicked reads as
                    // broken. The dock menu is on its right-click, and the hover
                    // text says so — a context menu nobody is told about is a
                    // context menu nobody finds.
                    let grip = ui
                        .add(
                            egui::Label::new(egui::RichText::new("≣").weak())
                                .sense(egui::Sense::drag()),
                        )
                        .on_hover_cursor(egui::CursorIcon::Grab)
                        .on_hover_text(format!(
                            "{name} — drag to move it anywhere in the view.\nRight-click for \
                             the corners it can dock to.",
                        ));
                    if grip.dragged() {
                        drag = grip.drag_delta();
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                    }
                    grip.context_menu(|ui| {
                        ui.label(egui::RichText::new(name).small().strong());
                        ui.separator();
                        for d in Dock::CORNERS {
                            if ui.selectable_label(dock == d, d.label()).clicked() {
                                set_dock = Some(d);
                                ui.close();
                            }
                        }
                        ui.separator();
                        ui.add_enabled_ui(dock != Dock::Free, |ui| {
                            if ui
                                .button(Dock::Free.label())
                                .on_hover_text("put it back where you last dragged it")
                                .clicked()
                            {
                                set_dock = Some(Dock::Free);
                                ui.close();
                            }
                        });
                    });

                    if collapsed {
                        ui.label(egui::RichText::new(name).small().weak());
                    } else {
                        body(ui);
                        ui.separator();
                        ui.menu_button("▾", |ui| {
                            ui.label(egui::RichText::new(name).small().strong());
                            ui.separator();
                            for d in Dock::CORNERS {
                                if ui.selectable_label(dock == d, d.label()).clicked() {
                                    set_dock = Some(d);
                                    ui.close();
                                }
                            }
                            ui.separator();
                            ui.small("Drag the ≣ grip to float it anywhere instead.");
                        })
                        .response
                        .on_hover_text("dock this panel to a corner of the Scene view");
                    }

                    // Collapse, in the ▦ Model strip's own idiom: ⏶ folds away,
                    // ⏷ opens back up.
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new(if collapsed {
                                "⏷"
                            } else {
                                "⏶"
                            }))
                            .frame(false),
                        )
                        .on_hover_text(if collapsed {
                            "show this panel again"
                        } else {
                            "fold this panel down to a tab, so it stops covering the scene"
                        })
                        .clicked()
                    {
                        toggle_collapse = true;
                    }
                });
            });
        });

    let rect = out.response.rect;

    // Applied after the draw: the closure holds `&mut` borrows of half the
    // editor, so the state it changes is collected and written here.
    if drag != egui::Vec2::ZERO {
        // Measured from where the panel ACTUALLY drew, not from where it was
        // asked to draw, so a drag that runs into an edge picks up again from
        // the edge instead of from an off-screen position it never had.
        let base = rect.min - view.min + drag;
        st.offset = [
            base.x.clamp(0.0, (view.width() - 48.0).max(0.0)),
            base.y.clamp(0.0, (view.height() - 24.0).max(0.0)),
        ];
        st.dock = Dock::Free;
    }
    if let Some(d) = set_dock {
        st.dock = d;
    }
    if toggle_collapse {
        st.collapsed = !st.collapsed;
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draw a panel over `view` for two frames (the first has no cached size
    /// for the pivot to work from) and return where it landed.
    fn drawn(view: egui::Rect, st: &mut PanelState) -> egui::Rect {
        let ctx = crate::icons::test_context();
        let mut rect = egui::Rect::NOTHING;
        for _ in 0..2 {
            let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
                rect = show(ui.ctx(), view, "test_panel", "Test", st, |ui| {
                    ui.label("body");
                });
            });
        }
        rect
    }

    /// The bug this module exists for: a panel docked top-right belongs to the
    /// top-right of the VIEW. Anchored to the window it lands in the far corner
    /// of the screen, over whatever panel owns that corner.
    #[test]
    fn a_docked_panel_sits_in_the_view_not_in_the_window() {
        let view = egui::Rect::from_min_size(egui::pos2(300.0, 180.0), egui::vec2(420.0, 320.0));
        for dock in Dock::CORNERS {
            let mut st = PanelState { dock, ..Default::default() };
            let rect = drawn(view, &mut st);
            assert!(
                view.contains_rect(rect),
                "{dock:?} drew at {rect:?}, outside the view {view:?}",
            );
        }
    }

    /// Each corner is actually the corner it says — top-right is right of
    /// centre and above it, and so on round.
    #[test]
    fn each_corner_is_the_corner_it_names() {
        let view = egui::Rect::from_min_size(egui::pos2(300.0, 180.0), egui::vec2(420.0, 320.0));
        let c = view.center();
        for (dock, right, below) in [
            (Dock::TopLeft, false, false),
            (Dock::TopRight, true, false),
            (Dock::BottomLeft, false, true),
            (Dock::BottomRight, true, true),
        ] {
            let mut st = PanelState { dock, ..Default::default() };
            let r = drawn(view, &mut st);
            assert_eq!(r.center().x > c.x, right, "{dock:?} horizontal");
            assert_eq!(r.center().y > c.y, below, "{dock:?} vertical");
        }
    }

    /// A collapsed panel still draws — it is a tab you can click, not a hidden
    /// thing — and it is narrower than the open one.
    #[test]
    fn collapsing_leaves_a_tab_behind() {
        let view = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, 400.0));
        let mut open = PanelState::default();
        let open_rect = drawn(view, &mut open);
        let mut shut = PanelState { collapsed: true, ..Default::default() };
        let shut_rect = drawn(view, &mut shut);
        assert!(shut_rect.width() > 0.0, "a collapsed panel must still be clickable");
        assert!(
            shut_rect.width() < open_rect.width(),
            "collapsed {shut_rect:?} is no smaller than open {open_rect:?}",
        );
    }

    /// A floating panel is measured from the view's own corner, so moving the
    /// view (a dock divider drag) carries the panel with it rather than leaving
    /// it behind over some other tab.
    #[test]
    fn a_floating_panel_is_measured_from_the_views_corner() {
        let mut st = PanelState { dock: Dock::Free, offset: [40.0, 30.0], ..Default::default() };
        let a = drawn(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, 400.0)), &mut st);
        let b = drawn(egui::Rect::from_min_size(egui::pos2(120.0, 60.0), egui::vec2(600.0, 400.0)), &mut st);
        assert_eq!(b.min - a.min, egui::vec2(120.0, 60.0));
        assert_eq!(st.offset, [40.0, 30.0], "drawing must not move a panel by itself");
    }

    /// Old settings files must not reset a panel somebody placed — the whole
    /// point of persisting it is that it survives.
    #[test]
    fn panel_placement_round_trips_and_older_files_keep_what_they_say() {
        let p = ViewportPanels {
            tools: PanelState { dock: Dock::Free, offset: [12.0, 300.0], collapsed: true },
            gizmos: PanelState { dock: Dock::BottomRight, ..Default::default() },
        };
        let s = ron::ser::to_string_pretty(&p, ron::ser::PrettyConfig::default()).unwrap();
        assert_eq!(ron::from_str::<ViewportPanels>(&s).unwrap(), p);

        let partial: ViewportPanels = ron::from_str("(tools: (collapsed: true))").unwrap();
        assert!(partial.tools.collapsed);
        assert_eq!(partial.tools.dock, Dock::TopLeft);
        assert_eq!(partial.gizmos.dock, Dock::TopRight);
    }
}
