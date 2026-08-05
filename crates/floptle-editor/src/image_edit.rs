//! The 🖼 Image tab's canvas: view transform, tool state machine, input,
//! overlays and the tab-local undo stack.
//!
//! The kernel (`floptle-image`) owns every pixel operation; this file owns the
//! *interaction* — where the canvas is on screen, which gesture is in flight,
//! what the overlays draw, and what a stroke costs in undo.
//!
//! Two house rules shape the whole thing:
//!
//! - **Nothing moves on its own.** The view is never re-centred, re-fitted or
//!   re-zoomed except when you ask (`0` fits, `Ctrl+0` is 100 %). Opening a
//!   document is the one exception, and only because there is no previous view.
//! - **The pixel grid is sacred.** In Pixel mode the zoom snaps to integer
//!   factors and the pan snaps to whole texels, so one image pixel is always an
//!   integer number of screen pixels and the grid can't shimmer.
//!
//! Undo is **tab-local** and must never reach `Editor::push_history` — that
//! snapshots the whole scene, and a scene snapshot per brush stroke would be
//! absurd. It's cheap here because `TileGrid` is copy-on-write: an undo entry is
//! a document clone, which costs one `Arc` bump per resident tile.

use std::path::PathBuf;
use std::time::SystemTime;

use egui::{Color32, Pos2, Rect as ERect, Sense, Stroke as EStroke, Vec2};
use floptle_image::brush::{Brush, BrushMode, DabCtx, GradientKind, StrokeState};
use floptle_image::composite;
use floptle_image::doc::{Image, LayerKind, Mode, VectorCache};
use floptle_image::select::{Mask, SelectOp};
use floptle_image::vector::{NodeKind, Paint, Stroke as VStroke, VNode, VPath};
use floptle_image::{Palette, Rect};

/// How many document snapshots the tab-local undo keeps.
const UNDO_DEPTH: usize = 64;
/// Rate limit for rebuilding the layer thumbnails.
const THUMB_EVERY: std::time::Duration = std::time::Duration::from_millis(400);

/// Which canvas tool is armed. Tab-local and keyed by letters — the viewport's
/// `Tool` digits are full (`gizmo.rs`), and these only apply while this tab has
/// focus, exactly like the ◈ Shaders canvas.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum ImgTool {
    #[default]
    Pencil,
    Brush,
    Eraser,
    Bucket,
    Gradient,
    Line,
    Rectangle,
    Ellipse,
    SelectRect,
    SelectEllipse,
    Lasso,
    Wand,
    Move,
    Eyedropper,
    /// Vector: drag nodes, double-click to toggle corner↔curve, click an edge to
    /// insert. The Scratch model.
    Reshape,
    /// Vector: click to lay down nodes, Enter/close to finish.
    Pen,
    /// Free transform: lift the selection (or the layer) and move / scale /
    /// rotate it with handles until you commit.
    Transform,
    /// Type into the image. Rasterized through the editor's own font stack, so
    /// it looks exactly like the text everywhere else in the engine.
    Text,
}

impl ImgTool {
    pub(crate) const ALL: [ImgTool; 18] = [
        ImgTool::Pencil,
        ImgTool::Brush,
        ImgTool::Eraser,
        ImgTool::Bucket,
        ImgTool::Gradient,
        ImgTool::Line,
        ImgTool::Rectangle,
        ImgTool::Ellipse,
        ImgTool::SelectRect,
        ImgTool::SelectEllipse,
        ImgTool::Lasso,
        ImgTool::Wand,
        ImgTool::Move,
        ImgTool::Eyedropper,
        ImgTool::Reshape,
        ImgTool::Pen,
        ImgTool::Transform,
        ImgTool::Text,
    ];

    /// Label + the single-key shortcut, for the tool strip's tooltip.
    pub(crate) fn label(self) -> (&'static str, &'static str) {
        match self {
            ImgTool::Pencil => ("Pencil", "B"),
            ImgTool::Brush => ("Brush", "B"),
            ImgTool::Eraser => ("Eraser", "E"),
            ImgTool::Bucket => ("Fill", "G"),
            ImgTool::Gradient => ("Gradient", "Shift+G"),
            ImgTool::Line => ("Line", "L"),
            ImgTool::Rectangle => ("Rectangle", "U"),
            ImgTool::Ellipse => ("Ellipse", "Shift+U"),
            ImgTool::SelectRect => ("Select box", "M"),
            ImgTool::SelectEllipse => ("Select ellipse", "Shift+M"),
            ImgTool::Lasso => ("Lasso", "Q"),
            ImgTool::Wand => ("Magic wand", "W"),
            ImgTool::Move => ("Move layer", "V"),
            ImgTool::Eyedropper => ("Eyedropper", "I"),
            ImgTool::Reshape => ("Reshape (vector)", "A"),
            ImgTool::Pen => ("Pen (vector)", "P"),
            ImgTool::Transform => ("Free transform", "Ctrl+T"),
            ImgTool::Text => ("Text", "T"),
        }
    }

    fn is_select(self) -> bool {
        matches!(
            self,
            ImgTool::SelectRect | ImgTool::SelectEllipse | ImgTool::Lasso | ImgTool::Wand
        )
    }

    fn is_paint(self) -> bool {
        matches!(self, ImgTool::Pencil | ImgTool::Brush | ImgTool::Eraser)
    }
}

/// A gesture in flight.
#[derive(Clone, Debug)]
pub(crate) enum Drag {
    /// Freehand painting.
    Stroke,
    /// A shape or marquee being dragged out: the anchor, in canvas pixels.
    Box { from: (f32, f32) },
    /// A gradient being dragged out.
    Gradient { from: (f32, f32) },
    /// Lasso point accumulation.
    Lasso,
    /// Moving the active layer: pointer anchor + the layer's offset at grab.
    MoveLayer { from: (f32, f32), offset: (i32, i32) },
    /// Dragging a vector node: path index, node index, and whether a handle.
    VectorNode { path: usize, node: usize, handle: Option<bool> },
}

/// The destructive filters, offered with a live preview rather than a blind
/// "apply and see". Each one re-runs from a snapshot every time a slider moves,
/// which is affordable precisely because a document clone is `Arc` bumps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FilterKind {
    Blur,
    Sharpen,
    Noise,
    Pixelate,
    Offset,
    Seamless,
    NormalMap,
}

impl FilterKind {
    pub(crate) const ALL: [FilterKind; 7] = [
        FilterKind::Blur,
        FilterKind::Sharpen,
        FilterKind::Noise,
        FilterKind::Pixelate,
        FilterKind::Offset,
        FilterKind::Seamless,
        FilterKind::NormalMap,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            FilterKind::Blur => "Blur",
            FilterKind::Sharpen => "Sharpen",
            FilterKind::Noise => "Noise",
            FilterKind::Pixelate => "Pixelate",
            FilterKind::Offset => "Offset (wrap)",
            FilterKind::Seamless => "Make seamless",
            FilterKind::NormalMap => "Normal map from height",
        }
    }

    pub(crate) fn hint(self) -> &'static str {
        match self {
            FilterKind::Blur => "gaussian-ish, premultiplied so transparent edges don't darken",
            FilterKind::Sharpen => "unsharp mask",
            FilterKind::Noise => "grain",
            FilterKind::Pixelate => "average each block — how the texture reads at a lower resolution",
            FilterKind::Offset => "roll the image, wrapping — brings tiling seams into the middle",
            FilterKind::Seamless => "mirror-blend both edge bands so the image tiles without a seam",
            FilterKind::NormalMap => "read the luminance as height and write a tangent-space normal",
        }
    }

    /// `(label, which param, range)` — param 0 is `a`, param 1 is `b`.
    pub(crate) fn sliders(self) -> Vec<(&'static str, usize, std::ops::RangeInclusive<f32>)> {
        match self {
            FilterKind::Blur => vec![("radius", 0, 0.5..=32.0)],
            FilterKind::Sharpen => vec![("amount", 0, 0.0..=3.0), ("radius", 1, 0.5..=8.0)],
            FilterKind::Noise => vec![("amount", 0, 0.0..=1.0)],
            FilterKind::Pixelate => vec![("block", 0, 2.0..=64.0)],
            FilterKind::Offset => vec![("x", 0, -1.0..=1.0), ("y", 1, -1.0..=1.0)],
            FilterKind::Seamless => vec![("band", 0, 1.0..=256.0)],
            FilterKind::NormalMap => vec![("strength", 0, 0.1..=8.0)],
        }
    }

    fn defaults(self) -> (f32, f32) {
        match self {
            FilterKind::Blur => (2.0, 0.0),
            FilterKind::Sharpen => (0.6, 1.0),
            FilterKind::Noise => (0.15, 0.0),
            FilterKind::Pixelate => (4.0, 0.0),
            FilterKind::Offset => (0.5, 0.5),
            FilterKind::Seamless => (24.0, 0.0),
            FilterKind::NormalMap => (2.0, 0.0),
        }
    }
}

/// A filter mid-preview: its parameters and the document it started from.
#[derive(Clone)]
pub(crate) struct FilterState {
    pub(crate) kind: FilterKind,
    pub(crate) a: f32,
    pub(crate) b: f32,
    pub(crate) mono: bool,
    base: Image,
}

/// A free transform in flight: the lifted pixels and where they're going.
///
/// Same shape as the filter preview — a snapshot of the document plus live
/// parameters, re-applied from the snapshot on every change — so the result
/// never compounds and Cancel is exact.
#[derive(Clone)]
pub(crate) struct XformSession {
    base: Image,
    /// The lifted pixels, `rect`-sized straight RGBA.
    src: Vec<u8>,
    /// Where they came from, in canvas space.
    rect: Rect,
    pub(crate) xf: floptle_image::transform::Xform,
    grab: Option<XformGrab>,
    /// Whether `rect` is cleared from the layer before the pixels are re-laid.
    /// True for a transform (the pixels are being *moved*), false for a paste
    /// (they came from the clipboard — clearing would eat what's underneath).
    lift: bool,
}

impl XformSession {
    /// The lifted region's width in canvas pixels — the "before" of a scale, so
    /// the numeric editor can offer an output size instead of only a factor.
    pub(crate) fn source_w(&self) -> u32 {
        self.rect.w
    }

    pub(crate) fn source_h(&self) -> u32 {
        self.rect.h
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum XformGrab {
    /// Dragging the body: the canvas point grabbed, and the translate at grab.
    Move { from: (f32, f32), translate: (f32, f32) },
    /// Dragging a corner (0 = top-left, clockwise): its local offset from the
    /// pivot at grab time, and the scale at grab time.
    Scale { local: (f32, f32), scale: (f32, f32) },
    /// Dragging the rotate handle: the angle grabbed at, and the rotation then.
    Rotate { from: f32, rotate: f32 },
}

/// Text being typed into the image, previewed live.
#[derive(Clone)]
pub(crate) struct TextSession {
    base: Image,
    /// Top-left of the text block, canvas space.
    pub(crate) at: (f32, f32),
    pub(crate) text: String,
    pub(crate) size: f32,
    /// The rasterized block (RGBA, its own size), rebuilt when anything changes.
    bitmap: Option<(Vec<u8>, u32, u32)>,
    /// The bitmap needs rebuilding from the font atlas (needs an egui Context).
    dirty: bool,
    /// The panel's field should claim the keyboard — ONCE, on the frame the
    /// block is placed. Asking every frame is a focus trap: nothing else in the
    /// tab can be clicked and Escape can never be seen, because the field takes
    /// focus straight back.
    focus: bool,
}

/// The New / Resize / Scale form.
#[derive(Clone, Debug)]
pub(crate) struct NewForm {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) mode: Mode,
    pub(crate) background: bool,
    /// Resizing an open document rather than creating one.
    pub(crate) resize: bool,
    /// …and resampling rather than re-canvasing.
    pub(crate) scale: bool,
    pub(crate) nearest: bool,
    /// A resize keeps the existing image in the middle rather than top-left.
    pub(crate) centre: bool,
}

impl Default for NewForm {
    fn default() -> Self {
        NewForm {
            w: 64,
            h: 64,
            mode: Mode::Pixel,
            background: false,
            resize: false,
            scale: false,
            nearest: true,
            centre: true,
        }
    }
}

impl NewForm {
    /// Sizes worth one click. Pixel presets first — that is the mode this engine
    /// exists for.
    pub(crate) const PRESETS: &'static [(&'static str, u32, u32, Mode)] = &[
        ("16²", 16, 16, Mode::Pixel),
        ("32²", 32, 32, Mode::Pixel),
        ("64²", 64, 64, Mode::Pixel),
        ("128²", 128, 128, Mode::Pixel),
        ("512²", 512, 512, Mode::Painterly),
        ("1024²", 1024, 1024, Mode::Painterly),
        ("2048²", 2048, 2048, Mode::Painterly),
    ];
}

/// What the right panel is showing about the active layer's mask.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum PaintTargetSurface {
    #[default]
    Pixels,
    /// The brush edits the active layer's MASK instead of its colour.
    Mask,
}

pub(crate) struct ImageEditState {
    /// The `.flimg` on disk (absolute). `None` for an unsaved scratch document.
    pub(crate) path: Option<PathBuf>,
    pub(crate) doc: Option<Image>,
    /// The document differs from the `.flimg` on disk.
    pub(crate) dirty: bool,
    /// The flattened PNG on disk is out of date. Split from `dirty` because the
    /// Live loop only owes the PNG — re-encoding every layer of a 2048² document
    /// four times a second would make the brush stutter.
    pub(crate) png_dirty: bool,
    /// mtime of `path` when we last read/wrote it — an external edit re-loads.
    pub(crate) mtime: Option<SystemTime>,

    // --- view ---
    pub(crate) zoom: f32,
    /// Screen offset of the canvas origin within the view rect.
    pub(crate) pan: Vec2,
    /// Set once when a document opens; the view is never re-fitted after that.
    pub(crate) fit_pending: bool,
    pub(crate) tiled_view: bool,
    /// How the overlays draw — colours, opacities, and the zoom the pixel grid
    /// starts at. Per-user, loaded once and saved when the View menu changes it.
    pub(crate) look: crate::prefs::CanvasLook,
    pub(crate) onion: bool,
    pub(crate) frame: usize,
    pub(crate) playing: bool,
    play_clock: f32,

    // --- tools ---
    pub(crate) tool: ImgTool,
    pub(crate) brush: Brush,
    pub(crate) color: [u8; 4],
    pub(crate) color2: [u8; 4],
    pub(crate) tolerance: u8,
    pub(crate) contiguous: bool,
    pub(crate) sel_op: SelectOp,
    pub(crate) sel_feather: u32,
    pub(crate) shape_fill: bool,
    pub(crate) shape_stroke: bool,
    pub(crate) shape_vector: bool,
    pub(crate) stroke_width: f32,
    pub(crate) grad_kind: GradientKind,
    pub(crate) surface: PaintTargetSurface,
    pub(crate) clone_src: Option<(f32, f32)>,
    /// Symmetry: mirror every dab across the canvas centre.
    pub(crate) mirror_x: bool,
    pub(crate) mirror_y: bool,

    // --- interaction ---
    drag: Option<Drag>,
    stroke: StrokeState,
    lasso: Vec<(f32, f32)>,
    pen: Option<VPath>,
    /// The selected vector node (path, node), for the reshape tool's handles.
    pub(crate) sel_node: Option<(usize, usize)>,
    /// Live cursor position in canvas pixels (brush outline + status bar).
    pub(crate) cursor: Option<(f32, f32)>,
    /// The canvas rect as of the last frame, so keyboard zoom has something to
    /// zoom *about* (the view centre) without waiting for a mouse move.
    last_view: Option<ERect>,
    /// Cut/copied pixels: straight RGBA and its size. Tab-local — the OS
    /// clipboard carries text, and a raster block is not text.
    clip: Option<(Vec<u8>, u32, u32)>,
    /// What's being typed into the hex field, while it's being typed. `None`
    /// means "show the current colour" — without this, the field would fight
    /// you for the caret on every keystroke.
    pub(crate) hex_entry: Option<String>,

    // --- undo ---
    undo: Vec<Image>,
    redo: Vec<Image>,
    /// The pre-edit document backing the NEXT undo push, while a continuous
    /// edit (a dragged slider) is in flight.
    pending_undo: Option<Image>,

    // --- presentation ---
    tex: Option<egui::TextureHandle>,
    tex_size: (u32, u32),
    tex_nearest: bool,
    /// The previous frame, composited once per frame change for the onion skin.
    onion_tex: Option<egui::TextureHandle>,
    /// Per-layer thumbnails for the layer list, rebuilt on a slow rate limit —
    /// a 2048² document costs a full composite per layer, so this must never
    /// ride the brush.
    thumbs: Vec<Option<egui::TextureHandle>>,
    thumbs_at: Option<std::time::Instant>,
    thumbs_dirty: bool,
    /// Canvas region needing a recomposite before the next paint.
    pending: Option<Rect>,
    vcache: VectorCache,
    /// The last full composite, kept for colour picking. Rebuilding it per
    /// pointer-move (the eyedropper drags) would recomposite the whole canvas
    /// dozens of times a second.
    flat_cache: Option<Vec<u8>>,
    /// Cached marching-ants segments (canvas space), rebuilt when the selection
    /// changes — scanning a 2048² mask every frame would not be free.
    ants: Vec<[(f32, f32); 2]>,
    ants_valid: bool,
    pub(crate) status: Option<(String, f32)>,
    /// Palettes offered in the panel: the built-ins plus `.floptle/palettes/`.
    pub(crate) palettes: Vec<Palette>,
    pub(crate) palettes_loaded: bool,
    /// The tab drew this frame — the hot-reload poll and playback only run then.
    pub(crate) tab_visible: bool,
    /// Sheet-export column count (0 = auto).
    pub(crate) sheet_cols: u32,

