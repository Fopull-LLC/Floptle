//! **Nothing goes off the edge.**
//!
//! A dock panel is whatever width the user dragged it to, and the answer to a
//! narrow one is never "the right-hand half of the controls is somewhere past
//! the border". A panel that is too thin should get *smaller* controls, then
//! *wrapped* controls, then *stacked* ones — and only ever stop shrinking when
//! a chip can no longer hold a glyph. It must never stop being fully visible.
//!
//! The panels this module replaced were laid out with fixed widths inside a
//! non-wrapping [`egui::Ui::horizontal`]: a 58 px label column plus four 74 px
//! chips is 350 px of row that a 240 px panel simply cannot show. egui does not
//! complain about that — the row is *drawn*, at the width it asked for, and the
//! part past the clip rect is thrown away by the renderer. Nothing in the frame
//! says so, which is why this shipped.
//!
//! ## The three behaviours, in the order they kick in
//!
//! 1. **Shrink.** Equal-width chips fill the row exactly instead of holding a
//!    fixed width, down to [`MIN_CHIP_W`].
//! 2. **Wrap.** Below that, the strip breaks onto as many lines as it needs,
//!    with the chips on every line still equal-width — [`columns`] is the whole
//!    decision and it is a pure function, so it is tested directly.
//! 3. **Stack.** Below [`MIN_CONTENT_W`] of usable room the label column costs
//!    more than it aligns, so [`row`] moves the caption onto its own line and
//!    hands the controls the full width.
//!
//! A panel narrower than one chip still gets that chip, clipped to the panel:
//! the floor is a floor on *layout*, not a minimum size we impose on the user.
//! Dragging a panel down to a sliver is allowed, and it degrades rather than
//! breaking.
//!
//! ## Why the guard is a shape test
//!
//! [`tests::overflow`] runs a panel headlessly at a ladder of widths and walks
//! the frame's [`egui::epaint::ClippedShape`]s, flagging any whose bounding box
//! crosses its own clip rect horizontally. That is exactly the condition "the
//! user cannot see this", stated once, in terms of what actually reached the
//! renderer — so it catches a fixed width, an unwrapped row and an over-long
//! label with one assertion and needs no per-widget bookkeeping.

use egui::{Label, RichText, Ui, Vec2};

/// The measurements the form panels are built on, shared so the ▦ Model tab and
/// the ◫ Tiles tab cannot drift into looking like two different programs.
pub(crate) const LABEL_W: f32 = 58.0;
/// A chip's *natural* width — what it takes when there is room.
pub(crate) const CHIP_W: f32 = 74.0;
pub(crate) const BTN_H: f32 = 22.0;

/// Below this a chip is not a chip: it cannot hold a glyph and its padding, so
/// shrinking stops here and wrapping starts.
pub(crate) const MIN_CHIP_W: f32 = 30.0;

/// The usable width below which a left label column stops earning its place.
///
/// A caption and its controls sharing a 150 px row leaves the controls 90 px,
/// which is narrower than one drag field. Stacked, they get the whole 150.
pub(crate) const MIN_CONTENT_W: f32 = 96.0;

/// How many equal columns fit in `avail`, and how wide each one is.
///
/// The one decision behind every strip in the editor, factored out because it
/// is the part that is easy to get subtly wrong and easy to test exactly.
///
/// * Never wider than `natural` — two chips on a wide panel stay chip-sized
///   rather than stretching halfway across the editor.
/// * Never narrower than `min`, by taking fewer per line instead.
/// * If even a single column is below `min`, it still returns one column at the
///   full width available. A sliver of a panel gets a sliver of a chip; it does
///   not get a chip that reaches past the edge, and it is not refused.
///
/// The width comes back **floored to a whole pixel**. That is worth a line
/// because it is load-bearing rather than cosmetic: a row of chips summing to
/// exactly `avail` overhangs by a pixel in practice, since the width a `Ui`
/// reports and the width its clip rect ends up with disagree in the last
/// fraction. Flooring spends at most one pixel per column and cannot overrun.
pub(crate) fn columns(avail: f32, spacing: f32, want: usize, natural: f32, min: f32) -> (usize, f32) {
    let want = want.max(1);
    let each = |k: usize| (avail - spacing * k.saturating_sub(1) as f32) / k as f32;
    let per_row = (1..=want).rev().find(|&k| each(k) >= min).unwrap_or(1);
    (per_row, each(per_row).min(natural).floor().max(1.0))
}

/// The width still **visible** on this line: `available_width`, clamped to what
/// is left of the clip rect.
///
/// This is the load-bearing difference between a panel that degrades and one
/// that unravels. A `Ui`'s available width is derived from a content region
/// that **grows** when something over-wide is allocated in it — so the moment
/// one widget overflows, every widget after it is measured against a region
/// that is already off-screen, wraps at the wrong width, and overflows too. One
/// fixed-width button at the top of a tab is enough to push the rest of the tab
/// out with it, which is exactly the cascade the ◫ Tiles guard caught: sixteen
/// reported overflows, one cause.
///
/// The clip rect does not grow. It is the panel.
pub(crate) fn usable_width(ui: &Ui) -> f32 {
    ui.available_width().min(edge(ui) - ui.cursor().left()).max(1.0)
}

/// The right-hand edge nothing may be laid out past: the panel, or a region that
/// has deliberately been made narrower than it.
///
/// The clip rect alone is not enough. A [`group`] sets an explicit max width so
/// its frame can draw a border inside the panel — and the content inside it must
/// fit *that*, not the panel, or the border lands on top of it. Taking the
/// nearer of the two respects a deliberate constraint while still ignoring a
/// region that has merely grown, since a grown region's right edge is past the
/// clip and the clip wins.
fn edge(ui: &Ui) -> f32 {
    ui.clip_rect().right().min(ui.max_rect().right())
}

/// The most a fixed-width control may ask for and still be fully visible:
/// `want`, capped at the panel's own width measured from this region's left
/// edge.
///
/// Deliberately measured from the clip rect and **not** from what is left of the
/// current line: inside a wrapped layout the remainder of a line is not a
/// budget, it is a reason to wrap onto the next one. This only has to stop a
/// control that is wider than the panel itself.
pub(crate) fn fit(ui: &Ui, want: f32) -> f32 {
    want.min(edge(ui) - ui.max_rect().left() - 1.0).floor().max(1.0)
}

/// Shorten `text` with an ellipsis until it fits `width`.
///
/// The escape hatch for anything egui lays out with [`egui::TextWrapMode::Extend`]
/// and gives no way to override — an [`egui::CollapsingHeader`] title is the one
/// every tab has, and its wrap mode is hard-coded. Shortening the string is the
/// only lever left, so the disclosure that says "grass — 0 of 47 drawn" in a
/// wide dock says "grass — 0 of…" in a narrow one instead of running past the
/// border.
pub(crate) fn elide(ui: &Ui, text: &str, width: f32) -> String {
    if text_w(ui, text) <= width {
        return text.to_string();
    }
    let mut end = text.len();
    while end > 0 {
        end -= 1;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = format!("{}…", &text[..end]);
        if text_w(ui, &candidate) <= width {
            return candidate;
        }
    }
    "…".to_string()
}

/// A disclosure title that fits the panel — [`elide`] against the width left
/// after the arrow.
pub(crate) fn header_text(ui: &Ui, title: &str) -> String {
    // The triangle plus its padding, from egui's own collapsing-header layout.
    const ARROW: f32 = 22.0;
    elide(ui, title, (usable_width(ui) - ARROW).max(8.0))
}

/// Like [`fit`], but measured from where the control will actually start.
///
/// For a widget that does **not** take part in a wrapped layout's line
/// breaking, wrapping is not the fallback — shrinking is. [`egui::ComboBox`] is
/// the one that matters here: it sizes its own button rather than allocating
/// up front, so a wrapped row never gets the chance to move it to the next line
/// and it simply draws past the edge.
pub(crate) fn fit_here(ui: &Ui, want: f32) -> f32 {
    want.min(edge(ui) - ui.cursor().left() - 1.0).floor().max(1.0)
}