    // --- dialogs / modes ---
    /// The keyboard-shortcut sheet is open.
    pub(crate) show_keys: bool,
    pub(crate) new_form: Option<NewForm>,
    pub(crate) save_name: Option<String>,
    pub(crate) filter: Option<FilterState>,
    /// A free transform in flight.
    pub(crate) xform: Option<XformSession>,
    /// Text being typed into the image.
    pub(crate) text: Option<TextSession>,
    /// Re-export the PNG after every edit, so the mesh in the Scene view keeps
    /// up with the brush. This is the whole pitch of an in-engine editor, and
    /// it costs one PNG write per quiet moment (never mid-stroke).
    pub(crate) live: bool,
    pub(crate) last_live: Option<std::time::Instant>,
}

impl Default for ImageEditState {
    fn default() -> Self {
        ImageEditState {
            path: None,
            doc: None,
            dirty: false,
            png_dirty: false,
            mtime: None,
            zoom: 8.0,
            pan: Vec2::ZERO,
            fit_pending: true,
            tiled_view: false,
            look: crate::prefs::load_canvas_look(),
            onion: false,
            frame: 0,
            playing: false,
            play_clock: 0.0,
            tool: ImgTool::Pencil,
            brush: Brush::default(),
            color: [30, 30, 40, 255],
            color2: [255, 255, 255, 255],
            tolerance: 16,
            contiguous: true,
            sel_op: SelectOp::Replace,
            sel_feather: 0,
            shape_fill: true,
            shape_stroke: false,
            shape_vector: false,
            stroke_width: 2.0,
            grad_kind: GradientKind::Linear,
            surface: PaintTargetSurface::Pixels,
            clone_src: None,
            mirror_x: false,
            mirror_y: false,
            drag: None,
            stroke: StrokeState::default(),
            lasso: Vec::new(),
            pen: None,
            sel_node: None,
            cursor: None,
            last_view: None,
            clip: None,
            hex_entry: None,
            undo: Vec::new(),
            redo: Vec::new(),
            pending_undo: None,
            tex: None,
            tex_size: (0, 0),
            tex_nearest: true,
            onion_tex: None,
            thumbs: Vec::new(),
            thumbs_at: None,
            thumbs_dirty: true,
            pending: None,
            flat_cache: None,
            vcache: VectorCache::default(),
            ants: Vec::new(),
            ants_valid: false,
            status: None,
            palettes: Vec::new(),
            palettes_loaded: false,
            tab_visible: false,
            sheet_cols: 0,
            show_keys: false,
            new_form: None,
            save_name: None,
            filter: None,
            xform: None,
            text: None,
            live: false,
            last_live: None,
        }
    }
}

impl ImageEditState {
    // --- document lifecycle ----------------------------------------------

    /// Adopt a freshly-loaded document. Resets the view exactly once (there is
    /// no previous view to preserve) and clears the undo history.
    pub(crate) fn adopt(&mut self, doc: Image, path: Option<PathBuf>, mtime: Option<SystemTime>) {
        self.frame = 0;
        self.tool = match doc.mode {
            Mode::Vector => ImgTool::Reshape,
            Mode::Painterly => ImgTool::Brush,
            Mode::Pixel => ImgTool::Pencil,
        };
        self.brush = default_brush_for(doc.mode);
        self.doc = Some(doc);
        self.path = path;
        self.mtime = mtime;
        self.dirty = false;
        self.png_dirty = false;
        self.undo.clear();
        self.redo.clear();
        self.pending_undo = None;
        self.drag = None;
        self.lasso.clear();
        self.pen = None;
        self.sel_node = None;
        self.fit_pending = true;
        self.invalidate_all();
    }

    pub(crate) fn close(&mut self) {
        *self = ImageEditState {
            palettes: std::mem::take(&mut self.palettes),
            palettes_loaded: self.palettes_loaded,
            // Copy in one document, close it, paste into the next — the
            // clipboard belongs to the tab, not to the file that filled it.
            clip: self.clip.take(),
            ..Default::default()
        };
    }

    pub(crate) fn title(&self) -> String {
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".into());
        if self.dirty { format!("{name} •") } else { name }
    }

    /// A gesture or a filter preview is in flight — the moment NOT to write a
    /// PNG to disk.
    pub(crate) fn busy(&self) -> bool {
        self.drag.is_some() || self.filter.is_some() || self.xform.is_some() || self.text.is_some()
    }