/// A checkbox whose label is laid out inside the panel.
///
/// [`egui::Checkbox`] offers no wrap control and takes its natural width, which
/// makes the widest one in a section a *grower*: it pushes the content region
/// out, and then every paragraph after it wraps against an edge that is off
/// screen. Clamping the width it is laid out in is the only lever the widget
/// leaves.
pub(crate) fn check(ui: &mut Ui, on: &mut bool, text: &str) -> egui::Response {
    let w = fit_here(ui, f32::INFINITY);
    // The tick and the gap egui puts between it and the label, so the elide
    // budget is the room the TEXT actually gets.
    const BOX: f32 = 26.0;
    let label = elide(ui, text, (w - BOX).max(8.0));
    ui.scope(|ui| {
        ui.set_max_width(w);
        ui.checkbox(on, label)
    })
    .inner
}

/// A slider that fits, value box and all.
///
/// [`egui::Slider`] draws its number box *after* the track and outside the size
/// it was allocated, so `add_sized` does not bound it — a slider given 119 px
/// draws 119 px of track and then puts a 40 px number past the edge. What
/// bounds it is `spacing.slider_width`, which measures the track alone.
pub(crate) fn slider(ui: &mut Ui, s: egui::Slider<'_>) -> egui::Response {
    /// Room for the number box egui appends, plus the gap before it.
    const VALUE_BOX: f32 = 62.0;

    // A slider CLAMPS to the width available rather than wrapping, which is the
    // detail that decides this. Squeezed onto the tail of a line it does not
    // move down; it draws a stub of a track and pushes its number box past the
    // panel.
    //
    // So when the rest of the line is too small, ask for a block of the whole
    // REGION's width. That is one allocation larger than what is left of the
    // line, which is what a wrapped layout moves to the next line — and there
    // the block gets the width it asked for.
    //
    // The obvious alternative — filling the rest of the line first — does not
    // work and was tried: the fill lands, the cursor ends at the line's end, and
    // the slider is then measured against ZERO remaining width. It took the
    // panel's region from 160 px to 239 px, and the give-away was that 200 px
    // and 120 px both passed while 160 px failed. A layout bug that is not
    // monotonic in width is a threshold being crossed, not a size being wrong.
    let line = fit_here(ui, f32::INFINITY);
    let w = if line >= MIN_CONTROL { line } else { fit(ui, f32::INFINITY) };
    // `allocate_ui` and not `scope`: a scope reserves nothing up front, so the
    // wrapped parent never learns how much room this wanted and never wraps it.
    // Reserving `w` is what makes the request visible — and a request larger
    // than the rest of the line is exactly what a wrapped layout answers by
    // moving to the next one.
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui(egui::vec2(w, h), |ui| {
        ui.spacing_mut().slider_width = (w - VALUE_BOX).max(24.0);
        ui.add(s)
    })
    .inner
}

/// The narrowest a property control can be and still be worth drawing beside a
/// caption. Set by the widest hard minimum among them — a slider, whose number
/// box will not go under about 40 px however short its track is.
pub(crate) const MIN_CONTROL: f32 = 110.0;

/// A grouped box that stays inside the panel.
///
/// [`egui::Frame::group`] draws its stroke and margins *around* the width its
/// content was given, so a group filling the panel reaches a few pixels past it.
/// That makes a group a **grower**, and a grower is never a local problem: the
/// region it widens is the region every right-aligned control after it aligns
/// to, so a `Layout::right_to_left` button inside one lines itself up against an
/// edge nobody can see.
pub(crate) fn group<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    /// Under this a border costs more than it groups. Thirteen pixels of margin
    /// and stroke out of a hundred-and-twenty is a tenth of the panel spent on
    /// saying "these belong together" — which a rule above them says just as
    /// well and for nothing.
    const BORDER_WORTH_IT: f32 = 160.0;

    let w = fit_here(ui, f32::INFINITY);
    if w < BORDER_WORTH_IT {
        ui.separator();
        return add(ui);
    }
    let frame = egui::Frame::group(ui.style());
    // Both margins, plus the stroke — which is centred on the edge it draws, so
    // half of it lies outside the rect on each side, i.e. one whole width.
    let pad = frame.inner_margin.sum().x + frame.outer_margin.sum().x + frame.stroke.width;
    ui.scope(|ui| {
        ui.set_max_width((w - pad).max(24.0));
        frame.show(ui, add).inner
    })
    .inner
}

/// A two-column property grid that stays inside the panel.
///
/// [`egui::Grid`] sizes its columns from their content and never wraps, so a
/// grid is a **grower** — and its `max_col_width` defaults to the `Ui`'s
/// available width, which is precisely the number that grows. Clamping the `Ui`
/// the grid is built in bounds both at once.
///
/// The column cap leaves room for a caption beside it. A caption is a word or
/// two and is bounded by its own content long before it reaches the cap, so in
/// practice the pair comes to the panel's width and not to twice the cap.
pub(crate) fn grid<R>(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    /// What a caption column is allowed, plus the gap after it.
    const CAPTION: f32 = 78.0;
    let w = fit_here(ui, f32::INFINITY);
    // Too thin for two columns: run the same rows as a WRAPPED FLOW instead.
    //
    // This needs no change at the call sites, which is the reason to do it here
    // rather than rewriting sixty rows: outside a grid, `Ui::end_row` falls
    // through to the layout, and in a wrapped horizontal that means "start a new
    // line" — exactly what it meant between grid rows. So the same closure lays
    // out as captions-beside-controls when there is room and as
    // caption-then-control when there is not, and a `ui.label(..)` that was a
    // column header simply becomes the line's first word.
    if w < CAPTION + MIN_CONTROL {
        return ui.horizontal_wrapped(add).inner;
    }
    ui.scope(|ui| {
        ui.set_max_width(w);
        egui::Grid::new(id)
            .num_columns(2)
            .spacing([8.0, 5.0])
            .max_col_width((w - CAPTION).max(36.0))
            .show(ui, add)
            .inner
    })
    .inner
}

/// A paragraph that wraps to the panel instead of running off it.
///
/// Needed because a `ui.label(..)` inside a `ui.horizontal(..)` does **not**
/// wrap — a horizontal layout sets [`egui::TextWrapMode::Extend`], which is
/// right for a caption beside a control and wrong for the sentence of help text
/// that every section in this editor ends with. The explicit `set_max_width` is
/// the other half: `wrap()` on its own wraps at the *grown* region, which is
/// already past the edge.
pub(crate) fn para(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> egui::Response {
    let w = usable_width(ui);
    ui.scope(|ui| {
        ui.set_max_width(w);
        ui.add(Label::new(text).wrap())
    })
    .inner
}

/// One chip of a segmented control.
pub(crate) struct Chip<'a> {
    /// What it says when there is room to say it.
    pub label: &'a str,
    /// What it says when there is not — usually the glyph on its own. Empty
    /// falls back to truncating `label`, which is worse but never wrong.
    pub short: &'a str,
    pub hover: &'a str,
    pub on: bool,
}

impl<'a> Chip<'a> {
    pub fn mode(label: &'a str, hover: &'a str, on: bool) -> Self {
        Self { label, short: "", hover, on }
    }

    /// What to fall back to when the chip is too narrow for `label`.
    pub fn short(mut self, short: &'a str) -> Self {
        self.short = short;
        self
    }
}

/// Draw a strip of equal-width chips that shrinks, then wraps, and never leaves
/// the panel. Returns the index of the one that was clicked.
pub(crate) fn strip(ui: &mut Ui, chips: &[Chip]) -> Option<usize> {
    if chips.is_empty() {
        return None;
    }
    let spacing = ui.spacing().item_spacing.x;
    let pad = ui.spacing().button_padding.x * 2.0;
    // The natural width is the widest label plus its padding, floored at the
    // house chip width so a strip of short labels still lines up with the rest
    // of the panel rather than collapsing to the text.
    let natural = chips
        .iter()
        .map(|c| text_w(ui, c.label) + pad)
        .fold(CHIP_W, f32::max);
    // One pixel held back: see `columns` — the reported width is not quite the
    // clipped width, and a single chip has no flooring slack to absorb that.
    let avail = (usable_width(ui) - 1.0).max(1.0);
    let (per_row, w) = columns(avail, spacing, chips.len(), natural, MIN_CHIP_W);

    let mut clicked = None;
    ui.vertical(|ui| {
        for (r, line) in chips.chunks(per_row).enumerate() {
            ui.horizontal(|ui| {
                for (c, chip) in line.iter().enumerate() {
                    // The label if it fits, the short form if that fits, and a
                    // truncated label as the last resort — an ellipsis inside
                    // the panel beats a full word outside it.
                    let text = if text_w(ui, chip.label) + pad <= w || chip.short.is_empty() {
                        chip.label
                    } else {
                        chip.short
                    };
                    let resp = ui
                        .add_sized([w, BTN_H], egui::Button::selectable(chip.on, text).truncate())
                        .on_hover_text(chip.hover);
                    if resp.clicked() {
                        clicked = Some(r * per_row + c);
                    }
                }
            });
        }
    });
    clicked
}