    pub(crate) fn toast(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), 3.0));
    }

    // --- undo -------------------------------------------------------------

    /// Begin a CONTINUOUS edit (a dragged opacity slider, a retuned adjustment).
    ///
    /// The snapshot is taken once at the start of the drag and banked when the
    /// pointer comes up — the shader graph's `pending_undo` pattern — so a
    /// three-second slider drag is one undo step, not two hundred.
    pub(crate) fn begin_edit(&mut self) {
        if self.pending_undo.is_none() {
            self.pending_undo = self.doc.clone();
        }
        self.mark_dirty();
    }

    /// Bank a pending continuous edit. Called once the pointer is up.
    pub(crate) fn flush_edit(&mut self) {
        let Some(prev) = self.pending_undo.take() else { return };
        self.undo.push(prev);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Bank the current document before a mutation. Cheap: tiles are `Arc`s, so
    /// this copies layer metadata and bumps refcounts, not pixels.
    pub(crate) fn push_undo(&mut self) {
        let Some(doc) = &self.doc else { return };
        self.undo.push(doc.clone());
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.mark_dirty();
    }

    /// The document changed: both the `.flimg` and the exported PNG are stale.
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
        self.png_dirty = true;
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Commit whatever modal gesture is live (transform, text, filter).
    ///
    /// Those hold a SNAPSHOT of the document and re-apply themselves onto it, so
    /// anything that changes what they're applying to — switching layer, moving
    /// to another frame — has to settle them first, or the next preview writes
    /// into a layer the session never meant.
    pub(crate) fn commit_live(&mut self) {
        self.commit_transform();
        self.commit_text();
        self.commit_filter();
    }

    /// The same, but throwing the gesture away — what history does, because an
    /// undo that left a live preview attached to a document it no longer
    /// matches would re-apply it over the restored one.
    pub(crate) fn cancel_live(&mut self) {
        self.cancel_transform();
        self.cancel_text();
        self.cancel_filter();
    }

    pub(crate) fn undo(&mut self) {
        self.cancel_live();
        let (Some(prev), Some(cur)) = (self.undo.pop(), self.doc.clone()) else { return };
        self.redo.push(cur);
        self.doc = Some(prev);
        self.after_history();
    }

    pub(crate) fn redo(&mut self) {
        self.cancel_live();
        let (Some(next), Some(cur)) = (self.redo.pop(), self.doc.clone()) else { return };
        self.undo.push(cur);
        self.doc = Some(next);
        self.after_history();
    }

    fn after_history(&mut self) {
        self.pending_undo = None;
        self.mark_dirty();
        self.drag = None;
        self.lasso.clear();
        if let Some(d) = &self.doc {
            self.frame = self.frame.min(d.frames.saturating_sub(1));
        }
        self.invalidate_all();
    }

    // --- invalidation -----------------------------------------------------

    /// The whole canvas needs recompositing (a layer/mode/selection changed).
    pub(crate) fn invalidate_all(&mut self) {
        self.pending = self.doc.as_ref().map(|d| d.bounds());
        self.ants_valid = false;
        self.flat_cache = None;
        self.onion_tex = None;
        self.thumbs_dirty = true;
        self.vcache.clear();
    }

    /// Only `r` changed (a brush dab).
    pub(crate) fn invalidate(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        self.flat_cache = None;
        self.pending = Some(self.pending.map_or(r, |p| p.union(r)));
    }

    /// Call after any edit to the active vector layer's paths.
    pub(crate) fn invalidate_vectors(&mut self) {
        self.vcache.clear();
        self.invalidate_all();
    }

    // --- view maths -------------------------------------------------------

    /// Snap the zoom for Pixel mode: integer factors up, 1/n down. A pixel
    /// editor that shows a blurry approximation of your own art is worthless.
    fn snap_zoom(&self, z: f32) -> f32 {
        if !self.pixel_mode() {
            return z.clamp(0.02, 64.0);
        }
        let z = z.clamp(0.02, 64.0);
        if z >= 1.0 {
            z.round().max(1.0)
        } else {
            1.0 / (1.0 / z).round().max(1.0)
        }
    }

    pub(crate) fn pixel_mode(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.mode == Mode::Pixel)
    }

    fn canvas_origin(&self, view: ERect) -> Pos2 {
        let mut p = view.min + self.pan;
        if self.pixel_mode() && self.zoom >= 1.0 {
            // Whole-texel pan, or the grid shimmers as you drag.
            p.x = p.x.round();
            p.y = p.y.round();
        }
        p
    }

    pub(crate) fn to_screen(&self, view: ERect, x: f32, y: f32) -> Pos2 {
        let o = self.canvas_origin(view);
        Pos2::new(o.x + x * self.zoom, o.y + y * self.zoom)
    }

    pub(crate) fn to_canvas(&self, view: ERect, p: Pos2) -> (f32, f32) {
        let o = self.canvas_origin(view);
        ((p.x - o.x) / self.zoom, (p.y - o.y) / self.zoom)
    }

    /// Fit the whole canvas in `view` with a little margin.
    pub(crate) fn fit(&mut self, view: ERect) {
        let Some(d) = &self.doc else { return };
        let sx = (view.width() - 32.0).max(16.0) / d.w as f32;
        let sy = (view.height() - 32.0).max(16.0) / d.h as f32;
        self.zoom = self.snap_zoom(sx.min(sy));
        self.center(view);
    }

    pub(crate) fn center(&mut self, view: ERect) {
        let Some(d) = &self.doc else { return };
        self.pan = Vec2::new(
            (view.width() - d.w as f32 * self.zoom) * 0.5,
            (view.height() - d.h as f32 * self.zoom) * 0.5,
        );
    }

    /// Zoom about a screen point, keeping the canvas pixel under it fixed.
    pub(crate) fn zoom_at(&mut self, view: ERect, at: Pos2, factor: f32) {
        let before = self.to_canvas(view, at);
        let z = self.snap_zoom(self.zoom * factor);
        if (z - self.zoom).abs() < 1e-6 {
            return;
        }
        self.zoom = z;
        let after = self.to_canvas(view, at);
        self.pan += Vec2::new((after.0 - before.0) * self.zoom, (after.1 - before.1) * self.zoom);
    }

    // --- selection helpers -------------------------------------------------

    fn selection(&self) -> Option<&Mask> {
        self.doc.as_ref().and_then(|d| d.selection.as_ref())
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.selection().is_some()
    }

    /// Apply a freshly-built marquee under the current boolean op.
    fn apply_marquee(&mut self, mut m: Mask) {
        if self.sel_feather > 0 {
            m.feather(self.sel_feather);
        }
        let Some(doc) = &mut self.doc else { return };
        match (doc.selection.take(), self.sel_op) {
            (Some(mut cur), op) if op != SelectOp::Replace => {
                cur.combine(&m, op);
                doc.selection = Some(cur);
            }
            _ => doc.selection = Some(m),
        }
        // An empty result means "no selection", not "nothing is editable" — the
        // difference between a working canvas and a mysteriously dead one.
        if doc.selection.as_ref().is_some_and(|s| s.is_empty() || s.is_full()) {
            doc.selection = None;
        }
        self.ants_valid = false;
    }

    /// Clear the selection — which is the same thing as selecting everything,
    /// because every operation treats "no selection" as "the whole canvas".
    /// Bound to both Ctrl+A and Ctrl+D, since either is a reasonable reflex.
    pub(crate) fn deselect(&mut self) {
        if !self.has_selection() {
            return;
        }
        self.push_undo();
        if let Some(d) = &mut self.doc {
            d.selection = None;
        }
        self.ants_valid = false;
        self.toast("selection cleared (the whole canvas is editable)");
    }

    // --- clipboard -----------------------------------------------------------

    /// Copy the selection (or, with none, everything painted on this layer) out
    /// of the active layer. `cut` erases what it took.
    ///
    /// Tab-local rather than the OS clipboard: the engine's node clipboard rides
    /// the OS one as RON text, and a raster block is not text. Pasting between
    /// two documents works because the tab outlives the document.
    pub(crate) fn copy_selection(&mut self, cut: bool) -> bool {
        let frame = self.frame;
        let Some(doc) = self.doc.as_ref() else { return false };
        let Some(layer) = doc.layers.get(doc.active) else { return false };
        let off = layer.offset;
        let Some(grid) = layer.grid(frame) else {
            self.toast("copy needs a pixel layer");
            return false;
        };
        let rect = match doc.selection.as_ref() {
            Some(sel) => sel.selected_bounds(),
            None => {
                let b = grid.opaque_bounds();
                Rect::new(b.x + off.0, b.y + off.1, b.w, b.h)
            }
        }
        .intersect(doc.bounds());
        if rect.is_empty() {
            self.toast("nothing to copy");
            return false;
        }
        let mut px = grid.read_rect(Rect::new(rect.x - off.0, rect.y - off.1, rect.w, rect.h));
        if let Some(sel) = doc.selection.as_ref() {
            for y in 0..rect.h as i32 {
                for x in 0..rect.w as i32 {
                    let o = (y as usize * rect.w as usize + x as usize) * 4 + 3;
                    px[o] = floptle_image::u8c(px[o] as f32 * sel.at(rect.x + x, rect.y + y));
                }
            }
        }
        self.clip = Some((px, rect.w, rect.h));
        if cut {
            self.delete_selection();
            self.toast(format!("cut {}×{}", rect.w, rect.h));
        } else {
            self.toast(format!("copied {}×{}", rect.w, rect.h));
        }
        true
    }

    /// Paste the clipboard as a FLOATING transform: it lands under the cursor
    /// (or in the middle), and stays movable until Enter commits it. That way a
    /// paste is never a blind drop in the corner you then have to hunt for.
    pub(crate) fn paste(&mut self) -> bool {
        let Some((px, cw, ch)) = self.clip.clone() else {
            self.toast("nothing copied yet");
            return false;
        };
        let ok = self
            .doc
            .as_ref()
            .and_then(|d| d.layers.get(d.active))
            .is_some_and(|l| l.kind.is_raster() && !l.locked);
        if !ok {
            self.toast("paste needs an unlocked pixel layer");
            return false;
        }
        // Anything already in flight commits first — a paste is a new gesture.
        self.commit_live();
        let Some(doc) = self.doc.as_ref() else { return false };
        let (dw, dh) = (doc.w, doc.h);
        let at = self.cursor.unwrap_or((dw as f32 / 2.0, dh as f32 / 2.0));
        let rect = Rect::new(
            (at.0 - cw as f32 / 2.0).round() as i32,
            (at.1 - ch as f32 / 2.0).round() as i32,
            cw,
            ch,
        );
        let base = doc.clone();
        self.xform = Some(XformSession {
            base,
            src: px,
            rect,
            xf: floptle_image::transform::Xform {
                pivot: (rect.x as f32 + cw as f32 / 2.0, rect.y as f32 + ch as f32 / 2.0),
                ..Default::default()
            },
            grab: None,
            lift: false,
        });
        self.tool = ImgTool::Transform;
        self.apply_xform_preview();
        self.toast("pasted — drag to place, Enter to apply");
        true
    }

    pub(crate) fn has_clipboard(&self) -> bool {
        self.clip.is_some()
    }

    /// Arrow-key nudge: the floating transform if there is one, otherwise the
    /// active layer.
    pub(crate) fn nudge(&mut self, dx: i32, dy: i32) {
        if self.xform.is_some() {
            if let Some(s) = self.xform.as_mut() {
                s.xf.translate = (s.xf.translate.0 + dx as f32, s.xf.translate.1 + dy as f32);
            }
            self.apply_xform_preview();
            return;
        }
        let locked = self
            .doc
            .as_ref()
            .and_then(|d| d.layers.get(d.active))
            .is_some_and(|l| l.locked);
        if locked {
            return;
        }
        self.begin_edit();
        if let Some(d) = &mut self.doc {
            let a = d.active;
            if let Some(l) = d.layers.get_mut(a) {
                l.offset = (l.offset.0 + dx, l.offset.1 + dy);
            }
        }
        self.invalidate_all();
    }

    /// Keyboard zoom, about the middle of the view (there may be no cursor).
    pub(crate) fn zoom_step(&mut self, factor: f32) {
        let Some(view) = self.last_view else {
            self.zoom = self.snap_zoom(self.zoom * factor);
            return;
        };
        self.zoom_at(view, view.center(), factor);
    }

    /// Erase what's selected on the active layer (Delete). With no selection
    /// this clears the layer — which is what Delete means in every editor, and
    /// is one Ctrl+Z away.
    pub(crate) fn delete_selection(&mut self) {
        self.push_undo();
        let frame = self.frame;
        let Some(doc) = &mut self.doc else { return };
        let (bounds, sel) = (doc.bounds(), doc.selection.clone());
        let active = doc.active;
        if let Some(l) = doc.layers.get_mut(active)
            && !l.locked
            && let Some(g) = l.grid_mut(frame)
        {
            floptle_image::brush::clear_region(g, bounds, sel.as_ref());
            g.prune();
        }
        self.invalidate_all();
    }

    pub(crate) fn invert_selection(&mut self) {
        self.push_undo();
        let Some(d) = &mut self.doc else { return };
        let mut m = d.selection.take().unwrap_or_else(|| Mask::new(d.w, d.h, 0));
        m.invert();
        d.selection = Some(m);
        self.ants_valid = false;
    }

    // --- the paint plumbing ------------------------------------------------

    /// The colour a stroke lays down, honouring palette lock.
    pub(crate) fn stroke_color(&self) -> [u8; 4] {
        self.locked_color(self.color)
    }

    /// The same for the secondary colour (gradient end, shape stroke) — a locked
    /// palette that only guarded the primary would leak off-palette colours
    /// through every gradient.
    pub(crate) fn stroke_color2(&self) -> [u8; 4] {
        self.locked_color(self.color2)
    }

    fn locked_color(&self, c: [u8; 4]) -> [u8; 4] {
        match self.doc.as_ref() {
            Some(d) if d.palette_lock => d.palette.as_ref().map_or(c, |p| p.snap(c)),
            _ => c,
        }
    }

    /// Paint one dab (or continue a stroke) at canvas (x, y).
    fn paint_at(&mut self, x: f32, y: f32, first: bool) {
        let color = self.stroke_color();
        let Some(doc) = &mut self.doc else { return };
        let (w, h) = (doc.w, doc.h);
        let mut brush = self.brush.clone();
        if self.tool == ImgTool::Eraser {
            brush.mode = BrushMode::Erase;
        }
        // Painting a MASK is the same brush over an 8-bit surface: do it by
        // hand rather than pretending the mask is a layer.
        if self.surface == PaintTargetSurface::Mask {
            let dirty = paint_mask(doc, &brush, x, y, self.tool == ImgTool::Eraser);
            self.invalidate(dirty);
            return;
        }
        let clone = self
            .clone_src
            .map(|(sx, sy)| (x - sx, y - sy))
            .unwrap_or((0.0, 0.0));
        let sel_ptr = doc.selection.clone();
        let tiling = doc.tiling;
        let active = doc.active;
        let Some(layer) = doc.layers.get_mut(active) else { return };
        if layer.locked || !layer.visible {
            return;
        }
        let origin = layer.offset;
        let Some(grid) = layer.grid_mut(self.frame) else { return };
        let ctx = DabCtx {
            sel: sel_ptr.as_ref(),
            origin,
            canvas: (w, h),
            wrap: tiling,
            clone_offset: clone,
        };
        let mut dirty = Rect::EMPTY;
        // The mirrors run as independent dabs at the same stroke phase.
        let pts: Vec<(f32, f32)> = {
            let mut v = vec![(x, y)];
            if self.mirror_x {
                v.push((w as f32 - x, y));
            }
            if self.mirror_y {
                v.push((x, h as f32 - y));
            }
            if self.mirror_x && self.mirror_y {
                v.push((w as f32 - x, h as f32 - y));
            }
            v
        };
        for (i, (px, py)) in pts.iter().enumerate() {
            if i == 0 {
                if first {
                    self.stroke.begin(*px, *py, w, h);
                    dirty = dirty.union(floptle_image::brush::stamp(
                        grid, &brush, *px, *py, color, &ctx, &mut self.stroke,
                    ));
                } else {
                    dirty = dirty.union(floptle_image::brush::stroke_to(
                        grid, &brush, *px, *py, color, &ctx, &mut self.stroke,
                    ));
                }
            } else {
                // Mirrors get their own throwaway stroke state so their spacing
                // and pixel-perfect history don't fight the primary stroke's.
                let mut s = StrokeState::default();
                s.begin(*px, *py, w, h);
                dirty = dirty.union(floptle_image::brush::stamp(
                    grid, &brush, *px, *py, color, &ctx, &mut s,
                ));
            }
        }
        self.invalidate(dirty);
    }

    // --- gestures ----------------------------------------------------------

    /// Run the canvas: allocate the view, handle input, paint everything.
    /// Returns the view rect so the caller can show a status bar for it.
    pub(crate) fn canvas_ui(&mut self, ui: &mut egui::Ui) -> ERect {
        let view = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(view, Sense::click_and_drag());
        self.last_view = Some(view);
        if self.doc.is_none() {
            return view;
        }
        if self.fit_pending {
            self.fit(view);
            self.fit_pending = false;
        }
        self.handle_input(ui, &resp, view);
        self.sync_texture(ui.ctx());
        self.paint(ui, view);
        view
    }

    fn handle_input(&mut self, ui: &mut egui::Ui, resp: &egui::Response, view: ERect) {
        let ctx = ui.ctx().clone();
        let hovered = resp.hovered();
        let (scroll, modifiers, space, ptr) = ctx.input(|i| {
            (
                i.smooth_scroll_delta.y,
                i.modifiers,
                i.key_down(egui::Key::Space),
                i.pointer.latest_pos(),
            )
        });
        self.cursor = ptr.filter(|p| view.contains(*p)).map(|p| self.to_canvas(view, p));
        // The pointer says which tool is armed without anyone reading the strip.
        if hovered {
            ctx.set_cursor_icon(match self.tool {
                _ if space => egui::CursorIcon::Grab,
                ImgTool::Move => egui::CursorIcon::Move,
                ImgTool::Text => egui::CursorIcon::Text,
                ImgTool::Transform => egui::CursorIcon::Move,
                // NOT `CursorIcon::None` for the brushes: a 1 px nib telegraphs
                // as a 2 px ring, and "my cursor vanished" is a worse trade
                // than a crosshair sitting inside the outline.
                _ => egui::CursorIcon::Crosshair,
            });
        }

        // --- navigation ---
        if hovered && scroll.abs() > 0.0
            && let Some(p) = ptr {
                if modifiers.ctrl {
                    // Ctrl+wheel = brush size, the muscle memory from every
                    // paint program.
                    let step = if scroll > 0.0 { 1.0 } else { -1.0 };
                    self.brush.radius = (self.brush.radius + step * (self.brush.radius * 0.25).max(0.5)).clamp(0.5, 512.0);
                } else {
                    let f = (scroll * 0.0035).exp();
                    self.zoom_at(view, p, f);
                }
            }
        if hovered {
            let (fit, hundred) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::Num0) && !i.modifiers.ctrl,
                    i.key_pressed(egui::Key::Num0) && i.modifiers.ctrl,
                )
            });
            if hundred {
                self.zoom = 1.0;
                self.center(view);
            } else if fit {
                self.fit(view);
            }
        }
        // Middle-drag or Space+drag pans.
        let middle = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Middle));
        if (middle || (space && resp.dragged())) && resp.dragged() {
            self.pan += resp.drag_delta();
            if middle {
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            return;
        }
        // Right-drag = eyedropper preview, pick on release.
        if resp.dragged_by(egui::PointerButton::Secondary)
            || resp.clicked_by(egui::PointerButton::Secondary)
        {
            if let Some((x, y)) = self.cursor
                && let Some(c) = self.pick_color(x, y)
            {
                self.color = c;
                // A live text block follows the colour you just picked.
                self.mark_text_dirty();
            }
            return;
        }

        let Some((cx, cy)) = self.cursor else { return };
        let shift = modifiers.shift;
        let alt = modifiers.alt;

        // Alt+click sets the clone-stamp source, the universal binding.
        if self.brush.mode == BrushMode::Clone && alt && resp.drag_started() {
            self.clone_src = Some((cx, cy));
            self.toast("clone source set");
            return;
        }

        if resp.drag_started() {
            self.begin_drag(cx, cy, shift, alt);
        } else if resp.dragged() {
            self.continue_drag(cx, cy, shift);
        } else if resp.drag_stopped() {
            self.end_drag(cx, cy, shift);
        } else if resp.clicked() {
            // A click with no drag: tools that act on a point.
            self.click(cx, cy, shift);
        }
        if resp.double_clicked() && self.tool == ImgTool::Reshape {
            self.toggle_node_kind(cx, cy);
        }
    }

    fn begin_drag(&mut self, x: f32, y: f32, shift: bool, _alt: bool) {
        // The live preview states are modal over the document; starting a
        // gesture with a DIFFERENT tool means you're done with them.
        if self.filter.is_some() {
            self.commit_filter();
        }
        if self.xform.is_some() && self.tool != ImgTool::Transform {
            self.commit_transform();
        }
        if self.text.is_some() && self.tool != ImgTool::Text {
            self.commit_text();
        }
        match self.tool {
            // Point tools: `clicked()` never fires once a drag starts, so they
            // act here as well. The two are mutually exclusive in egui, so this
            // can't double-apply.
            ImgTool::Bucket | ImgTool::Wand | ImgTool::Eyedropper | ImgTool::Pen => {
                self.click(x, y, shift);
            }
            // …but NOT while a transform or text block is live: those own the
            // canvas until they're committed.
            t if t.is_paint() => {
                self.push_undo();
                self.drag = Some(Drag::Stroke);
                self.paint_at(x, y, true);
            }
            ImgTool::Line | ImgTool::Rectangle | ImgTool::Ellipse => {
                self.drag = Some(Drag::Box { from: (x, y) });
            }
            ImgTool::Gradient => {
                self.drag = Some(Drag::Gradient { from: (x, y) });
            }
            ImgTool::SelectRect | ImgTool::SelectEllipse => {
                // Dragging INSIDE a live selection moves it. Every editor does
                // this, it is obviously what you were about to do, and needing a
                // tool change first is most of why moving a bit of art felt like
                // more work than the edit (`floptle/0095`).
                if self.press_moves_selection(x, y) {
                    return;
                }
                self.drag = Some(Drag::Box { from: (x, y) });
            }
            ImgTool::Lasso => {
                if self.press_moves_selection(x, y) {
                    return;
                }
                self.lasso.clear();
                self.lasso.push((x, y));
                self.drag = Some(Drag::Lasso);
            }
            ImgTool::Move => {
                let off = self
                    .doc
                    .as_ref()
                    .and_then(|d| d.layers.get(d.active))
                    .map(|l| l.offset)
                    .unwrap_or((0, 0));
                self.push_undo();
                self.drag = Some(Drag::MoveLayer { from: (x, y), offset: off });
            }
            ImgTool::Transform => {
                // First press lifts; later presses grab a handle. A press
                // outside the box means "done with this one".
                if self.xform.is_none() && !self.begin_transform() {
                    return;
                }
                if !self.grab_transform(x, y) {
                    self.commit_transform();
                }
            }
            ImgTool::Text => self.begin_text(x, y),
            ImgTool::Reshape => {
                if let Some(hit) = self.hit_vector(x, y) {
                    self.push_undo();
                    self.sel_node = Some((hit.0, hit.1));
                    self.drag = Some(Drag::VectorNode { path: hit.0, node: hit.1, handle: hit.2 });
                }
            }
            _ => {}
        }
    }

    fn continue_drag(&mut self, x: f32, y: f32, shift: bool) {
        match self.drag.clone() {
            Some(Drag::Stroke) => self.paint_at(x, y, false),
            Some(Drag::Lasso) => {
                if self
                    .lasso
                    .last()
                    .is_none_or(|(lx, ly)| (lx - x).abs() + (ly - y).abs() > 1.0)
                {
                    self.lasso.push((x, y));
                }
            }
            Some(Drag::MoveLayer { from, offset }) => {
                let (dx, dy) = ((x - from.0).round() as i32, (y - from.1).round() as i32);
                if let Some(d) = &mut self.doc
                    && let Some(l) = d.layers.get_mut(d.active)
                {
                    l.offset = (offset.0 + dx, offset.1 + dy);
                }
                self.invalidate_all();
            }
            Some(Drag::VectorNode { path, node, handle }) => {
                self.drag_vector_node(path, node, handle, x, y, shift);
            }
            None if self.tool == ImgTool::Transform => self.drag_transform(x, y, shift),
            _ => {}
        }
    }

    fn end_drag(&mut self, x: f32, y: f32, shift: bool) {
        let drag = self.drag.take();
        match drag {
            Some(Drag::Stroke) => {
                self.stroke.end();
                self.prune_active();
            }
            Some(Drag::Box { from }) => {
                let (x1, y1) = constrain(from, (x, y), shift, self.tool == ImgTool::Line);
                match self.tool {
                    ImgTool::SelectRect => {
                        self.push_undo();
                        let (w, h) = self.canvas_size();
                        let r = rect_between(from, (x1, y1));
                        self.apply_marquee(floptle_image::select::rect_mask(w, h, r));
                    }
                    ImgTool::SelectEllipse => {
                        self.push_undo();
                        let (w, h) = self.canvas_size();
                        let r = rect_between(from, (x1, y1));
                        self.apply_marquee(floptle_image::select::ellipse_mask(w, h, r));
                    }
                    ImgTool::Line | ImgTool::Rectangle | ImgTool::Ellipse => {
                        self.commit_shape(from, (x1, y1));
                    }
                    _ => {}
                }
            }
            Some(Drag::Gradient { from }) => {
                let to = constrain(from, (x, y), shift, true);
                self.push_undo();
                let color = self.stroke_color();
                let c2 = self.stroke_color2();
                let kind = self.grad_kind;
                let sel = self.doc.as_ref().and_then(|d| d.selection.clone());
                let frame = self.frame;
                if let Some(doc) = &mut self.doc {
                    let bounds = doc.bounds();
                    let active = doc.active;
                    if let Some(l) = doc.layers.get_mut(active)
                        && !l.locked
                        && let Some(g) = l.grid_mut(frame)
                    {
                        floptle_image::brush::gradient_fill(
                            g,
                            bounds,
                            kind,
                            from,
                            to,
                            color,
                            c2,
                            sel.as_ref(),
                            floptle_image::Blend::Mix,
                            1.0,
                        );
                    }
                }
                self.invalidate_all();
            }
            Some(Drag::Lasso) => {
                self.push_undo();
                let (w, h) = self.canvas_size();
                let pts = std::mem::take(&mut self.lasso);
                if pts.len() >= 3 {
                    self.apply_marquee(floptle_image::select::polygon_mask(w, h, &pts));
                }
            }
            Some(Drag::MoveLayer { .. }) => self.invalidate_all(),
            None if self.tool == ImgTool::Transform => {
                if let Some(t) = self.xform.as_mut() {
                    t.grab = None;
                }
            }
            Some(Drag::VectorNode { .. }) => {
                self.invalidate_vectors();
            }
            _ => {}
        }
    }

    fn click(&mut self, x: f32, y: f32, shift: bool) {
        match self.tool {
            ImgTool::Bucket => {
                self.push_undo();
                let color = self.stroke_color();
                let (tol, contig) = (self.tolerance, self.contiguous);
                let sel = self.doc.as_ref().and_then(|d| d.selection.clone());
                let frame = self.frame;
                if let Some(doc) = &mut self.doc {
                    let active = doc.active;
                    if let Some(l) = doc.layers.get_mut(active)
                        && !l.locked
                    {
                        let off = l.offset;
                        if let Some(g) = l.grid_mut(frame) {
                            floptle_image::brush::flood_fill(
                                g,
                                x as i32 - off.0,
                                y as i32 - off.1,
                                color,
                                tol,
                                contig,
                                sel.as_ref(),
                                floptle_image::Blend::Mix,
                                1.0,
                            );
                        }
                    }
                }
                self.invalidate_all();
            }
            ImgTool::Wand => {
                self.push_undo();
                let (tol, contig) = (self.tolerance, self.contiguous);
                let frame = self.frame;
                let m = self.doc.as_ref().and_then(|d| {
                    let l = d.layers.get(d.active)?;
                    let g = l.grid(frame)?;
                    Some(floptle_image::select::wand_mask(
                        g,
                        x as i32 - l.offset.0,
                        y as i32 - l.offset.1,
                        tol,
                        contig,
                    ))
                });
                if let Some(m) = m {
                    self.apply_marquee(m);
                } else {
                    self.toast("the magic wand needs a pixel layer");
                }
            }
            ImgTool::Eyedropper => {
                if let Some(c) = self.pick_color(x, y) {
                    self.color = c;
                }
            }
            ImgTool::Pen => self.pen_click(x, y, shift),
            ImgTool::Text => self.begin_text(x, y),
            ImgTool::Transform => {
                if self.xform.is_none() {
                    self.begin_transform();
                } else if !self.grab_transform(x, y) {
                    self.commit_transform();
                }
            }
            ImgTool::Reshape => {
                // Clicking an edge inserts a node — the third of the three
                // Scratch gestures (drag a node, toggle it, add one).
                if self.hit_vector(x, y).is_none()
                    && let Some((path, seg, t)) = self.hit_segment(x, y)
                {
                    self.push_undo();
                    if let Some(p) = self.active_paths_mut().and_then(|ps| ps.get_mut(path)) {
                        let idx = p.insert_node(seg, t);
                        self.sel_node = Some((path, idx));
                    }
                    self.invalidate_vectors();
                }
            }
            t if t.is_select() => {
                // A bare click with a marquee tool clears the selection.
                self.push_undo();
                if let Some(d) = &mut self.doc {
                    d.selection = None;
                }
                self.ants_valid = false;
            }
            _ => {}
        }
    }

    fn canvas_size(&self) -> (u32, u32) {
        self.doc.as_ref().map_or((1, 1), |d| (d.w, d.h))
    }

    fn pick_color(&mut self, x: f32, y: f32) -> Option<[u8; 4]> {
        let (w, h) = self.canvas_size();
        let (xi, yi) = (x.floor() as i32, y.floor() as i32);
        if xi < 0 || yi < 0 || xi >= w as i32 || yi >= h as i32 {
            return None;
        }
        if self.flat_cache.is_none() {
            let doc = self.doc.as_ref()?;
            self.flat_cache = Some(composite::flatten(doc, self.frame));
        }
        let flat = self.flat_cache.as_ref()?;
        let o = (yi as usize * w as usize + xi as usize) * 4;
        Some([*flat.get(o)?, flat[o + 1], flat[o + 2], flat[o + 3]])
    }

    fn prune_active(&mut self) {
        let frame = self.frame;
        if let Some(doc) = &mut self.doc {
            let active = doc.active;
            if let Some(g) = doc.layers.get_mut(active).and_then(|l| l.grid_mut(frame)) {
                g.prune();
            }
        }
    }

    // --- shapes ------------------------------------------------------------

    fn shape_paths(&self, from: (f32, f32), to: (f32, f32)) -> Vec<VPath> {
        let stroke = VStroke {
            color: self.stroke_color2(),
            width: self.stroke_width,
            ..Default::default()
        };
        let mut p = match self.tool {
            ImgTool::Line => VPath::line(from.0, from.1, to.0, to.1, stroke.clone()),
            ImgTool::Rectangle => {
                let r = rect_between(from, to);
                VPath::rect(r.x as f32, r.y as f32, r.w as f32, r.h as f32)
            }
            ImgTool::Ellipse => {
                let r = rect_between(from, to);
                VPath::ellipse(
                    r.x as f32 + r.w as f32 * 0.5,
                    r.y as f32 + r.h as f32 * 0.5,
                    r.w as f32 * 0.5,
                    r.h as f32 * 0.5,
                )
            }
            _ => return Vec::new(),
        };
        if self.tool != ImgTool::Line {
            p.fill = self.shape_fill.then(|| Paint::Solid(self.stroke_color()));
            p.stroke = self.shape_stroke.then_some(stroke);
            if p.fill.is_none() && p.stroke.is_none() {
                p.fill = Some(Paint::Solid(self.stroke_color()));
            }
        }
        vec![p]
    }

    fn commit_shape(&mut self, from: (f32, f32), to: (f32, f32)) {
        let paths = self.shape_paths(from, to);
        if paths.is_empty() {
            return;
        }
        self.push_undo();
        let aa = !self.pixel_mode();
        if self.shape_vector {
            // Same tool, one modifier: spawn a vector layer instead of pixels.
            let name = format!("{} shape", self.tool.label().0);
            if let Some(doc) = &mut self.doc {
                let mut l = floptle_image::doc::Layer::vector(name);
                l.kind = LayerKind::Vector { paths };
                doc.add_layer(l);
            }
            self.invalidate_vectors();
            return;
        }
        let sel = self.doc.as_ref().and_then(|d| d.selection.clone());
        let frame = self.frame;
        if let Some(doc) = &mut self.doc {
            let canvas = (doc.w, doc.h);
            let active = doc.active;
            if let Some(l) = doc.layers.get_mut(active)
                && !l.locked
            {
                let off = l.offset;
                if let Some(g) = l.grid_mut(frame) {
                    floptle_image::brush::stamp_paths(
                        g,
                        &paths,
                        aa,
                        off,
                        canvas,
                        sel.as_ref(),
                        floptle_image::Blend::Mix,
                        1.0,
                    );
                } else {
                    self.status = Some(("shapes need a pixel layer".into(), 3.0));
                }
            }
        }
        self.invalidate_all();
    }

    // --- vector editing ----------------------------------------------------

    pub(crate) fn active_paths(&self) -> Option<&Vec<VPath>> {
        match &self.doc.as_ref()?.layers.get(self.doc.as_ref()?.active)?.kind {
            LayerKind::Vector { paths } => Some(paths),
            _ => None,
        }
    }

    fn active_paths_mut(&mut self) -> Option<&mut Vec<VPath>> {
        let doc = self.doc.as_mut()?;
        let active = doc.active;
        match &mut doc.layers.get_mut(active)?.kind {
            LayerKind::Vector { paths } => Some(paths),
            _ => None,
        }
    }

    /// (path, node, handle) under the cursor — `handle` is `Some(true)` for the
    /// incoming handle, `Some(false)` for outgoing, `None` for the node itself.
    fn hit_vector(&self, x: f32, y: f32) -> Option<(usize, usize, Option<bool>)> {
        let tol = (7.0 / self.zoom).max(1.5);
        let paths = self.active_paths()?;
        // Handles of the selected node win, so a handle on top of a node is grabbable.
        if let Some((pi, ni)) = self.sel_node
            && let Some(p) = paths.get(pi)
            && let Some(n) = p.nodes.get(ni)
            && n.kind == NodeKind::Curve
        {
            for (is_in, h) in [(true, n.h_in), (false, n.h_out)] {
                let (hx, hy) = (n.p[0] + h[0], n.p[1] + h[1]);
                if (hx - x).powi(2) + (hy - y).powi(2) <= tol * tol {
                    return Some((pi, ni, Some(is_in)));
                }
            }
        }
        for (pi, p) in paths.iter().enumerate() {
            if let Some(ni) = p.hit_node(x, y, tol) {
                return Some((pi, ni, None));
            }
        }
        None
    }

    fn hit_segment(&self, x: f32, y: f32) -> Option<(usize, usize, f32)> {
        let tol = (6.0 / self.zoom).max(1.5);
        let paths = self.active_paths()?;
        for (pi, p) in paths.iter().enumerate() {
            if let Some((seg, t)) = p.hit_segment(x, y, tol) {
                return Some((pi, seg, t));
            }
        }
        None
    }

    fn drag_vector_node(
        &mut self,
        path: usize,
        node: usize,
        handle: Option<bool>,
        x: f32,
        y: f32,
        shift: bool,
    ) {
        if let Some(p) = self.active_paths_mut().and_then(|ps| ps.get_mut(path))
            && let Some(n) = p.nodes.get_mut(node)
        {
            match handle {
                None => n.p = [x, y],
                Some(is_in) => {
                    let h = [x - n.p[0], y - n.p[1]];
                    if is_in {
                        n.h_in = h;
                        if !shift {
                            n.h_out = [-h[0], -h[1]]; // symmetric unless Shift breaks it
                        }
                    } else {
                        n.h_out = h;
                        if !shift {
                            n.h_in = [-h[0], -h[1]];
                        }
                    }
                    n.kind = NodeKind::Curve;
                }
            }
        }
        self.invalidate_vectors();
    }

    fn toggle_node_kind(&mut self, x: f32, y: f32) {
        let Some((pi, ni, _)) = self.hit_vector(x, y) else { return };
        self.push_undo();
        if let Some(p) = self.active_paths_mut().and_then(|ps| ps.get_mut(pi))
            && let Some(n) = p.nodes.get_mut(ni)
        {
            n.kind = if n.kind == NodeKind::Corner { NodeKind::Curve } else { NodeKind::Corner };
            // Dropping to a corner drops the handles too, or they'd come back.
            if n.kind == NodeKind::Corner {
                n.h_in = [0.0, 0.0];
                n.h_out = [0.0, 0.0];
            }
        }
        self.invalidate_vectors();
    }

    fn pen_click(&mut self, x: f32, y: f32, close: bool) {
        let mut p = self.pen.take().unwrap_or(VPath {
            nodes: Vec::new(),
            closed: false,
            fill: Some(Paint::Solid(self.color)),
            stroke: None,
            even_odd: false,
        });
        // Clicking the first node again (or Shift-clicking) closes the path.
        let near_start = p
            .nodes
            .first()
            .is_some_and(|n| (n.p[0] - x).abs() + (n.p[1] - y).abs() < (8.0 / self.zoom).max(2.0));
        if (close || near_start) && p.nodes.len() >= 3 {
            p.closed = true;
            self.commit_pen(p);
            return;
        }
        p.nodes.push(VNode::corner(x, y));
        self.pen = Some(p);
    }

    /// Finish the pen path onto a vector layer (creating one if needed).
    pub(crate) fn commit_pen(&mut self, p: VPath) {
        if p.nodes.len() < 2 {
            self.pen = None;
            return;
        }
        self.push_undo();
        if self.active_paths().is_some() {
            if let Some(ps) = self.active_paths_mut() {
                ps.push(p);
            }
        } else if let Some(doc) = &mut self.doc {
            let mut l = floptle_image::doc::Layer::vector("Vector");
            l.kind = LayerKind::Vector { paths: vec![p] };
            doc.add_layer(l);
        }
        self.pen = None;
        self.invalidate_vectors();
    }

    /// Escape: back out of whatever is in flight, newest first. Returns true
    /// when something was cancelled (so the editor's Escape chain stops here).
    pub(crate) fn cancel_pen(&mut self) -> bool {
        if self.pen.take().is_some() {
            return true;
        }
        if self.cancel_text() || self.cancel_transform() {
            return true;
        }
        if self.filter.is_some() {
            self.cancel_filter();
            return true;
        }
        false
    }

    /// The path at `i` on the active vector layer.
    pub(crate) fn vector_path_mut(&mut self, i: usize) -> Option<&mut VPath> {
        self.active_paths_mut()?.get_mut(i)
    }

    // --- destructive filters, with a live preview ---------------------------

    /// Start a filter preview on the active layer.
    pub(crate) fn begin_filter(&mut self, kind: FilterKind) {
        let ok = self
            .doc
            .as_ref()
            .and_then(|d| d.layers.get(d.active))
            .is_some_and(|l| l.kind.is_raster() && !l.locked);
        if !ok {
            self.toast("filters need an unlocked pixel layer");
            return;
        }
        let Some(base) = self.doc.clone() else { return };
        let (a, b) = kind.defaults();
        self.filter = Some(FilterState { kind, a, b, mono: false, base });
        self.apply_filter_preview();
    }

    pub(crate) fn set_filter_params(&mut self, a: f32, b: f32, mono: bool) {
        if let Some(f) = self.filter.as_mut() {
            f.a = a;
            f.b = b;
            f.mono = mono;
        }
        self.apply_filter_preview();
    }

    /// Re-run the filter from its snapshot. Every parameter change lands here,
    /// which is why the result never compounds.
    fn apply_filter_preview(&mut self) {
        let Some(f) = self.filter.clone() else { return };
        self.doc = Some(f.base.clone());
        let frame = self.frame;
        let Some(doc) = self.doc.as_mut() else { return };
        let sel = doc.selection.clone();
        // A tiling document blurs ACROSS the seam; a clamped blur would build a
        // bright rim exactly where the texture repeats.
        let tiling = doc.tiling;
        let active = doc.active;
        let Some(layer) = doc.layers.get_mut(active) else { return };
        let off = layer.offset;
        let Some(g) = layer.grid_mut(frame) else { return };
        let (gw, gh) = (g.width(), g.height());
        let before = g.to_rgba();
        let mut buf = before.clone();
        match f.kind {
            FilterKind::Blur => floptle_image::filter::blur(&mut buf, gw, gh, f.a, tiling),
            FilterKind::Sharpen => floptle_image::filter::sharpen(&mut buf, gw, gh, f.a, f.b),
            FilterKind::Noise => floptle_image::filter::noise(&mut buf, gw, gh, f.a, f.mono, 1),
            FilterKind::Pixelate => floptle_image::filter::pixelate(&mut buf, gw, gh, f.a as u32),
            FilterKind::Offset => floptle_image::filter::offset_wrap(
                &mut buf,
                gw,
                gh,
                (f.a * gw as f32) as i32,
                (f.b * gh as f32) as i32,
            ),
            FilterKind::Seamless => floptle_image::filter::seamless(&mut buf, gw, gh, f.a as u32),
            FilterKind::NormalMap => {
                floptle_image::filter::normal_from_height(&mut buf, gw, gh, f.a, true)
            }
        }
        // Write back, weighted by the selection (canvas space → layer space).
        g.edit_rect(Rect::size(gw, gh), |x, y, px| {
            let k = sel.as_ref().map_or(1.0, |s| s.at(x + off.0, y + off.1));
            if k <= 0.0 {
                return;
            }
            let o = (y as usize * gw as usize + x as usize) * 4;
            if o + 4 > buf.len() {
                return;
            }
            for i in 0..4 {
                px[i] = floptle_image::u8c(
                    before[o + i] as f32 + (buf[o + i] as f32 - before[o + i] as f32) * k,
                );
            }
        });
        self.invalidate_all();
    }

    pub(crate) fn commit_filter(&mut self) {
        let Some(f) = self.filter.take() else { return };
        // The undo step is the document as it was BEFORE the preview started.
        self.undo.push(f.base);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.mark_dirty();
        self.toast(format!("{} applied", f.kind.label()));
    }

    pub(crate) fn cancel_filter(&mut self) {
        let Some(f) = self.filter.take() else { return };
        self.doc = Some(f.base);
        self.invalidate_all();
    }

    // --- free transform -----------------------------------------------------

    /// Lift the selection (or, with none, the active layer's painted content)
    /// A press at `(x, y)` that lands inside the live selection: arm the
    /// transform on it and grab it, so the drag that follows moves it.
    ///
    /// Returns whether the press was claimed. `false` means "there was nothing
    /// under you" and the caller goes on to start a new marquee.
    fn press_moves_selection(&mut self, x: f32, y: f32) -> bool {
        let inside = self
            .doc
            .as_ref()
            .and_then(|d| d.selection.as_ref())
            .is_some_and(|m| m.at(x.floor() as i32, y.floor() as i32) > 0.0);
        if !inside || !self.begin_transform() {
            return false;
        }
        // The transform is modal until committed, and the tool follows it —
        // the same thing a paste already does, so the two arrive in the same
        // state rather than two states that look alike.
        self.tool = ImgTool::Transform;
        self.grab_transform(x, y);
        true
    }

    /// Duplicate the selection in place and float the copy, ready to be dragged.
    ///
    /// Not a copy-then-paste: that costs two actions and destroys whatever was
    /// on the clipboard, so stamping the same bit of art repeatedly meant
    /// re-copying it every time (`floptle/0095`). `lift: false` is the whole
    /// difference from a move — the original stays where it is.
    pub(crate) fn duplicate_selection(&mut self) -> bool {
        if self.xform.is_some() {
            self.commit_transform();
        }
        if !self.begin_transform() {
            return false;
        }
        if let Some(sess) = self.xform.as_mut() {
            sess.lift = false;
            // Re-lay it from the untouched base, or the original stays lifted
            // from the preview that ran when the session opened.
            sess.base = self.doc.clone().unwrap_or_else(|| sess.base.clone());
        }
        self.tool = ImgTool::Transform;
        self.apply_xform_preview();
        self.toast("duplicated — drag to place, Enter to apply");
        true
    }

    /// into a transform session. Returns false when there's nothing to lift.
    pub(crate) fn begin_transform(&mut self) -> bool {
        if self.xform.is_some() {
            return true;
        }
        let frame = self.frame;
        let Some(doc) = self.doc.as_ref() else { return false };
        let layer = doc.layers.get(doc.active);
        let Some(layer) = layer else { return false };
        if layer.locked || !layer.kind.is_raster() {
            self.toast("free transform needs an unlocked pixel layer");
            return false;
        }
        let off = layer.offset;
        let Some(grid) = layer.grid(frame) else { return false };
        // The selection decides what moves; with none, everything painted does.
        let rect = match doc.selection.as_ref() {
            Some(sel) => sel.selected_bounds(),
            None => {
                let b = grid.opaque_bounds();
                Rect::new(b.x + off.0, b.y + off.1, b.w, b.h)
            }
        }
        .intersect(doc.bounds());
        if rect.is_empty() {
            self.toast("nothing to transform on this layer");
            return false;
        }
        // Read the pixels in canvas space, masked by the selection.
        let mut src = grid.read_rect(Rect::new(rect.x - off.0, rect.y - off.1, rect.w, rect.h));
        if let Some(sel) = doc.selection.as_ref() {
            for y in 0..rect.h as i32 {
                for x in 0..rect.w as i32 {
                    let o = (y as usize * rect.w as usize + x as usize) * 4;
                    let k = sel.at(rect.x + x, rect.y + y);
                    src[o + 3] = floptle_image::u8c(src[o + 3] as f32 * k);
                }
            }
        }
        let base = doc.clone();
        self.xform = Some(XformSession {
            base,
            src,
            rect,
            xf: floptle_image::transform::Xform {
                pivot: (rect.x as f32 + rect.w as f32 / 2.0, rect.y as f32 + rect.h as f32 / 2.0),
                ..Default::default()
            },
            grab: None,
            lift: true,
        });
        self.apply_xform_preview();
        true
    }

    /// The four transformed corners, canvas space, clockwise from top-left.
    pub(crate) fn xform_corners(&self) -> Option<[(f32, f32); 4]> {
        let s = self.xform.as_ref()?;
        let r = s.rect;
        let (x0, y0) = (r.x as f32, r.y as f32);
        let (x1, y1) = (r.right() as f32, r.bottom() as f32);
        Some([
            s.xf.apply(x0, y0),
            s.xf.apply(x1, y0),
            s.xf.apply(x1, y1),
            s.xf.apply(x0, y1),
        ])
    }

    /// Where the rotate handle sits (above the top edge).
    pub(crate) fn xform_rotate_handle(&self) -> Option<(f32, f32)> {
        let c = self.xform_corners()?;
        let mid = ((c[0].0 + c[1].0) * 0.5, (c[0].1 + c[1].1) * 0.5);
        let bot = ((c[3].0 + c[2].0) * 0.5, (c[3].1 + c[2].1) * 0.5);
        let (dx, dy) = (mid.0 - bot.0, mid.1 - bot.1);
        let len = (dx * dx + dy * dy).sqrt().max(1e-3);
        let reach = (22.0 / self.zoom.max(0.05)).max(4.0);
        Some((mid.0 + dx / len * reach, mid.1 + dy / len * reach))
    }

    /// Decide what a press at (x, y) grabs. Returns false if it fell outside,
    /// which the caller treats as "commit and move on".
    fn grab_transform(&mut self, x: f32, y: f32) -> bool {
        let tol = (9.0 / self.zoom.max(0.05)).max(1.5);
        let corners = match self.xform_corners() {
            Some(c) => c,
            None => return false,
        };
        let rot = self.xform_rotate_handle();
        let Some(s) = self.xform.as_mut() else { return false };
        let pivot = (s.xf.pivot.0 + s.xf.translate.0, s.xf.pivot.1 + s.xf.translate.1);
        if let Some(r) = rot
            && (r.0 - x).hypot(r.1 - y) <= tol
        {
            s.grab = Some(XformGrab::Rotate {
                from: (y - pivot.1).atan2(x - pivot.0),
                rotate: s.xf.rotate,
            });
            return true;
        }
        let half = (s.rect.w as f32 / 2.0, s.rect.h as f32 / 2.0);
        for (i, c) in corners.iter().enumerate() {
            if (c.0 - x).hypot(c.1 - y) <= tol {
                let local = match i {
                    0 => (-half.0, -half.1),
                    1 => (half.0, -half.1),
                    2 => (half.0, half.1),
                    _ => (-half.0, half.1),
                };
                s.grab = Some(XformGrab::Scale { local, scale: s.xf.scale });
                return true;
            }
        }
        if point_in_quad((x, y), &corners) {
            s.grab = Some(XformGrab::Move { from: (x, y), translate: s.xf.translate });
            return true;
        }
        false
    }

    /// Continue the grabbed handle. `shift` constrains (uniform scale, 15° rotation).
    fn drag_transform(&mut self, x: f32, y: f32, shift: bool) {
        let Some(s) = self.xform.as_mut() else { return };
        let Some(grab) = s.grab else { return };
        match grab {
            XformGrab::Move { from, translate } => {
                s.xf.translate = (translate.0 + x - from.0, translate.1 + y - from.1);
            }
            XformGrab::Scale { local, scale } => {
                // Work in the pre-rotation frame, so scaling a rotated box still
                // follows the corner you're holding.
                let pivot = (s.xf.pivot.0 + s.xf.translate.0, s.xf.pivot.1 + s.xf.translate.1);
                let (dx, dy) = (x - pivot.0, y - pivot.1);
                let (c, sn) = ((-s.xf.rotate).cos(), (-s.xf.rotate).sin());
                let (lx, ly) = (dx * c - dy * sn, dx * sn + dy * c);
                let mut sx = if local.0.abs() > 0.5 { lx / local.0 } else { scale.0 };
                let mut sy = if local.1.abs() > 0.5 { ly / local.1 } else { scale.1 };
                if shift {
                    let m = sx.abs().max(sy.abs());
                    sx = m * sx.signum();
                    sy = m * sy.signum();
                }
                s.xf.scale = (sx.clamp(-64.0, 64.0), sy.clamp(-64.0, 64.0));
            }
            XformGrab::Rotate { from, rotate } => {
                let pivot = (s.xf.pivot.0 + s.xf.translate.0, s.xf.pivot.1 + s.xf.translate.1);
                let now = (y - pivot.1).atan2(x - pivot.0);
                let mut a = rotate + (now - from);
                if shift {
                    let step = std::f32::consts::PI / 12.0;
                    a = (a / step).round() * step;
                }
                s.xf.rotate = a;
            }
        }
        self.apply_xform_preview();
    }

    /// Redraw the document from the snapshot with the current transform applied.
    /// Re-lay the floating transform from its snapshot after `xf` changed by
    /// some route other than a drag (the numeric editor). Same apply, so the
    /// two cannot land differently.
    pub(crate) fn reapply_transform(&mut self) {
        self.apply_xform_preview();
    }

    fn apply_xform_preview(&mut self) {
        let Some(sess) = self.xform.clone() else { return };
        self.doc = Some(sess.base.clone());
        let frame = self.frame;
        let nearest = self.pixel_mode();
        let Some(doc) = self.doc.as_mut() else { return };
        let sel = doc.selection.clone();
        let active = doc.active;
        let Some(layer) = doc.layers.get_mut(active) else { return };
        let off = layer.offset;
        let Some(grid) = layer.grid_mut(frame) else { return };

        // 1. Lift: the source region leaves the layer. A PASTE skips this — its
        //    pixels came from the clipboard, and clearing the box they happen to
        //    land in would delete whatever was already there.
        if sess.lift {
            let clear =
                Rect::new(sess.rect.x - off.0, sess.rect.y - off.1, sess.rect.w, sess.rect.h);
            grid.edit_rect(clear, |lx, ly, px| {
                let k = sel.as_ref().map_or(1.0, |m| m.at(lx + off.0, ly + off.1));
                px[3] = floptle_image::u8c(px[3] as f32 * (1.0 - k));
            });
        }

        // 2. Land: only the destination box is touched, so a big canvas costs
        //    the size of the thing being moved, not the size of the document.
        let mut dest = Rect::EMPTY;
        let r = sess.rect;
        for (cx, cy) in [
            sess.xf.apply(r.x as f32, r.y as f32),
            sess.xf.apply(r.right() as f32, r.y as f32),
            sess.xf.apply(r.right() as f32, r.bottom() as f32),
            sess.xf.apply(r.x as f32, r.bottom() as f32),
        ] {
            dest = dest.union(Rect::new(cx.floor() as i32 - 1, cy.floor() as i32 - 1, 3, 3));
        }
        let dest = Rect::new(dest.x - off.0, dest.y - off.1, dest.w, dest.h);
        let (sw, sh) = (sess.rect.w, sess.rect.h);
        grid.edit_rect(dest, |lx, ly, px| {
            let (cx, cy) = (lx as f32 + off.0 as f32 + 0.5, ly as f32 + off.1 as f32 + 0.5);
            let (ux, uy) = sess.xf.inverse(cx, cy);
            let (fx, fy) = (ux - sess.rect.x as f32, uy - sess.rect.y as f32);
            if fx < 0.0 || fy < 0.0 || fx >= sw as f32 || fy >= sh as f32 {
                return;
            }
            let s = if nearest {
                sample_nearest(&sess.src, sw, sh, fx, fy)
            } else {
                sample_bilinear(&sess.src, sw, sh, fx, fy)
            };
            if s[3] == 0 {
                return;
            }
            *px = floptle_image::blend::over(*px, s, floptle_image::Blend::Mix, 1.0);
        });
        self.invalidate_all();
    }

    /// Set the transform from the panel's numeric fields.
    pub(crate) fn set_xform(&mut self, xf: floptle_image::transform::Xform) {
        if let Some(s) = self.xform.as_mut() {
            s.xf = xf;
        }
        self.apply_xform_preview();
    }

    pub(crate) fn commit_transform(&mut self) {
        let Some(s) = self.xform.take() else { return };
        self.undo.push(s.base);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.mark_dirty();
        self.toast("transform applied");
    }

    pub(crate) fn cancel_transform(&mut self) -> bool {
        let Some(s) = self.xform.take() else { return false };
        self.doc = Some(s.base);
        self.invalidate_all();
        true
    }

    // --- text ----------------------------------------------------------------

    /// Start (or move) a text block at a canvas point.
    pub(crate) fn begin_text(&mut self, x: f32, y: f32) {
        let ok = self
            .doc
            .as_ref()
            .and_then(|d| d.layers.get(d.active))
            .is_some_and(|l| l.kind.is_raster() && !l.locked);
        if !ok {
            self.toast("text needs an unlocked pixel layer");
            return;
        }
        match self.text.as_mut() {
            Some(t) => {
                t.at = (x.floor(), y.floor());
                t.dirty = true;
            }
            None => {
                let Some(base) = self.doc.clone() else { return };
                self.text = Some(TextSession {
                    base,
                    at: (x.floor(), y.floor()),
                    text: String::new(),
                    size: if self.pixel_mode() { 12.0 } else { 48.0 },
                    bitmap: None,
                    dirty: true,
                    focus: true,
                });
            }
        }
    }

    /// Set the text and size from the panel.
    pub(crate) fn set_text(&mut self, text: String, size: f32) {
        if let Some(t) = self.text.as_mut() {
            t.text = text;
            t.size = size;
            t.dirty = true;
        }
    }

    pub(crate) fn text_needs_render(&self) -> bool {
        self.text.as_ref().is_some_and(|t| t.dirty)
    }

    /// True exactly once per text block: the frame its field should grab the
    /// keyboard. Asking on every frame traps focus in the field forever.
    pub(crate) fn take_text_focus(&mut self) -> bool {
        match self.text.as_mut() {
            Some(t) if t.focus => {
                t.focus = false;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn mark_text_dirty(&mut self) {
        if let Some(t) = self.text.as_mut() {
            t.dirty = true;
        }
    }

    /// Rasterize the text through the editor's own font stack and drop it into
    /// the document. Needs an egui `Context` (the font atlas lives there), so
    /// the tab calls this rather than the canvas.
    pub(crate) fn render_text(&mut self, ctx: &egui::Context) {
        let Some(t) = self.text.as_ref() else { return };
        if !t.dirty {
            return;
        }
        let (text, size) = (t.text.clone(), t.size);
        let color = self.stroke_color();
        let bitmap = if text.trim().is_empty() {
            None
        } else {
            rasterize_text(ctx, &text, size, color)
        };
        if let Some(t) = self.text.as_mut() {
            t.bitmap = bitmap;
            t.dirty = false;
        }
        self.apply_text_preview();
    }

    fn apply_text_preview(&mut self) {
        let Some(sess) = self.text.clone() else { return };
        self.doc = Some(sess.base.clone());
        let Some((bmp, bw, bh)) = sess.bitmap else {
            self.invalidate_all();
            return;
        };
        let frame = self.frame;
        let hard = self.pixel_mode();
        let Some(doc) = self.doc.as_mut() else { return };
        let sel = doc.selection.clone();
        let active = doc.active;
        let Some(layer) = doc.layers.get_mut(active) else { return };
        let off = layer.offset;
        let Some(grid) = layer.grid_mut(frame) else { return };
        let at = (sess.at.0.round() as i32, sess.at.1.round() as i32);
        let dest = Rect::new(at.0 - off.0, at.1 - off.1, bw, bh);
        grid.edit_rect(dest, |lx, ly, px| {
            let (bx, by) = (lx - dest.x, ly - dest.y);
            if bx < 0 || by < 0 || bx >= bw as i32 || by >= bh as i32 {
                return;
            }
            let o = (by as usize * bw as usize + bx as usize) * 4;
            let mut s = [bmp[o], bmp[o + 1], bmp[o + 2], bmp[o + 3]];
            if hard {
                // Pixel mode: text is crisp or it isn't there.
                s[3] = if s[3] >= 128 { 255 } else { 0 };
            }
            if s[3] == 0 {
                return;
            }
            let k = sel.as_ref().map_or(1.0, |m| m.at(lx + off.0, ly + off.1));
            if k <= 0.0 {
                return;
            }
            *px = floptle_image::blend::over(*px, s, floptle_image::Blend::Mix, k);
        });
        self.invalidate_all();
    }

    pub(crate) fn commit_text(&mut self) {
        let Some(t) = self.text.take() else { return };
        if t.bitmap.is_none() {
            // Nothing was typed — restore and say nothing.
            self.doc = Some(t.base);
            self.invalidate_all();
            return;
        }
        self.undo.push(t.base);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.mark_dirty();
        self.toast("text applied");
    }

    pub(crate) fn cancel_text(&mut self) -> bool {
        let Some(t) = self.text.take() else { return false };
        self.doc = Some(t.base);
        self.invalidate_all();
        true
    }

    /// The text block's on-canvas rect, for the overlay.
    pub(crate) fn text_rect(&self) -> Option<Rect> {
        let t = self.text.as_ref()?;
        let (_, w, h) = t.bitmap.as_ref().map(|(b, w, h)| (b, *w, *h)).unwrap_or((&Vec::new(), 1, 1));
        Some(Rect::new(t.at.0 as i32, t.at.1 as i32, w.max(1), h.max(1)))
    }

    // --- painting the canvas ------------------------------------------------

    /// Recomposite whatever is pending and push it to the egui texture.
    fn sync_texture(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        let (w, h) = (doc.w, doc.h);
        let nearest = doc.mode == Mode::Pixel || self.zoom >= 1.0;
        let opts = if nearest { egui::TextureOptions::NEAREST } else { egui::TextureOptions::LINEAR };
        // A resize or a sampler change rebuilds the whole texture.
        if self.tex.is_none() || self.tex_size != (w, h) || self.tex_nearest != nearest {
            let flat = composite::flatten(doc, self.frame);
            let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &flat);
            self.tex = Some(ctx.load_texture("flimg-canvas", img, opts));
            self.tex_size = (w, h);
            self.tex_nearest = nearest;
            self.pending = None;
            return;
        }
        // The onion skin is the previous frame; rebuild it only when the frame
        // or the document changed (invalidate_all drops it).
        if self.onion && doc.frames > 1 && self.onion_tex.is_none() {
            let prev = (self.frame + doc.frames - 1) % doc.frames;
            if prev != self.frame {
                let flat = composite::flatten(doc, prev);
                let img =
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &flat);
                self.onion_tex = Some(ctx.load_texture("flimg-onion", img, opts));
            }
        }
        if !self.onion && self.onion_tex.is_some() {
            self.onion_tex = None;
        }
        let Some(mut r) = self.pending.take() else { return };
        // Floyd–Steinberg (and anything else canvas-wide) can't be redrawn a
        // rect at a time — widen rather than tear.
        if composite::needs_full_canvas(doc) {
            r = doc.bounds();
        }
        let r = r.intersect(doc.bounds());
        if r.is_empty() {
            return;
        }
        let buf = composite::composite_rect(doc, self.frame, r, &mut self.vcache);
        let img = egui::ColorImage::from_rgba_unmultiplied([r.w as usize, r.h as usize], &buf);
        if let Some(t) = self.tex.as_mut() {
            t.set_partial([r.x.max(0) as usize, r.y.max(0) as usize], img, opts);
        }
    }

    fn paint(&mut self, ui: &mut egui::Ui, view: ERect) {
        let Some(doc) = &self.doc else { return };
        let (w, h) = (doc.w as f32, doc.h as f32);
        let p = ui.painter_at(view);
        let tl = self.to_screen(view, 0.0, 0.0);
        let br = self.to_screen(view, w, h);
        let img_rect = ERect::from_min_max(tl, br);

        // Transparency checker, only under the canvas itself.
        if self.look.checker {
            draw_checker(&p, img_rect.intersect(view), &self.look);
        }
        let Some(tex) = &self.tex else { return };
        let tint = Color32::WHITE;
        let uv = ERect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        // Onion skin: the PREVIOUS frame, ghosted, underneath. Composited once
        // per frame change (sync_texture), not once per paint.
        if let Some(o) = &self.onion_tex {
            p.image(o.id(), img_rect, uv, Color32::from_white_alpha(90));
        }
        if self.tiled_view {
            // Neighbours are live mirrors of the same texture: this is how you
            // see the repeat while you work.
            for ty in -1..=1i32 {
                for tx in -1..=1i32 {
                    let o = Vec2::new(tx as f32 * w * self.zoom, ty as f32 * h * self.zoom);
                    let r = img_rect.translate(o);
                    if !r.intersects(view) {
                        continue;
                    }
                    let t = if tx == 0 && ty == 0 { tint } else { Color32::from_white_alpha(190) };
                    p.image(tex.id(), r, uv, t);
                }
            }
            p.rect_stroke(
                img_rect,
                0.0,
                EStroke::new(1.0, Color32::from_rgb(120, 190, 255).gamma_multiply(0.8)),
                egui::StrokeKind::Outside,
            );
        } else {
            p.image(tex.id(), img_rect, uv, tint);
        }

        // Canvas border.
        p.rect_stroke(
            img_rect,
            0.0,
            EStroke::new(1.0, ui.visuals().widgets.noninteractive.fg_stroke.color.gamma_multiply(0.5)),
            egui::StrokeKind::Outside,
        );

        // Pixel grid, once a texel is comfortably bigger than a screen pixel.
        if self.look.pixel_grid && self.zoom >= self.look.pixel_grid_zoom.max(1.0) {
            let l = &self.look;
            let c = rgba(l.pixel_grid_color, l.pixel_grid_alpha);
            // Two-tone: a light line with dark dashes over it, so one of the two
            // shows against a background of any colour. This is the part that
            // does not need configuring to be legible; the colours above are the
            // escape hatch for when it still is not.
            let dark = Color32::from_black_alpha(l.pixel_grid_alpha);
            let dash = |p: &egui::Painter, a: Pos2, b: Pos2| {
                p.line_segment([a, b], EStroke::new(1.0, c));
                if l.pixel_grid_two_tone {
                    p.add(egui::Shape::dashed_line(&[a, b], EStroke::new(1.0, dark), 3.0, 3.0));
                }
            };
            let mut x = 0.0;
            while x <= w {
                let sx = self.to_screen(view, x, 0.0).x;
                dash(&p, Pos2::new(sx, img_rect.top()), Pos2::new(sx, img_rect.bottom()));
                x += 1.0;
            }
            let mut y = 0.0;
            while y <= h {
                let sy = self.to_screen(view, 0.0, y).y;
                dash(&p, Pos2::new(img_rect.left(), sy), Pos2::new(img_rect.right(), sy));
                y += 1.0;
            }
        }

        // The SHEET's cell grid — heavier than the pixel grid, a different
        // colour, and drawn after it so it wins where they coincide. This is the
        // grid a tileset is actually cut on, and drawing it is the difference
        // between laying out a sheet and counting texels by hand
        // (`floptle/0096`).
        if self.look.cell_grid
            && let Some((cw, ch)) = doc.cell_size()
        {
            let l = &self.look;
            let c = rgba(l.cell_grid_color, l.cell_grid_alpha);
            // A cell smaller than a few screen pixels is a solid block of lines,
            // which hides the art the grid exists to help you place.
            let step = (cw.min(ch) as f32) * self.zoom;
            if step >= 5.0 {
                let mut x = 0.0;
                while x <= w {
                    let sx = self.to_screen(view, x, 0.0).x;
                    p.line_segment(
                        [Pos2::new(sx, img_rect.top()), Pos2::new(sx, img_rect.bottom())],
                        EStroke::new(1.0, c),
                    );
                    x += cw as f32;
                }
                let mut y = 0.0;
                while y <= h {
                    let sy = self.to_screen(view, 0.0, y).y;
                    p.line_segment(
                        [Pos2::new(img_rect.left(), sy), Pos2::new(img_rect.right(), sy)],
                        EStroke::new(1.0, c),
                    );
                    y += ch as f32;
                }
            }
        }

        self.paint_overlays(ui, view, img_rect);
    }

    fn paint_overlays(&mut self, ui: &mut egui::Ui, view: ERect, img_rect: ERect) {
        let p = ui.painter_at(view);
        // --- selection marching ants ---
        if !self.ants_valid {
            self.rebuild_ants();
        }
        if !self.ants.is_empty() {
            let c = Color32::from_rgb(255, 255, 255);
            let d = Color32::from_rgb(20, 20, 20);
            for seg in &self.ants {
                let a = self.to_screen(view, seg[0].0, seg[0].1);
                let b = self.to_screen(view, seg[1].0, seg[1].1);
                p.line_segment([a, b], EStroke::new(2.0, d));
                p.line_segment([a, b], EStroke::new(1.0, c));
            }
        }

        // --- in-flight gesture previews ---
        match (&self.drag, self.cursor) {
            (Some(Drag::Box { from }), Some(cur)) => {
                let shift = ui.input(|i| i.modifiers.shift);
                let to = constrain(*from, cur, shift, self.tool == ImgTool::Line);
                let a = self.to_screen(view, from.0, from.1);
                let b = self.to_screen(view, to.0, to.1);
                let col = ui.visuals().selection.stroke.color;
                match self.tool {
                    ImgTool::Line => {
                        p.line_segment([a, b], EStroke::new(1.5, col));
                    }
                    ImgTool::Ellipse | ImgTool::SelectEllipse => {
                        let r = ERect::from_two_pos(a, b);
                        p.circle_stroke(r.center(), r.width().min(r.height()) * 0.5, EStroke::new(1.0, col));
                        p.rect_stroke(r, 0.0, EStroke::new(1.0, col.gamma_multiply(0.4)), egui::StrokeKind::Inside);
                    }
                    _ => {
                        p.rect_stroke(
                            ERect::from_two_pos(a, b),
                            0.0,
                            EStroke::new(1.0, col),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
            }
            (Some(Drag::Gradient { from }), Some(cur)) => {
                let shift = ui.input(|i| i.modifiers.shift);
                let to = constrain(*from, cur, shift, true);
                let a = self.to_screen(view, from.0, from.1);
                let b = self.to_screen(view, to.0, to.1);
                p.line_segment([a, b], EStroke::new(1.5, Color32::WHITE));
                p.circle_filled(a, 3.0, Color32::from_rgba_unmultiplied(self.color[0], self.color[1], self.color[2], 255));
                p.circle_filled(b, 3.0, Color32::from_rgba_unmultiplied(self.color2[0], self.color2[1], self.color2[2], 255));
            }
            (Some(Drag::Lasso), _) => {
                let col = ui.visuals().selection.stroke.color;
                for w in self.lasso.windows(2) {
                    let a = self.to_screen(view, w[0].0, w[0].1);
                    let b = self.to_screen(view, w[1].0, w[1].1);
                    p.line_segment([a, b], EStroke::new(1.0, col));
                }
            }
            _ => {}
        }

        // --- vector nodes (reshape) ---
        if matches!(self.tool, ImgTool::Reshape | ImgTool::Pen)
            && let Some(paths) = self.active_paths()
        {
            let sel = self.sel_node;
            for (pi, path) in paths.iter().enumerate() {
                // The outline, so a path with no fill is still grabbable.
                let flat = path.flatten();
                let col = Color32::from_rgb(120, 190, 255).gamma_multiply(0.7);
                for i in 0..flat.len().saturating_sub(usize::from(!path.closed)) {
                    let a = flat[i];
                    let b = flat[(i + 1) % flat.len()];
                    p.line_segment(
                        [self.to_screen(view, a.0, a.1), self.to_screen(view, b.0, b.1)],
                        EStroke::new(1.0, col),
                    );
                }
                for (ni, n) in path.nodes.iter().enumerate() {
                    let s = self.to_screen(view, n.p[0], n.p[1]);
                    let selected = sel == Some((pi, ni));
                    let fill = if n.kind == NodeKind::Curve {
                        Color32::from_rgb(120, 210, 255)
                    } else {
                        Color32::from_rgb(255, 210, 120)
                    };
                    if n.kind == NodeKind::Curve {
                        p.circle(s, 4.0, fill, EStroke::new(1.0, Color32::BLACK));
                    } else {
                        p.rect_filled(ERect::from_center_size(s, Vec2::splat(7.0)), 1.0, fill);
                    }
                    if selected {
                        p.circle_stroke(s, 7.0, EStroke::new(1.5, Color32::WHITE));
                        if n.kind == NodeKind::Curve {
                            for h in [n.h_in, n.h_out] {
                                let hs = self.to_screen(view, n.p[0] + h[0], n.p[1] + h[1]);
                                p.line_segment([s, hs], EStroke::new(1.0, Color32::from_white_alpha(140)));
                                p.circle_filled(hs, 3.0, Color32::WHITE);
                            }
                        }
                    }
                }
            }
        }
        // The pen's in-progress path.
        if let Some(pen) = &self.pen {
            let col = Color32::from_rgb(255, 210, 120);
            for w in pen.nodes.windows(2) {
                p.line_segment(
                    [
                        self.to_screen(view, w[0].p[0], w[0].p[1]),
                        self.to_screen(view, w[1].p[0], w[1].p[1]),
                    ],
                    EStroke::new(1.5, col),
                );
            }
            if let (Some(last), Some(cur)) = (pen.nodes.last(), self.cursor) {
                p.line_segment(
                    [
                        self.to_screen(view, last.p[0], last.p[1]),
                        self.to_screen(view, cur.0, cur.1),
                    ],
                    EStroke::new(1.0, col.gamma_multiply(0.5)),
                );
            }
            for n in &pen.nodes {
                p.circle_filled(self.to_screen(view, n.p[0], n.p[1]), 3.5, col);
            }
        }

        // --- free transform box ---
        if let Some(corners) = self.xform_corners() {
            let col = Color32::from_rgb(120, 190, 255);
            let pts: Vec<Pos2> =
                corners.iter().map(|c| self.to_screen(view, c.0, c.1)).collect();
            for i in 0..4 {
                p.line_segment([pts[i], pts[(i + 1) % 4]], EStroke::new(1.5, col));
            }
            for q in &pts {
                p.rect_filled(ERect::from_center_size(*q, Vec2::splat(8.0)), 1.0, col);
                p.rect_stroke(
                    ERect::from_center_size(*q, Vec2::splat(8.0)),
                    1.0,
                    EStroke::new(1.0, Color32::BLACK),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some(r) = self.xform_rotate_handle() {
                let rs = self.to_screen(view, r.0, r.1);
                let mid = Pos2::new((pts[0].x + pts[1].x) * 0.5, (pts[0].y + pts[1].y) * 0.5);
                p.line_segment([mid, rs], EStroke::new(1.0, col));
                p.circle(rs, 5.0, col, EStroke::new(1.0, Color32::BLACK));
            }
        }

        // --- text block ---
        if let Some(r) = self.text_rect() {
            let a = self.to_screen(view, r.x as f32, r.y as f32);
            let b = self.to_screen(view, r.right() as f32, r.bottom() as f32);
            let col = Color32::from_rgb(255, 210, 120);
            p.rect_stroke(
                ERect::from_two_pos(a, b),
                0.0,
                EStroke::new(1.0, col.gamma_multiply(0.8)),
                egui::StrokeKind::Outside,
            );
            // A caret at the anchor, so an empty block is still visible.
            p.line_segment([a, Pos2::new(a.x, b.y.max(a.y + 8.0))], EStroke::new(1.5, col));
        }

        // --- symmetry axes ---
        if self.mirror_x || self.mirror_y {
            let c = Color32::from_rgb(255, 120, 200).gamma_multiply(0.6);
            if self.mirror_x {
                let x = img_rect.center().x;
                p.line_segment([Pos2::new(x, img_rect.top()), Pos2::new(x, img_rect.bottom())], EStroke::new(1.0, c));
            }
            if self.mirror_y {
                let y = img_rect.center().y;
                p.line_segment([Pos2::new(img_rect.left(), y), Pos2::new(img_rect.right(), y)], EStroke::new(1.0, c));
            }
        }

        // --- brush telegraph ---
        // Drawn DURING a stroke as well — a brush you can't see the size of
        // halfway through a stroke is a brush you're guessing with.
        //
        // And drawn from the brush's OWN footprint, so what you see outlined is
        // the set of texels that will change. The circle this used to draw was
        // re-derived from `radius` and was wrong for every brush that is not a
        // smooth disc — most visibly the one-pixel pencil, which showed a small
        // circle floating between texels (`floptle/0094`).
        if let Some((cx, cy)) = self.cursor
            && self.tool.is_paint()
        {
            self.draw_brush_telegraph(&p, view, cx, cy);
        }
        // Clone source marker.
        if let Some((sx, sy)) = self.clone_src
            && self.brush.mode == BrushMode::Clone
        {
            let s = self.to_screen(view, sx, sy);
            p.circle_stroke(s, 5.0, EStroke::new(1.0, Color32::from_rgb(255, 200, 100)));
        }
    }

    /// The cursor telegraph: the outline of the texels this dab would touch.
    ///
    /// Two contours rather than one, because a brush has two interesting edges
    /// and conflating them is what made the old circle a lie:
    ///
    /// * the **half-coverage** contour — where the brush actually is. For a
    ///   pixel brush this is the exact set of texels that change, drawn on the
    ///   texel grid, so preview and result are the same shape in the same place.
    /// * the **outer reach**, drawn faintly and only when the brush is soft. A
    ///   soft brush must not claim a hard edge it does not have, and a hard one
    ///   must not be given a halo it does not have either.
    ///
    /// Both are drawn light-over-dark so they survive art of any colour without
    /// being configured for it.
    fn draw_brush_telegraph(&self, p: &egui::Painter, view: ERect, cx: f32, cy: f32) {
        let light = Color32::from_white_alpha(190);
        let dark = Color32::from_black_alpha(130);
        let soft = !self.brush.pixel_perfect && self.brush.hardness < 0.99;

        // Below a couple of screen pixels per texel the per-texel outline is
        // noise, and a very large brush is a circle whatever its texels say.
        let per_texel = self.zoom;
        if per_texel < 2.0 || !self.brush.footprint_is_cheap() {
            let s = self.to_screen(view, cx, cy);
            let r = (self.brush.radius * self.zoom).max(2.0);
            p.circle_stroke(s, r + 1.0, EStroke::new(1.0, dark));
            p.circle_stroke(s, r, EStroke::new(1.0, light));
            return;
        }

        let (rect, cov) = self.brush.footprint(cx, cy);
        let at = |col: i32, row: i32| -> f32 {
            if col < 0 || row < 0 || col >= rect.w as i32 || row >= rect.h as i32 {
                return 0.0;
            }
            cov[row as usize * rect.w as usize + col as usize]
        };
        // The boundary of a threshold set: every edge a covered texel does not
        // share with another covered texel. Exact, and no marching-squares
        // ambiguity to get wrong at a diagonal.
        let edges = |t: f32, out: &mut Vec<[Pos2; 2]>| {
            for row in 0..rect.h as i32 {
                for col in 0..rect.w as i32 {
                    if at(col, row) < t {
                        continue;
                    }
                    let (x0, y0) = ((rect.x + col) as f32, (rect.y + row) as f32);
                    let tl = self.to_screen(view, x0, y0);
                    let br = self.to_screen(view, x0 + 1.0, y0 + 1.0);
                    if at(col, row - 1) < t {
                        out.push([tl, Pos2::new(br.x, tl.y)]);
                    }
                    if at(col, row + 1) < t {
                        out.push([Pos2::new(tl.x, br.y), br]);
                    }
                    if at(col - 1, row) < t {
                        out.push([tl, Pos2::new(tl.x, br.y)]);
                    }
                    if at(col + 1, row) < t {
                        out.push([Pos2::new(br.x, tl.y), br]);
                    }
                }
            }
        };

        // The outer reach first, so the real edge draws over it.
        if soft {
            let mut reach = Vec::new();
            edges(0.06, &mut reach);
            for [a, b] in reach {
                p.line_segment([a, b], EStroke::new(1.0, Color32::from_white_alpha(60)));
            }
        }
        let mut body = Vec::new();
        edges(0.5, &mut body);
        for [a, b] in &body {
            p.line_segment([*a + Vec2::splat(1.0), *b + Vec2::splat(1.0)], EStroke::new(1.0, dark));
        }
        for [a, b] in &body {
            p.line_segment([*a, *b], EStroke::new(1.0, light));
        }
    }

    /// Rebuild the selection outline (canvas-space segments). Only runs when the
    /// selection actually changed — a per-frame scan of a 2048² mask is not free.
    fn rebuild_ants(&mut self) {
        self.ants.clear();
        self.ants_valid = true;
        let Some(doc) = &self.doc else { return };
        let Some(sel) = &doc.selection else { return };
        let b = sel.selected_bounds();
        if b.is_empty() {
            return;
        }
        // An edge exists wherever a selected pixel touches an unselected one.
        for y in b.y..b.bottom() {
            for x in b.x..b.right() {
                let here = sel.get(x, y) > 127;
                if !here {
                    continue;
                }
                if sel.get(x - 1, y) <= 127 {
                    self.ants.push([(x as f32, y as f32), (x as f32, y as f32 + 1.0)]);
                }
                if sel.get(x + 1, y) <= 127 {
                    self.ants.push([(x as f32 + 1.0, y as f32), (x as f32 + 1.0, y as f32 + 1.0)]);
                }
                if sel.get(x, y - 1) <= 127 {
                    self.ants.push([(x as f32, y as f32), (x as f32 + 1.0, y as f32)]);
                }
                if sel.get(x, y + 1) <= 127 {
                    self.ants.push([(x as f32, y as f32 + 1.0), (x as f32 + 1.0, y as f32 + 1.0)]);
                }
                // A pathological selection (a checkerboard) could produce
                // millions of segments; cap it and fall back to the bounds.
                if self.ants.len() > 20_000 {
                    self.ants.clear();
                    let (x0, y0) = (b.x as f32, b.y as f32);
                    let (x1, y1) = (b.right() as f32, b.bottom() as f32);
                    self.ants = vec![
                        [(x0, y0), (x1, y0)],
                        [(x1, y0), (x1, y1)],
                        [(x1, y1), (x0, y1)],
                        [(x0, y1), (x0, y0)],
                    ];
                    return;
                }
            }
        }
    }

    /// Advance frame playback. Called once per frame while the tab is visible.
    pub(crate) fn tick(&mut self, dt: f32) {
        if let Some((_, t)) = &mut self.status {
            *t -= dt;
            if *t <= 0.0 {
                self.status = None;
            }
        }
        let Some(doc) = &self.doc else { return };
        if !self.playing || doc.frames <= 1 {
            return;
        }
        self.play_clock += dt * doc.fps.max(0.1);
        while self.play_clock >= 1.0 {
            self.play_clock -= 1.0;
            self.frame = (self.frame + 1) % doc.frames;
            self.pending = Some(doc.bounds());
            self.flat_cache = None;
            self.onion_tex = None;
        }
    }

    /// Switch the visible frame (and repaint the canvas).
    pub(crate) fn set_frame(&mut self, f: usize) {
        // A floating transform belongs to the frame it was lifted from.
        self.commit_live();
        let Some(doc) = &self.doc else { return };
        self.frame = f.min(doc.frames.saturating_sub(1));
        self.invalidate_all();
    }

    /// Refresh the layer thumbnails, at most a few times a second and never
    /// mid-gesture. `None` for a layer means "not built yet" — the list draws a
    /// placeholder rather than blocking on it.
    pub(crate) fn sync_thumbs(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.doc.as_ref() else { return };
        let n = doc.layers.len();
        if self.thumbs.len() != n {
            self.thumbs = vec![None; n];
            self.thumbs_dirty = true;
        }
        if !self.thumbs_dirty || self.busy() || self.drag.is_some() {
            return;
        }
        let now = std::time::Instant::now();
        if self.thumbs_at.is_some_and(|t| now.duration_since(t) < THUMB_EVERY) {
            return;
        }
        self.thumbs_at = Some(now);
        self.thumbs_dirty = false;
        const EDGE: u32 = 28;
        for i in 0..n {
            let px = composite::layer_only(doc, i, self.frame);
            let small = floptle_image::transform::resample(&px, doc.w, doc.h, EDGE, EDGE, false);
            let img = egui::ColorImage::from_rgba_unmultiplied([EDGE as usize, EDGE as usize], &small);
            self.thumbs[i] =
                Some(ctx.load_texture(format!("flimg-thumb-{i}"), img, egui::TextureOptions::LINEAR));
        }
    }

    pub(crate) fn thumb(&self, i: usize) -> Option<&egui::TextureHandle> {
        self.thumbs.get(i).and_then(|t| t.as_ref())
    }

    /// The live selection's bounding box in canvas pixels, for the status bar.
    /// `None` when nothing is selected.
    pub(crate) fn selection_bounds(&self) -> Option<Rect> {
        let b = self.doc.as_ref()?.selection.as_ref()?.selected_bounds();
        (!b.is_empty()).then_some(b)
    }

    /// Whether an onion skin is currently drawn (for the tests + the status bar).
    pub(crate) fn onion_active(&self) -> bool {
        self.onion && self.doc.as_ref().is_some_and(|d| d.frames > 1)
    }
}

/// The tab's letter keybinds — the universal paint-program bindings, live only
/// while the 🖼 tab holds focus (the viewport's `Tool` digits are full, and this
/// tab never touches them; see `gizmo.rs`). Returns true when the key was ours.
pub(crate) fn image_key(
    st: &mut ImageEditState,
    code: winit::keyboard::KeyCode,
    shift: bool,
) -> bool {
    use winit::keyboard::KeyCode as K;
    // While a text block is live the keyboard belongs to the text field in the
    // panel — B must type a "b", not swap to the brush.
    if st.text.is_some() && !matches!(code, K::Escape | K::Enter | K::NumpadEnter) {
        return false;
    }
    let tool = match code {
        // B toggles between the hard pencil and the soft brush, the way every
        // editor overloads it.
        K::KeyB => Some(if st.tool == ImgTool::Pencil { ImgTool::Brush } else { ImgTool::Pencil }),
        K::KeyE => Some(ImgTool::Eraser),
        K::KeyG => Some(if shift { ImgTool::Gradient } else { ImgTool::Bucket }),
        K::KeyL => Some(ImgTool::Line),
        K::KeyU => Some(if shift { ImgTool::Ellipse } else { ImgTool::Rectangle }),
        K::KeyM => Some(if shift { ImgTool::SelectEllipse } else { ImgTool::SelectRect }),
        K::KeyQ => Some(ImgTool::Lasso),
        K::KeyW => Some(ImgTool::Wand),
        K::KeyV => Some(ImgTool::Move),
        K::KeyI => Some(ImgTool::Eyedropper),
        K::KeyA => Some(ImgTool::Reshape),
        K::KeyP => Some(ImgTool::Pen),
        K::KeyT => Some(ImgTool::Text),
        _ => None,
    };
    if let Some(t) = tool {
        st.tool = t;
        match t {
            ImgTool::Pencil => {
                st.brush.pixel_perfect = true;
                st.brush.mode = BrushMode::Paint;
            }
            ImgTool::Brush => {
                st.brush.pixel_perfect = false;
                st.brush.mode = BrushMode::Paint;
                if st.brush.radius <= 1.0 {
                    st.brush.radius = 8.0;
                }
            }
            ImgTool::Eraser => st.brush.mode = BrushMode::Erase,
            _ => {}
        }
        return true;
    }
    match code {
        K::KeyX => {
            std::mem::swap(&mut st.color, &mut st.color2);
            true
        }
        K::BracketLeft => {
            st.brush.radius = (st.brush.radius - (st.brush.radius * 0.25).max(0.5)).max(0.5);
            true
        }
        K::BracketRight => {
            st.brush.radius = (st.brush.radius + (st.brush.radius * 0.25).max(0.5)).min(512.0);
            true
        }
        K::Delete | K::Backspace => {
            st.delete_selection();
            true
        }
        // Nudge: the floating transform if one is up, else the active layer.
        // Shift moves ten at a time, the universal coarse step.
        K::ArrowLeft | K::ArrowRight | K::ArrowUp | K::ArrowDown => {
            let n = if shift { 10 } else { 1 };
            let (dx, dy) = match code {
                K::ArrowLeft => (-n, 0),
                K::ArrowRight => (n, 0),
                K::ArrowUp => (0, -n),
                _ => (0, n),
            };
            st.nudge(dx, dy);
            true
        }
        K::Equal | K::NumpadAdd => {
            st.zoom_step(1.25);
            true
        }
        K::Minus | K::NumpadSubtract => {
            st.zoom_step(0.8);
            true
        }
        K::Enter | K::NumpadEnter => {
            // Enter finishes whatever is in flight.
            if let Some(p) = st.pen.take() {
                st.commit_pen(p);
            } else if st.xform.is_some() {
                st.commit_transform();
            } else if st.text.is_some() {
                st.commit_text();
            }
            true
        }
        _ => false,
    }
}

/// Nearest sample from a tightly-packed RGBA buffer (pixel mode's resampler).
fn sample_nearest(buf: &[u8], w: u32, h: u32, x: f32, y: f32) -> [u8; 4] {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    if xi < 0 || yi < 0 || xi >= w as i32 || yi >= h as i32 {
        return [0, 0, 0, 0];
    }
    let o = (yi as usize * w as usize + xi as usize) * 4;
    [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
}

/// Bilinear sample, premultiplied so transparent neighbours can't drag their
/// stale colour into the result.
fn sample_bilinear(buf: &[u8], w: u32, h: u32, x: f32, y: f32) -> [u8; 4] {
    let at = |xi: i32, yi: i32| -> [f32; 4] {
        if xi < 0 || yi < 0 || xi >= w as i32 || yi >= h as i32 {
            return [0.0; 4];
        }
        let o = (yi as usize * w as usize + xi as usize) * 4;
        let a = buf[o + 3] as f32 / 255.0;
        [buf[o] as f32 * a, buf[o + 1] as f32 * a, buf[o + 2] as f32 * a, buf[o + 3] as f32]
    };
    let (fx, fy) = (x - 0.5, y - 0.5);
    let (x0, y0) = (fx.floor() as i32, fy.floor() as i32);
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let mut acc = [0f32; 4];
    for (dx, dy, wgt) in [
        (0, 0, (1.0 - tx) * (1.0 - ty)),
        (1, 0, tx * (1.0 - ty)),
        (0, 1, (1.0 - tx) * ty),
        (1, 1, tx * ty),
    ] {
        let s = at(x0 + dx, y0 + dy);
        for i in 0..4 {
            acc[i] += s[i] * wgt;
        }
    }
    let a = acc[3];
    let inv = if a > 0.5 { 255.0 / a } else { 0.0 };
    [
        floptle_image::u8c(acc[0] * inv),
        floptle_image::u8c(acc[1] * inv),
        floptle_image::u8c(acc[2] * inv),
        floptle_image::u8c(a),
    ]
}

/// Is the point inside the (possibly rotated, possibly mirrored) quad?
fn point_in_quad(p: (f32, f32), q: &[(f32, f32); 4]) -> bool {
    // Two triangles, barycentric sign test — robust to any winding.
    let tri = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
        let d = |u: (f32, f32), v: (f32, f32), w: (f32, f32)| {
            (u.0 - w.0) * (v.1 - w.1) - (v.0 - w.0) * (u.1 - w.1)
        };
        let (d1, d2, d3) = (d(p, a, b), d(p, b, c), d(p, c, a));
        let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
        let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
        !(neg && pos)
    };
    tri(q[0], q[1], q[2]) || tri(q[0], q[2], q[3])
}

/// Rasterize `text` through the EDITOR'S OWN font stack into an RGBA block.
///
/// The glyphs come out of egui's font atlas — the same atlas the rest of the
/// engine's UI draws from — so text stamped into an image matches the text
/// beside it, and the image kernel stays free of a font dependency.
pub(crate) fn rasterize_text(
    ctx: &egui::Context,
    text: &str,
    size: f32,
    color: [u8; 4],
) -> Option<(Vec<u8>, u32, u32)> {
    let font = egui::FontId::proportional(size.clamp(4.0, 512.0));
    let galley = ctx.fonts_mut(|f| {
        f.layout(text.to_owned(), font, egui::Color32::WHITE, f32::INFINITY)
    });
    let atlas = ctx.fonts(|f| f.image());
    let (aw, ah) = (atlas.size[0] as i32, atlas.size[1] as i32);
    let w = galley.size().x.ceil().max(1.0) as u32;
    let h = galley.size().y.ceil().max(1.0) as u32;
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for row in &galley.rows {
        for g in &row.row.glyphs {
            let uv = &g.uv_rect;
            if uv.is_nothing() {
                continue;
            }
            let dst_x = row.pos.x + g.pos.x + uv.offset.x;
            let dst_y = row.pos.y + g.pos.y + uv.offset.y;
            let (gw, gh) = (uv.size.x.max(1.0), uv.size.y.max(1.0));
            let (tw, th) = ((uv.max[0] - uv.min[0]) as f32, (uv.max[1] - uv.min[1]) as f32);
            for py in 0..gh.ceil() as i32 {
                for px in 0..gw.ceil() as i32 {
                    let ox = (dst_x + px as f32).floor() as i32;
                    let oy = (dst_y + py as f32).floor() as i32;
                    if ox < 0 || oy < 0 || ox >= w as i32 || oy >= h as i32 {
                        continue;
                    }
                    // Sample the atlas across the glyph's texel box (the atlas
                    // may be at a different scale than points on HiDPI).
                    let sx = uv.min[0] as f32 + (px as f32 + 0.5) / gw * tw;
                    let sy = uv.min[1] as f32 + (py as f32 + 0.5) / gh * th;
                    let (sxi, syi) = (sx.floor() as i32, sy.floor() as i32);
                    if sxi < 0 || syi < 0 || sxi >= aw || syi >= ah {
                        continue;
                    }
                    let a = atlas.pixels[(syi * aw + sxi) as usize].a();
                    if a == 0 {
                        continue;
                    }
                    let o = (oy as usize * w as usize + ox as usize) * 4;
                    // Glyph boxes can overlap; keep the strongest coverage.
                    if a > out[o + 3] {
                        out[o] = color[0];
                        out[o + 1] = color[1];
                        out[o + 2] = color[2];
                        out[o + 3] = floptle_image::u8c(a as f32 * (color[3] as f32 / 255.0));
                    }
                }
            }
        }
    }
    Some((out, w, h))
}

/// Brush defaults that match the document's way of working.
pub(crate) fn default_brush_for(mode: Mode) -> Brush {
    match mode {
        Mode::Pixel => Brush { radius: 0.5, hardness: 1.0, pixel_perfect: true, ..Default::default() },
        _ => Brush {
            radius: 12.0,
            hardness: 0.5,
            flow: 0.85,
            spacing: 0.1,
            pixel_perfect: false,
            ..Default::default()
        },
    }
}

/// Shift-constrain: squares for boxes, 45° steps for lines.
fn constrain(from: (f32, f32), to: (f32, f32), shift: bool, is_line: bool) -> (f32, f32) {
    if !shift {
        return to;
    }
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    if is_line {
        let a = dy.atan2(dx);
        let step = std::f32::consts::FRAC_PI_4;
        let a = (a / step).round() * step;
        let len = (dx * dx + dy * dy).sqrt();
        (from.0 + a.cos() * len, from.1 + a.sin() * len)
    } else {
        let s = dx.abs().max(dy.abs());
        (from.0 + s * dx.signum(), from.1 + s * dy.signum())
    }
}

/// The integer pixel rect two canvas points span.
fn rect_between(a: (f32, f32), b: (f32, f32)) -> Rect {
    Rect::from_points(
        a.0.floor() as i32,
        a.1.floor() as i32,
        (b.0.ceil() as i32 - 1).max(a.0.floor() as i32),
        (b.1.ceil() as i32 - 1).max(a.1.floor() as i32),
    )
}

/// Paint into the active layer's MASK rather than its pixels.
fn paint_mask(doc: &mut Image, brush: &Brush, x: f32, y: f32, erase: bool) -> Rect {
    let (w, h) = (doc.w, doc.h);
    let active = doc.active;
    let Some(layer) = doc.layers.get_mut(active) else { return Rect::EMPTY };
    if layer.locked {
        return Rect::EMPTY;
    }
    let mask = layer.mask.get_or_insert_with(|| Mask::new(w, h, 255));
    let r = brush.dab_rect(x, y).intersect(Rect::size(w, h));
    let radius = brush.radius.max(0.5);
    for py in r.y..r.bottom() {
        for px in r.x..r.right() {
            let d = ((px as f32 + 0.5 - x).powi(2) + (py as f32 + 0.5 - y).powi(2)).sqrt();
            if d > radius {
                continue;
            }
            let cov = if brush.hardness >= 0.99 || brush.pixel_perfect {
                1.0
            } else {
                let inner = radius * brush.hardness;
                ((radius - d) / (radius - inner).max(1e-4)).clamp(0.0, 1.0)
            } * brush.flow;
            let cur = mask.get(px, py) as f32 / 255.0;
            let target = if erase { 0.0 } else { 1.0 };
            mask.set(px, py, floptle_image::u8c((cur + (target - cur) * cov) * 255.0));
        }
    }
    r
}

/// An opaque colour with an explicit alpha, the shape every overlay setting has.
fn rgba(c: [u8; 3], a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], a)
}

fn draw_checker(p: &egui::Painter, rect: ERect, look: &crate::prefs::CanvasLook) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    // A checker sized in SCREEN pixels, so it doesn't turn into a moiré at
    // high zoom or vanish at low zoom.
    let step = look.checker_px.clamp(2.0, 64.0);
    let a = Color32::from_rgb(look.checker_a[0], look.checker_a[1], look.checker_a[2]);
    let b = Color32::from_rgb(look.checker_b[0], look.checker_b[1], look.checker_b[2]);
    p.rect_filled(rect, 0.0, a);
    let mut y = rect.top();
    let mut row = 0;
    while y < rect.bottom() {
        let mut x = rect.left() + if row % 2 == 0 { 0.0 } else { step };
        while x < rect.right() {
            let cell = ERect::from_min_size(Pos2::new(x, y), Vec2::splat(step)).intersect(rect);
            if cell.width() > 0.0 && cell.height() > 0.0 {
                p.rect_filled(cell, 0.0, b);
            }
            x += step * 2.0;
        }
        y += step;
        row += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_doc() -> ImageEditState {
        let mut st = ImageEditState::default();
        st.adopt(Image::new(32, 32, Mode::Pixel), None, None);
        st
    }

    #[test]
    fn pixel_mode_zoom_snaps_to_integers() {
        let st = state_with_doc();
        assert_eq!(st.snap_zoom(7.4), 7.0);
        assert_eq!(st.snap_zoom(0.4), 1.0 / 3.0);
        assert_eq!(st.snap_zoom(1.2), 1.0);
    }

    #[test]
    fn painterly_zoom_is_continuous() {
        let mut st = ImageEditState::default();
        st.adopt(Image::new(64, 64, Mode::Painterly), None, None);
        assert!((st.snap_zoom(1.37) - 1.37).abs() < 1e-6);
    }

    #[test]
    fn screen_and_canvas_coordinates_round_trip() {
        let mut st = state_with_doc();
        st.zoom = 4.0;
        st.pan = Vec2::new(11.0, -7.0);
        let view = ERect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(400.0, 300.0));
        let s = st.to_screen(view, 6.0, 9.0);
        let (x, y) = st.to_canvas(view, s);
        assert!((x - 6.0).abs() < 1e-3 && (y - 9.0).abs() < 1e-3, "{x},{y}");
    }

    #[test]
    fn zooming_keeps_the_pixel_under_the_cursor() {
        let mut st = state_with_doc();
        st.zoom = 4.0;
        let view = ERect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 400.0));
        let at = Pos2::new(210.0, 130.0);
        let before = st.to_canvas(view, at);
        st.zoom_at(view, at, 2.0);
        let after = st.to_canvas(view, at);
        assert!((before.0 - after.0).abs() < 0.6, "{before:?} vs {after:?}");
        assert!((before.1 - after.1).abs() < 0.6);
        assert_eq!(st.zoom, 8.0);
    }

    #[test]
    fn undo_restores_pixels_and_redo_puts_them_back() {
        let mut st = state_with_doc();
        st.push_undo();
        st.paint_at(4.5, 4.5, true);
        assert_ne!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().get(4, 4)[3], 0);
        st.undo();
        assert_eq!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().get(4, 4)[3], 0);
        st.redo();
        assert_ne!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().get(4, 4)[3], 0);
    }

    #[test]
    fn undo_is_capped_and_never_underflows() {
        let mut st = state_with_doc();
        for _ in 0..(UNDO_DEPTH + 20) {
            st.push_undo();
        }
        assert_eq!(st.undo.len(), UNDO_DEPTH);
        for _ in 0..(UNDO_DEPTH + 5) {
            st.undo();
        }
        assert!(!st.can_undo());
        assert!(st.doc.is_some(), "undoing past the start must not lose the document");
    }

    #[test]
    fn palette_lock_snaps_the_stroke_colour() {
        let mut st = state_with_doc();
        st.color = [200, 200, 200, 255];
        assert_eq!(st.stroke_color(), [200, 200, 200, 255]);
        let doc = st.doc.as_mut().unwrap();
        doc.palette = Some(Palette { name: "x".into(), colors: vec![[0, 0, 0, 255], [255, 255, 255, 255]] });
        doc.palette_lock = true;
        assert_eq!(st.stroke_color(), [255, 255, 255, 255]);
    }

    #[test]
    fn mirroring_paints_both_sides() {
        let mut st = state_with_doc();
        st.mirror_x = true;
        st.paint_at(4.5, 10.5, true);
        let g = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap();
        assert_ne!(g.get(4, 10)[3], 0, "the dab");
        assert_ne!(g.get(27, 10)[3], 0, "and its mirror");
    }

    #[test]
    fn a_locked_layer_refuses_the_brush() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().layers[0].locked = true;
        st.paint_at(4.5, 4.5, true);
        assert_eq!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().get(4, 4)[3], 0);
    }

    #[test]
    fn marquee_boolean_ops_apply() {
        let mut st = state_with_doc();
        st.sel_op = SelectOp::Replace;
        st.apply_marquee(floptle_image::select::rect_mask(32, 32, Rect::new(0, 0, 10, 32)));
        assert!(st.has_selection());
        st.sel_op = SelectOp::Add;
        st.apply_marquee(floptle_image::select::rect_mask(32, 32, Rect::new(20, 0, 12, 32)));
        let b = st.doc.as_ref().unwrap().selection.as_ref().unwrap().selected_bounds();
        assert_eq!(b, Rect::new(0, 0, 32, 32));
    }

    /// A selection covering everything is the same as none — and treating it as
    /// "a selection" would leave the canvas looking inexplicably dead outside it.
    #[test]
    fn a_full_or_empty_marquee_clears_the_selection() {
        let mut st = state_with_doc();
        st.apply_marquee(floptle_image::select::rect_mask(32, 32, Rect::new(0, 0, 32, 32)));
        assert!(!st.has_selection());
        st.apply_marquee(floptle_image::select::rect_mask(32, 32, Rect::EMPTY));
        assert!(!st.has_selection());
    }

    #[test]
    fn selection_clips_the_brush() {
        let mut st = state_with_doc();
        st.apply_marquee(floptle_image::select::rect_mask(32, 32, Rect::new(0, 0, 8, 32)));
        st.paint_at(4.5, 4.5, true);
        st.paint_at(20.5, 4.5, true);
        let g = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap();
        assert_ne!(g.get(4, 4)[3], 0, "inside");
        assert_eq!(g.get(20, 4)[3], 0, "outside");
    }

    #[test]
    fn shape_constraints_square_up() {
        let c = constrain((0.0, 0.0), (10.0, 3.0), true, false);
        assert_eq!(c, (10.0, 10.0));
        let l = constrain((0.0, 0.0), (10.0, 1.0), true, true);
        assert!(l.1.abs() < 1e-3, "a near-horizontal line snaps flat: {l:?}");
    }

    #[test]
    fn shapes_stamp_pixels_or_spawn_a_vector_layer() {
        let mut st = state_with_doc();
        st.tool = ImgTool::Rectangle;
        st.commit_shape((4.0, 4.0), (12.0, 12.0));
        assert_ne!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().get(8, 8)[3], 0);
        st.shape_vector = true;
        st.commit_shape((16.0, 16.0), (24.0, 24.0));
        assert_eq!(st.doc.as_ref().unwrap().layers.len(), 2);
        assert!(st.doc.as_ref().unwrap().layers[1].kind.is_vector());
    }

    #[test]
    fn the_pen_needs_three_nodes_to_close() {
        let mut st = state_with_doc();
        st.tool = ImgTool::Pen;
        st.pen_click(2.0, 2.0, false);
        st.pen_click(10.0, 2.0, false);
        assert!(st.pen.is_some());
        st.pen_click(2.0, 2.0, true); // close attempt with only 2 nodes
        assert!(st.pen.is_some(), "two nodes can't be a closed shape");
        st.pen_click(10.0, 10.0, false);
        st.pen_click(2.0, 2.0, true);
        assert!(st.pen.is_none());
        assert!(st.doc.as_ref().unwrap().layers.iter().any(|l| l.kind.is_vector()));
    }

    #[test]
    fn mask_painting_writes_the_mask_not_the_pixels() {
        let mut st = state_with_doc();
        st.surface = PaintTargetSurface::Mask;
        st.brush.radius = 3.0;
        st.paint_at(8.0, 8.0, true);
        let l = &st.doc.as_ref().unwrap().layers[0];
        assert!(l.mask.is_some());
        assert_eq!(l.mask.as_ref().unwrap().get(8, 8), 255);
        assert_eq!(l.grid(0).unwrap().get(8, 8)[3], 0, "pixels untouched");
    }

    #[test]
    fn adopting_a_document_picks_matching_tool_defaults() {
        let mut st = ImageEditState::default();
        st.adopt(Image::new(1024, 1024, Mode::Painterly), None, None);
        assert_eq!(st.tool, ImgTool::Brush);
        assert!(!st.brush.pixel_perfect);
        st.adopt(Image::new(32, 32, Mode::Pixel), None, None);
        assert_eq!(st.tool, ImgTool::Pencil);
        assert!(st.brush.pixel_perfect);
    }

    #[test]
    fn frame_playback_wraps() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().set_frames(3);
        st.doc.as_mut().unwrap().fps = 10.0;
        st.playing = true;
        for _ in 0..3 {
            st.tick(0.1);
        }
        assert_eq!(st.frame, 0, "three frames at 10 fps for 0.3 s wraps to the start");
    }

    #[test]
    fn marching_ants_fall_back_when_a_selection_is_pathological() {
        let mut st = ImageEditState::default();
        st.adopt(Image::new(400, 400, Mode::Pixel), None, None);
        // A checkerboard selection would produce ~640k segments.
        let mut m = Mask::new(400, 400, 0);
        for y in 0..400 {
            for x in 0..400 {
                if (x + y) % 2 == 0 {
                    m.set(x, y, 255);
                }
            }
        }
        st.doc.as_mut().unwrap().selection = Some(m);
        st.ants_valid = false;
        st.rebuild_ants();
        assert_eq!(st.ants.len(), 4, "capped to the bounding box");
    }

    /// Free transform: lift, move, commit — and ONE undo puts it all back.
    #[test]
    fn free_transform_moves_pixels_and_undo_restores_them() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(4, 4, 6, 6), |_, _, p| *p = [255, 0, 0, 255]);
        st.tool = ImgTool::Transform;
        assert!(st.begin_transform());
        let mut xf = st.xform.as_ref().unwrap().xf;
        xf.translate = (10.0, 8.0);
        st.set_xform(xf);
        let g = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap();
        assert_eq!(g.get(6, 6)[3], 0, "the source region was lifted");
        assert_eq!(g.get(16, 14), [255, 0, 0, 255], "and landed at the offset");
        st.commit_transform();
        assert!(st.xform.is_none() && st.dirty);
        st.undo();
        let g = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap();
        assert_eq!(g.get(6, 6), [255, 0, 0, 255], "one undo restores the whole transform");
        assert_eq!(g.get(16, 14)[3], 0);
    }

    /// Cancelling must restore the document EXACTLY, not approximately.
    #[test]
    fn cancelling_a_transform_is_exact() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(4, 4, 6, 6), |_, _, p| *p = [9, 40, 200, 255]);
        let before = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().to_rgba();
        st.tool = ImgTool::Transform;
        st.begin_transform();
        let mut xf = st.xform.as_ref().unwrap().xf;
        xf.rotate = 0.7;
        xf.scale = (1.8, 0.4);
        st.set_xform(xf);
        assert_ne!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().to_rgba(), before);
        assert!(st.cancel_transform());
        assert_eq!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().to_rgba(), before);
        assert!(!st.can_undo(), "a cancelled transform leaves no undo litter");
    }

    /// A transform lifts only what's SELECTED when there is a selection.
    #[test]
    fn a_selection_scopes_the_transform() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().layers[0].grid_mut(0).unwrap().fill([200, 200, 200, 255]);
        st.doc.as_mut().unwrap().selection =
            Some(floptle_image::select::rect_mask(32, 32, Rect::new(0, 0, 8, 8)));
        st.tool = ImgTool::Transform;
        assert!(st.begin_transform());
        let sess = st.xform.as_ref().unwrap();
        assert_eq!(sess.rect, Rect::new(0, 0, 8, 8));
        // Outside the selection nothing was lifted.
        assert_eq!(
            st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().get(20, 20),
            [200, 200, 200, 255]
        );
    }

    #[test]
    fn transform_handles_hit_test_where_they_are_drawn() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(8, 8, 8, 8), |_, _, p| *p = [1, 2, 3, 255]);
        st.zoom = 8.0;
        st.tool = ImgTool::Transform;
        st.begin_transform();
        let c = st.xform_corners().unwrap();
        assert!(st.grab_transform(c[0].0, c[0].1), "a corner is grabbable");
        assert!(st.grab_transform(12.0, 12.0), "the body is grabbable");
        assert!(!st.grab_transform(31.0, 31.0), "outside is not");
    }

    /// Copy → paste puts the pixels down as a FLOATING block: nothing is
    /// committed until Enter, one undo takes the whole paste back, and what was
    /// underneath survives (a paste doesn't lift, so it must not clear).
    #[test]
    fn paste_floats_over_what_was_already_there() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(0, 0, 4, 4), |_, _, p| *p = [255, 0, 0, 255]);
        st.doc.as_mut().unwrap().selection =
            Some(floptle_image::select::rect_mask(32, 32, Rect::new(0, 0, 4, 4)));
        assert!(st.copy_selection(false));
        assert!(st.has_clipboard());
        // Something else, where the paste will land.
        st.doc.as_mut().unwrap().layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(20, 20, 4, 4), |_, _, p| *p = [0, 0, 255, 255]);
        let before = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().to_rgba();

        st.cursor = Some((22.0, 22.0));
        assert!(st.paste());
        assert_eq!(st.tool, ImgTool::Transform, "a paste arms the transform tool");
        let g = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap();
        assert_eq!(g.get(21, 21), [255, 0, 0, 255], "the pasted pixels landed under the cursor");
        assert_eq!(g.get(0, 0), [255, 0, 0, 255], "and the source is still where it was");
        st.commit_transform();
        st.undo();
        assert_eq!(
            st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().to_rgba(),
            before,
            "one undo removes the whole paste"
        );
    }

    /// Cut takes the pixels with it.
    #[test]
    fn cut_copies_then_erases() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(2, 2, 4, 4), |_, _, p| *p = [7, 8, 9, 255]);
        st.doc.as_mut().unwrap().selection =
            Some(floptle_image::select::rect_mask(32, 32, Rect::new(2, 2, 4, 4)));
        assert!(st.copy_selection(true));
        assert_eq!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().get(3, 3)[3], 0);
        st.cursor = Some((16.0, 16.0));
        assert!(st.paste());
        assert_eq!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().get(15, 15), [7, 8, 9, 255]);
    }

    /// An empty layer has nothing to copy, and says so instead of stashing a
    /// zero-sized block that pastes as nothing.
    #[test]
    fn copying_nothing_is_refused() {
        let mut st = state_with_doc();
        assert!(!st.copy_selection(false));
        assert!(!st.has_clipboard());
        assert!(!st.paste());
    }

    /// Arrow keys nudge the transform when one is up, and the layer otherwise.
    #[test]
    fn nudge_moves_the_transform_or_the_layer() {
        let mut st = state_with_doc();
        st.nudge(3, -2);
        assert_eq!(st.doc.as_ref().unwrap().layers[0].offset, (3, -2));
        st.doc.as_mut().unwrap().layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(4, 4, 4, 4), |_, _, p| *p = [1, 1, 1, 255]);
        st.tool = ImgTool::Transform;
        assert!(st.begin_transform());
        st.nudge(5, 0);
        assert_eq!(st.xform.as_ref().unwrap().xf.translate, (5.0, 0.0));
        assert_eq!(
            st.doc.as_ref().unwrap().layers[0].offset,
            (3, -2),
            "the layer stays put while a transform is floating"
        );
    }

    /// A locked layer refuses the nudge as well as the brush.
    #[test]
    fn a_locked_layer_refuses_the_nudge() {
        let mut st = state_with_doc();
        st.doc.as_mut().unwrap().layers[0].locked = true;
        st.nudge(1, 1);
        assert_eq!(st.doc.as_ref().unwrap().layers[0].offset, (0, 0));
    }

    /// The text field asks for focus exactly once — asking every frame is what
    /// made Escape impossible to press.
    #[test]
    fn a_text_block_grabs_focus_once() {
        let mut st = state_with_doc();
        st.begin_text(2.0, 2.0);
        assert!(st.take_text_focus());
        assert!(!st.take_text_focus());
        assert!(!st.take_text_focus());
    }

    #[test]
    fn keyboard_zoom_steps_about_the_view() {
        let mut st = state_with_doc();
        st.last_view = Some(ERect::from_min_size(Pos2::ZERO, Vec2::splat(400.0)));
        st.zoom = 4.0;
        st.zoom_step(1.25);
        assert_eq!(st.zoom, 5.0, "pixel mode still snaps to whole factors");
        st.zoom_step(0.8);
        assert_eq!(st.zoom, 4.0);
    }

    #[test]
    fn rect_between_is_inclusive_of_the_dragged_pixels() {
        let r = rect_between((2.0, 3.0), (6.0, 9.0));
        assert_eq!(r, Rect::from_points(2, 3, 5, 8));
    }

    // ---- floptle/0095: the short paths ------------------------------------

    /// Dragging inside a live selection moves it, with no tool change first.
    #[test]
    fn a_drag_inside_the_selection_moves_it() {
        let mut st = ImageEditState::default();
        let mut doc = Image::new(32, 32, Mode::Pixel);
        doc.layers[0].grid_mut(0).unwrap().edit_rect(Rect::new(4, 4, 8, 8), |_, _, p| {
            *p = [200, 40, 40, 255]
        });
        doc.selection = Some(floptle_image::select::rect_mask(32, 32, Rect::new(4, 4, 8, 8)));
        st.doc = Some(doc);
        st.tool = ImgTool::SelectRect;

        st.begin_drag(6.0, 6.0, false, false);
        assert!(st.xform.is_some(), "a press inside the selection arms the transform");
        assert_eq!(st.tool, ImgTool::Transform, "and the transform owns the drag");

        // A press OUTSIDE it still starts a new marquee.
        st.cancel_transform();
        st.tool = ImgTool::SelectRect;
        st.begin_drag(25.0, 25.0, false, false);
        assert!(st.xform.is_none(), "outside the selection is a new marquee, not a move");
    }

    /// Duplicating floats a copy and leaves the original where it is — a move
    /// lifts, a duplicate does not.
    #[test]
    fn duplicating_a_selection_leaves_the_original_alone() {
        let mut st = ImageEditState::default();
        let mut doc = Image::new(32, 32, Mode::Pixel);
        doc.layers[0].grid_mut(0).unwrap().edit_rect(Rect::new(4, 4, 6, 6), |_, _, p| {
            *p = [200, 40, 40, 255]
        });
        doc.selection = Some(floptle_image::select::rect_mask(32, 32, Rect::new(4, 4, 6, 6)));
        st.doc = Some(doc);

        assert!(st.duplicate_selection(), "there is something to duplicate");
        let sess = st.xform.as_ref().expect("a session is floating");
        assert!(!sess.lift, "a duplicate does not lift its source");

        let g = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap();
        assert_eq!(g.get(6, 6), [200, 40, 40, 255], "the original is still there");
    }

    /// Nothing selected and nothing painted: there is nothing to duplicate, and
    /// it says so rather than floating an empty session.
    #[test]
    fn duplicating_nothing_does_nothing() {
        let mut st = ImageEditState {
            doc: Some(Image::new(16, 16, Mode::Pixel)),
            ..Default::default()
        };
        assert!(!st.duplicate_selection());
        assert!(st.xform.is_none());
    }

    /// The status bar can report the selection's box, so a region can be
    /// checked without counting pixels on screen.
    #[test]
    fn the_selection_reports_its_own_box() {
        let mut st = ImageEditState::default();
        let mut doc = Image::new(64, 64, Mode::Pixel);
        doc.selection = Some(floptle_image::select::rect_mask(64, 64, Rect::new(5, 9, 12, 3)));
        st.doc = Some(doc);
        let b = st.selection_bounds().expect("a live selection has a box");
        assert_eq!((b.x, b.y, b.w, b.h), (5, 9, 12, 3));

        st.doc.as_mut().unwrap().selection = None;
        assert!(st.selection_bounds().is_none());
    }

    /// A typed scale lands exactly: 8 wide at x2 is 16 wide, not 15 or 17.
    #[test]
    fn a_typed_scale_lands_on_the_number_asked_for() {
        let mut st = ImageEditState::default();
        let mut doc = Image::new(48, 48, Mode::Pixel);
        doc.layers[0].grid_mut(0).unwrap().edit_rect(Rect::new(8, 8, 8, 8), |_, _, p| {
            *p = [200, 40, 40, 255]
        });
        doc.selection = Some(floptle_image::select::rect_mask(48, 48, Rect::new(8, 8, 8, 8)));
        st.doc = Some(doc);
        assert!(st.begin_transform());
        let sess = st.xform.as_ref().unwrap();
        assert_eq!((sess.source_w(), sess.source_h()), (8, 8));

        st.xform.as_mut().unwrap().xf.scale = (2.0, 2.0);
        st.reapply_transform();
        st.commit_transform();

        // The 8x8 block, doubled about its own centre, spans 4..20.
        let g = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap();
        assert_eq!(g.get(5, 5)[3], 255, "the doubled block reaches its new corner");
        assert_eq!(g.get(18, 18)[3], 255, "and its far one");
        assert_eq!(g.get(2, 2)[3], 0, "and no further");
        assert_eq!(g.get(21, 21)[3], 0);
    }
}