/// A labelled row: a caption column on the left and controls on the right,
/// which becomes a caption *above* its controls when the row gets too thin for
/// both.
///
/// `min_content` is how much room the controls need before the label column is
/// worth its width — pass the widest control's own minimum, or
/// [`MIN_CONTENT_W`] if you have no better number.
///
/// The controls always run inside a **wrapped** horizontal, in both layouts, so
/// a caller that adds five drag fields gets two lines rather than three fields
/// past the border.
pub(crate) fn row<R>(
    ui: &mut Ui,
    label: &str,
    min_content: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    row_with(ui, label, LABEL_W, min_content, add)
}

/// [`row`] with an explicit caption-column width, for a panel whose captions
/// are longer than the tile-ish tabs' 58 px — ⚙ Settings runs a 120 px column.
pub(crate) fn row_with<R>(
    ui: &mut Ui,
    label: &str,
    label_w: f32,
    min_content: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    let spacing = ui.spacing().item_spacing.x;
    let stacked = usable_width(ui) < label_w + spacing + min_content;
    if stacked {
        ui.vertical(|ui| {
            if !label.is_empty() {
                ui.add(Label::new(RichText::new(label).weak().small()).truncate().selectable(false));
            }
            ui.horizontal_wrapped(add).inner
        })
        .inner
    } else {
        // ONE wrapped horizontal with the caption as its first item, rather
        // than a caption plus a nested wrapped row. The nesting is what makes
        // the geometry unreliable: the inner layout's region and the position
        // its widgets actually land at stop agreeing, so anything sizing itself
        // against the region (`fit`, a wrapping paragraph) computes from the
        // wrong left edge. Flat, the caption is simply the first thing on the
        // line and everything after it wraps under it.
        ui.horizontal_wrapped(|ui| {
            ui.add_sized(
                [label_w, BTN_H],
                Label::new(RichText::new(label).weak()).truncate().selectable(false),
            );
            add(ui)
        })
        .inner
    }
}

/// A titled section rule: `TITLE ─────────────`, with the rule drawn only when
/// there is a panel left to draw it across.
pub(crate) fn section(ui: &mut Ui, title: &str) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.add(
            Label::new(RichText::new(title).small().strong().color(ui.visuals().strong_text_color()))
                .truncate()
                .selectable(false),
        );
        // `available_rect_before_wrap` is a region rect, and a region grows —
        // see `usable_width`. The rule is drawn to whichever edge comes first.
        let rect = ui.available_rect_before_wrap();
        let right = rect.right().min(ui.clip_rect().right());
        if right - rect.left() > 8.0 {
            let y = rect.center().y;
            ui.painter().line_segment(
                [egui::pos2(rect.left() + 4.0, y), egui::pos2(right, y)],
                ui.visuals().widgets.noninteractive.bg_stroke,
            );
        }
    });
    ui.add_space(4.0);
}

/// An action button that fills the width it is given rather than the width its
/// text wants, so a run of them lines up and none of them overhangs.
pub(crate) fn action(ui: &mut Ui, enabled: bool, text: &str, hover: &str) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(text).truncate().min_size(Vec2::new(0.0, BTN_H)),
    )
    .on_hover_text(hover)
    .on_disabled_hover_text(hover)
    .clicked()
}

fn text_w(ui: &Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    ui.painter().layout_no_wrap(text.to_owned(), font, egui::Color32::WHITE).size().x
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The widths a form panel has to survive. 420 is a comfortable dock, 140
    /// is a user who dragged the splitter almost shut — which is allowed.
    pub(crate) const LADDER: [f32; 6] = [420.0, 320.0, 260.0, 200.0, 160.0, 120.0];

    /// Run `add` in a panel `width` wide and report everything it drew that
    /// falls outside the **panel**.
    ///
    /// Horizontal only, deliberately: a form panel scrolls vertically and being
    /// below the fold is not being invisible. Being past the right edge is.
    ///
    /// A shape is reported when it crosses **its own clip rect**, with one
    /// exemption: a clip rect visibly narrower than the panel is a control
    /// clipping its own text on purpose, and that is not this guard's business.
    /// `combo_button` does exactly that, so a long asset name is cut off cleanly
    /// rather than running under the arrow — truncation *inside* the panel, not
    /// content lost from it.
    ///
    /// The exemption has to be written this way round. Measuring the visible
    /// part (rect ∩ clip) against the panel reads as the obvious simplification
    /// and is **wrong**: where nothing clipped early the clip rect IS the panel,
    /// so the intersection is inside the panel by definition and the guard can
    /// never fire at all. That mistake was caught by
    /// `the_old_fixed_width_layout_does_not_fit_and_the_harness_says_so`, which
    /// is the entire reason to keep a test whose job is to fail.
    pub(crate) fn overflow(width: f32, mut add: impl FnMut(&mut Ui)) -> Vec<String> {
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 2000.0),
            )),
            ..Default::default()
        };
        // Two frames: the first one lays fonts out and sizes anything that
        // reports its width from last frame, so a one-frame answer can be a
        // frame stale exactly where it matters.
        let mut out = None;
        for _ in 0..2 {
            out = Some(ctx.run_ui(input(), |ui| {
                egui::ScrollArea::vertical().show(ui, &mut add);
            }));
        }

        // 2 px on the right. A 1 px stroke is centred on the edge it draws, so a
        // frame or a separator sitting exactly at the border already reaches
        // half a pixel past it, and widths that have been floored to whole
        // pixels and re-derived through two or three nested layouts land within
        // a pixel of each other rather than on it. Below this nothing is out of
        // view — the failures worth catching arrive at thirty to a hundred
        // pixels, and every real one found so far did.
        //
        // 2 px on the LEFT, because a glyph's ink is allowed to sit left of its
        // own origin — an em dash and an italic `j` both do — and a paragraph
        // starting at x = 0 therefore reports a bounding box starting at −1
        // while being entirely visible. Only the right edge is where a panel
        // actually loses content.
        const EPS_RIGHT: f32 = 2.0;
        const EPS_LEFT: f32 = 2.0;
        let panel = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 2000.0));
        let mut bad = Vec::new();
        for cs in out.expect("ran at least one frame").shapes {
            let r = cs.shape.visual_bounding_rect();
            if !r.is_finite() || !r.is_positive() {
                continue;
            }
            let clip = cs.clip_rect;
            // Deliberate local truncation, not a panel overflow.
            // A shape under a clip NARROWER THAN THE PANEL, where that clip is
            // itself inside the panel, is a widget truncating its own text — a
            // combo button clipping a long asset path. Nothing it draws reaches
            // the panel edge, which is the behaviour we want, not an overflow.
            //
            // The second half is the part that matters and was missing: if the
            // clip itself runs past the panel, the clip did not save anything and
            // the shape under it is an overflow like any other. Without that,
            // anything drawn under a clip the layout had already pushed off the
            // edge was exempt from the guard entirely.
            if clip.right() < panel.right() - 1.0 && clip.right() <= panel.right() {
                continue;
            }
            let over =
                (r.right() - clip.right() - EPS_RIGHT).max(clip.left() - r.left() - EPS_LEFT);
            if over > 0.0 {
                let what = match &cs.shape {
                    egui::Shape::Text(t) => format!("text {:?}", t.galley.text()),
                    other => format!("{:?}", std::mem::discriminant(other)),
                };
                bad.push(format!("{what} overhangs by {over:.1}px at {r:?} clip {clip:?}"));
            }
        }
        bad
    }

    /// The whole ladder at once, panicking with the narrowest failure first.
    pub(crate) fn assert_fits(what: &str, mut add: impl FnMut(&mut Ui)) {
        for w in LADDER {
            let bad = overflow(w, &mut add);
            assert!(
                bad.is_empty(),
                "{what} at {w}px wide put {} thing(s) outside the panel:\n  {}",
                bad.len(),
                bad.join("\n  ")
            );
        }
    }

    #[test]
    fn columns_shrink_before_they_wrap() {
        // Room for all four at their natural width: natural width, one row.
        assert_eq!(columns(400.0, 4.0, 4, 74.0, 30.0), (4, 74.0));
        // Tighter: still one row, but each chip gives up width to stay in it.
        // (200 - 3x4 spacing) / 4 = 47.
        assert_eq!(columns(200.0, 4.0, 4, 74.0, 30.0), (4, 47.0));
    }

    #[test]
    fn columns_wrap_before_they_go_under_the_floor() {
        // Four chips in 120px would be 27px each — under the floor. Three fit
        // at 37, so it takes the widest run that clears the floor, not the
        // fewest rows: 3 + 1 rather than 2 + 2.
        let (n, w) = columns(120.0, 4.0, 4, 74.0, 30.0);
        assert_eq!(n, 3);
        assert!(w >= 30.0, "{w}");
    }

    #[test]
    fn a_sliver_of_a_panel_gets_a_sliver_of_a_chip() {
        // Narrower than the floor: one column, the full width, and NOT a chip
        // that reaches past the edge. The user is allowed to do this.
        let (n, w) = columns(20.0, 4.0, 4, 74.0, 30.0);
        assert_eq!(n, 1);
        assert!(w <= 20.0, "{w} must stay inside a 20px panel");
        assert!(w > 0.0);
    }

    #[test]
    fn columns_never_stretch_past_natural() {
        let (n, w) = columns(1000.0, 4.0, 2, 74.0, 30.0);
        assert_eq!((n, w), (2, 74.0));
    }

    #[test]
    fn a_strip_stays_inside_every_width() {
        assert_fits("a six-chip strip", |ui| {
            strip(
                ui,
                &[
                    Chip::mode("◆ Vertex", "", true).short("◆"),
                    Chip::mode("╱ Edge", "", false).short("╱"),
                    Chip::mode("▰ Face", "", false).short("▰"),
                    Chip::mode("Grow", "", false),
                    Chip::mode("Shrink", "", false),
                    Chip::mode("Invert", "", false),
                ],
            );
        });
    }

    #[test]
    fn a_labelled_row_of_drag_fields_stays_inside_every_width() {
        assert_fits("a labelled row", |ui| {
            let mut a = 1.0f32;
            let mut b = 2.0f32;
            let mut c = 3.0f32;
            row(ui, "arch", MIN_CONTENT_W, |ui| {
                ui.add(egui::DragValue::new(&mut a).prefix("segments "));
                ui.add(egui::DragValue::new(&mut b).prefix("opening w "));
                ui.add(egui::DragValue::new(&mut c).prefix("h "));
            });
        });
    }

    #[test]
    fn a_long_caption_truncates_rather_than_overhanging() {
        assert_fits("a long caption", |ui| {
            section(ui, "A SECTION TITLE NOBODY EXPECTED TO BE THIS LONG");
            row(ui, "an unreasonably long caption", MIN_CONTENT_W, |ui| {
                action(ui, true, "and an unreasonably long button too", "");
            });
        });
    }

    /// The shape the guard exists to catch — the layout every form panel in the
    /// editor used before this module. Kept as a test so the harness is known to
    /// be able to fail: if this ever passes, the harness has stopped working and
    /// every other test in this file is worthless.
    #[test]
    fn the_old_fixed_width_layout_does_not_fit_and_the_harness_says_so() {
        let bad = overflow(200.0, |ui| {
            ui.horizontal(|ui| {
                ui.add_sized([LABEL_W, BTN_H], egui::Label::new("mode"));
                for label in ["◆ Vertex", "╱ Edge", "▰ Face"] {
                    ui.add_sized([CHIP_W, BTN_H], egui::Button::new(label));
                }
            });
        });
        assert!(!bad.is_empty(), "the harness failed to notice a 280px row in a 200px panel");
    }
}
