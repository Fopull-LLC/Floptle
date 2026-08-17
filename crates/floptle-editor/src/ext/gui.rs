//! `gui.*` — immediate-mode widgets, for the length of one callback.
//!
//! An extension's panel, overlay or dialog body is a Lua function the editor
//! calls while it is drawing. For that call — and only for that call — a `gui`
//! table exists, bound to the `egui::Ui` the callback is drawing into.
//!
//! ```lua
//! gui.heading("Brush")
//! gui.horizontal(function()
//!     gui.label("radius")
//!     radius = gui.slider(radius, 0.1, 20)
//! end)
//! if gui.button("Apply") then apply() end
//! ```
//!
//! **Widgets return their new value, they do not mutate.** `x = gui.slider(x,
//! …)` rather than `gui.slider(&x, …)`: Lua has no references to pass, and the
//! alternative — a table you hand in and read back — makes every call site
//! carry a container it never wanted. The one exception is `gui.button`, which
//! returns whether it was clicked, because that is what a button *is*.
//!
//! ## How a `&mut Ui` gets in here safely
//!
//! [`UiSlot`] holds a **stack** of the layouts currently being drawn into. The
//! bottom is the callback's own `Ui`; a nesting call (`gui.horizontal`) pushes
//! the child layout for the length of the inner callback and pops it after. Lua
//! only ever reaches the top of the stack, so the outer `&mut` is never used
//! while an inner one is live, and every pointer is popped before the borrow
//! that produced it ends.
//!
//! Calling any of this outside a draw callback raises — the stack is empty, and
//! there is no layout to draw into.

use std::cell::RefCell;

use mlua::{Function, Lua, Scope, Table};

/// `gui.textAt(x, y, text [, size [, r, g, b [, a]]])` — position, string, and
/// an optional size and colour, all trailing.
type TextAtArgs =
    (f32, f32, String, Option<f32>, Option<f64>, Option<f64>, Option<f64>, Option<f64>);

/// The layouts currently being drawn into, innermost last.
pub(crate) struct UiSlot {
    stack: Vec<*mut egui::Ui>,
}

impl UiSlot {
    pub(crate) fn new(ui: &mut egui::Ui) -> Self {
        Self { stack: vec![ui as *mut egui::Ui] }
    }

    fn top(&self) -> Option<*mut egui::Ui> {
        self.stack.last().copied()
    }

    fn push(&mut self, ui: &mut egui::Ui) {
        self.stack.push(ui as *mut egui::Ui);
    }

    fn pop(&mut self) {
        self.stack.pop();
    }
}

/// Run `f` against the innermost layout.
///
/// SAFETY: the pointer on top of the stack was written from a `&mut egui::Ui`
/// whose borrow strictly encloses this call (see the module docs), and the
/// `RefCell` borrow is released before `f` runs, so a nested `gui.*` call from
/// inside `f` takes its own turn rather than aliasing this one.
fn with<R>(slot: &RefCell<UiSlot>, f: impl FnOnce(&mut egui::Ui) -> R) -> mlua::Result<R> {
    let Some(ptr) = slot.borrow().top() else {
        return Err(mlua::Error::runtime(
            "gui.* only works while the editor is drawing your panel — call it from the \
             function you gave ed.window / ed.overlay, not from a timer or a menu item",
        ));
    };
    Ok(f(unsafe { &mut *ptr }))
}

/// The nesting shape shared by `horizontal`, `vertical`, `group` and friends:
/// run the egui layout function, and inside it push the child layout, call back
/// into Lua, and pop.
fn nest(
    slot: &RefCell<UiSlot>,
    cb: &Function,
    lay: impl FnOnce(&mut egui::Ui, &mut dyn FnMut(&mut egui::Ui)),
) -> mlua::Result<()> {
    let mut err: Option<mlua::Error> = None;
    with(slot, |ui| {
        lay(ui, &mut |inner: &mut egui::Ui| {
            slot.borrow_mut().push(inner);
            if err.is_none() {
                err = cb.call::<()>(()).err();
            }
            slot.borrow_mut().pop();
        });
    })?;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Does egui actually hold this family?
///
/// Asked of egui rather than assumed, because `FontFamily::Name` is a panic and
/// not a fallback when it is wrong.
fn is_bound(ui: &egui::Ui, family: &str) -> bool {
    let want = egui::FontFamily::Name(family.into());
    ui.ctx().fonts(|f| f.families().contains(&want))
}

fn color(r: f64, g: f64, b: f64, a: Option<f64>) -> egui::Color32 {
    let f = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    egui::Color32::from_rgba_unmultiplied(f(r), f(g), f(b), f(a.unwrap_or(1.0)))
}

/// Which typefaces the package that is drawing may name, and who to blame in the
/// Console when it names one that is not there.
///
/// Passed in rather than looked up, because `gui` is built per draw call and the
/// answer to "does this package have a face called Heading" is fixed for the
/// length of a package load.
pub(crate) struct FontScope<'a> {
    pub(crate) pkg_id: &'a str,
    pub(crate) faces: &'a [crate::fonts::PackageFont],
    /// Names already complained about, so a panel drawing at 60 Hz costs one
    /// Console line and not sixty a second.
    pub(crate) warned: &'a RefCell<std::collections::HashSet<String>>,
    pub(crate) log: &'a RefCell<Vec<super::ExtLog>>,
    pub(crate) pkg_name: &'a str,
}

/// Build the `gui` table for one callback. Every function is scoped: it stops
/// working the moment the call returns.
pub(crate) fn bind<'scope, 'env: 'scope>(
    lua: &Lua,
    scope: &'scope Scope<'scope, 'env>,
    slot: &'env RefCell<UiSlot>,
    fonts: &'env FontScope<'env>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;

    // ---- text -------------------------------------------------------------
    // Hover text is a trailing argument rather than a `gui.tooltip()` that
    // attaches to "the last widget": an immediate-mode call that silently
    // depends on the call before it is the kind of thing that works until
    // somebody inserts a line.
    t.set(
        "label",
        scope.create_function(move |_, (text, tip): (String, Option<String>)| {
            with(slot, |ui| {
                let r = ui.label(text);
                if let Some(tip) = tip {
                    r.on_hover_text(tip);
                }
            })
        })?,
    )?;
    t.set(
        "heading",
        scope
            .create_function(move |_, text: String| with(slot, |ui| ui.heading(text)).map(|_| ()))?,
    )?;
    t.set(
        "small",
        scope.create_function(move |_, text: String| with(slot, |ui| ui.small(text)).map(|_| ()))?,
    )?;
    t.set(
        "monospace",
        scope.create_function(move |_, text: String| {
            with(slot, |ui| ui.monospace(text)).map(|_| ())
        })?,
    )?;
    t.set(
        "colored",
        scope.create_function(
            move |_, (text, r, g, b, a): (String, f64, f64, f64, Option<f64>)| {
                with(slot, |ui| ui.colored_label(color(r, g, b, a), text)).map(|_| ())
            },
        )?,
    )?;
    t.set(
        "wrapped",
        scope.create_function(move |_, text: String| {
            with(slot, |ui| ui.add(egui::Label::new(text).wrap())).map(|_| ())
        })?,
    )?;
    // A link is a label that reports being clicked — the caller decides whether
    // that means `ed.openUrl`, which needs a permission this table does not.
    t.set(
        "link",
        scope.create_function(move |_, text: String| {
            with(slot, |ui| ui.link(text).clicked())
        })?,
    )?;

    // ---- buttons and toggles ----------------------------------------------
    t.set(
        "button",
        scope.create_function(move |_, (text, tip): (String, Option<String>)| {
            with(slot, |ui| {
                let r = ui.button(text);
                match tip {
                    Some(tip) => r.on_hover_text(tip).clicked(),
                    None => r.clicked(),
                }
            })
        })?,
    )?;
    t.set(
        "smallButton",
        scope.create_function(move |_, text: String| {
            with(slot, |ui| ui.small_button(text).clicked())
        })?,
    )?;
    t.set(
        "checkbox",
        scope.create_function(move |_, (value, text, tip): (bool, String, Option<String>)| {
            with(slot, |ui| {
                let mut v = value;
                let r = ui.checkbox(&mut v, text);
                if let Some(tip) = tip {
                    r.on_hover_text(tip);
                }
                v
            })
        })?,
    )?;
    t.set(
        "toggle",
        scope.create_function(move |_, (value, text): (bool, String)| {
            with(slot, |ui| {
                let mut v = value;
                ui.toggle_value(&mut v, text);
                v
            })
        })?,
    )?;
    t.set(
        "radio",
        scope.create_function(move |_, (selected, text): (bool, String)| {
            with(slot, |ui| ui.radio(selected, text).clicked())
        })?,
    )?;
    t.set(
        "selectable",
        scope.create_function(move |_, (selected, text): (bool, String)| {
            with(slot, |ui| ui.selectable_label(selected, text).clicked())
        })?,
    )?;

    // ---- numbers and text entry -------------------------------------------
    t.set(
        "slider",
        scope.create_function(
            move |_, (value, min, max, label): (f64, f64, f64, Option<String>)| {
                with(slot, |ui| {
                    let mut v = value;
                    let mut w = egui::Slider::new(&mut v, min..=max);
                    if let Some(l) = &label {
                        w = w.text(l);
                    }
                    ui.add(w);
                    v
                })
            },
        )?,
    )?;
    t.set(
        "drag",
        scope.create_function(move |_, (value, speed, label): (f64, Option<f64>, Option<String>)| {
            with(slot, |ui| {
                let mut v = value;
                let mut w = egui::DragValue::new(&mut v);
                if let Some(s) = speed {
                    w = w.speed(s);
                }
                if let Some(l) = &label {
                    ui.horizontal(|ui| {
                        ui.label(l);
                        ui.add(w);
                    });
                } else {
                    ui.add(w);
                }
                v
            })
        })?,
    )?;
    // The third argument asks for the keyboard THIS frame. A panel that opens
    // on a shortcut and cannot be typed into until you click it is a panel that
    // gets clicked into every single time.
    t.set(
        "textField",
        // Returns `value, submitted`. `submitted` is true on the frame the
        // field was left by pressing Enter — the only way a package can know a
        // question was finished, because a returned string looks identical
        // whether somebody typed a character or pressed the key that means
        // "send". Lua drops extra return values, so the one-value call sites
        // that existed before are unaffected.
        scope.create_function(
            move |_, (value, hint, focus): (String, Option<String>, Option<bool>)| {
                with(slot, |ui| {
                    let mut v = value;
                    let mut w = egui::TextEdit::singleline(&mut v);
                    if let Some(h) = &hint {
                        w = w.hint_text(h);
                    }
                    let r = ui.add(w);
                    if focus.unwrap_or(false) {
                        r.request_focus();
                    }
                    let submitted = r.lost_focus()
                        && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                    (v, submitted)
                })
            },
        )?,
    )?;
    t.set(
        "passwordField",
        scope.create_function(move |_, value: String| {
            with(slot, |ui| {
                let mut v = value;
                ui.add(egui::TextEdit::singleline(&mut v).password(true));
                v
            })
        })?,
    )?;
    t.set(
        "textArea",
        scope.create_function(move |_, (value, rows): (String, Option<usize>)| {
            with(slot, |ui| {
                let mut v = value;
                ui.add(egui::TextEdit::multiline(&mut v).desired_rows(rows.unwrap_or(4)));
                v
            })
        })?,
    )?;
    // 1-based, like every other index a Lua author handles.
    t.set(
        "combo",
        scope.create_function(move |_, (label, options, selected): (String, Vec<String>, usize)| {
            with(slot, |ui| {
                let mut idx = selected.clamp(1, options.len().max(1));
                let shown = options.get(idx - 1).cloned().unwrap_or_default();
                egui::ComboBox::from_label(label).selected_text(shown).show_ui(ui, |ui| {
                    for (i, o) in options.iter().enumerate() {
                        ui.selectable_value(&mut idx, i + 1, o);
                    }
                });
                idx
            })
        })?,
    )?;
    t.set(
        "colorEdit",
        scope.create_function(move |lua, (r, g, b): (f64, f64, f64)| {
            let out = with(slot, |ui| {
                let mut c = [r as f32, g as f32, b as f32];
                ui.color_edit_button_rgb(&mut c);
                c
            })?;
            let t = lua.create_table()?;
            t.set(1, out[0] as f64)?;
            t.set(2, out[1] as f64)?;
            t.set(3, out[2] as f64)?;
            Ok(t)
        })?,
    )?;

    // ---- layout ------------------------------------------------------------
    t.set(
        "horizontal",
        scope.create_function(move |_, cb: Function| {
            nest(slot, &cb, |ui, f| {
                ui.horizontal(|inner| f(inner));
            })
        })?,
    )?;
    t.set(
        "vertical",
        scope.create_function(move |_, cb: Function| {
            nest(slot, &cb, |ui, f| {
                ui.vertical(|inner| f(inner));
            })
        })?,
    )?;
    t.set(
        "group",
        scope.create_function(move |_, cb: Function| {
            nest(slot, &cb, |ui, f| {
                ui.group(|inner| f(inner));
            })
        })?,
    )?;
    t.set(
        "indent",
        scope.create_function(move |_, cb: Function| {
            nest(slot, &cb, |ui, f| {
                ui.indent("ext_indent", |inner| f(inner));
            })
        })?,
    )?;
    t.set(
        "scroll",
        scope.create_function(move |_, cb: Function| {
            nest(slot, &cb, |ui, f| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |inner| f(inner));
            })
        })?,
    )?;
    t.set(
        "collapsing",
        scope.create_function(move |_, (title, cb): (String, Function)| {
            nest(slot, &cb, move |ui, f| {
                ui.collapsing(title, |inner| f(inner));
            })
        })?,
    )?;
    t.set(
        "enabled",
        scope.create_function(move |_, (on, cb): (bool, Function)| {
            nest(slot, &cb, move |ui, f| {
                ui.add_enabled_ui(on, |inner| f(inner));
            })
        })?,
    )?;
    // Draw a run of widgets in a face this package shipped. Scoped, like every
    // other nesting call here — there is no `setFont`, because a mode that
    // outlives the panel that switched it on is a mode somebody forgets to
    // switch off.
    //
    // The face replaces the *family* of every text style and leaves the sizes
    // alone, so `gui.heading` inside is still bigger than `gui.label` inside.
    // That includes Monospace: a package that ships a mono face and asks for it
    // means it.
    t.set(
        "font",
        scope.create_function(move |_, (name, cb): (String, Function)| {
            let family = crate::fonts::family_key(fonts.pkg_id, &name);
            // Two different questions, and conflating them is a crash.
            //
            // DECLARED — is this face in the package's manifest? That is what
            // decides whether to complain: a name nobody shipped is an author's
            // typo and worth one Console line.
            //
            // BOUND — has egui actually been handed it? That is what decides
            // whether to *draw* with it, because epaint PANICS on a
            // `FontFamily::Name` it does not know rather than falling back. The
            // two can disagree for one frame, between a package load and the
            // `set_fonts` that follows it, and a declared face drawing in the
            // editor's type for that frame is the right answer to that.
            let declared = fonts.faces.iter().any(|f| f.family == family);
            if !declared && fonts.warned.borrow_mut().insert(family.clone()) {
                fonts.log.borrow_mut().push(super::ExtLog {
                    level: super::ExtLevel::Warn,
                    msg: format!(
                        "gui.font({name:?}): this package has no font by that name — drawing in \
                         the editor's type. Name it in package.ron: \
                         fonts: [ (name: {name:?}, path: \"fonts/…\") ]"
                    ),
                    from: fonts.pkg_name.to_owned(),
                });
            }
            nest(slot, &cb, move |ui, f| {
                let bound = declared && is_bound(ui, &family);
                // A child scope, so the style change cannot leak past the
                // closure even if the closure raises.
                ui.scope(|inner| {
                    if bound {
                        let fam = egui::FontFamily::Name(family.clone().into());
                        for id in inner.style_mut().text_styles.values_mut() {
                            id.family = fam.clone();
                        }
                    }
                    f(inner);
                });
            })
        })?,
    )?;
    // Did the face this package named actually load? A brand-conscious tool
    // can draw a wordmark as type when it has the face and as an image when it
    // does not, rather than guessing.
    t.set(
        "hasFont",
        scope.create_function(move |_, name: String| {
            let family = crate::fonts::family_key(fonts.pkg_id, &name);
            Ok(fonts.faces.iter().any(|f| f.family == family))
        })?,
    )?;
    t.set(
        "width",
        scope.create_function(move |_, (px, cb): (f32, Function)| {
            nest(slot, &cb, move |ui, f| {
                ui.allocate_ui(egui::vec2(px, ui.available_height()), |inner| f(inner));
            })
        })?,
    )?;
    t.set(
        "height",
        scope.create_function(move |_, (px, cb): (f32, Function)| {
            nest(slot, &cb, move |ui, f| {
                ui.allocate_ui(egui::vec2(ui.available_width(), px), |inner| f(inner));
            })
        })?,
    )?;
    // Push everything after this to the far end of the row. egui has no
    // flexible spacer of its own, but claiming the remaining width is exactly
    // what one is.
    t.set(
        "flexibleSpace",
        scope.create_function(move |_, ()| {
            with(slot, |ui| {
                ui.allocate_space(egui::vec2(ui.available_width(), 0.0));
            })
        })?,
    )?;
    t.set(
        "separator",
        scope.create_function(move |_, ()| with(slot, |ui| ui.separator()).map(|_| ()))?,
    )?;
    t.set(
        "space",
        scope.create_function(move |_, px: Option<f32>| {
            with(slot, |ui| ui.add_space(px.unwrap_or(6.0)))
        })?,
    )?;
    t.set(
        "available",
        scope.create_function(move |lua, ()| {
            let (w, h) = with(slot, |ui| {
                let s = ui.available_size();
                (s.x, s.y)
            })?;
            let t = lua.create_table()?;
            t.set("w", w)?;
            t.set("h", h)?;
            Ok(t)
        })?,
    )?;
    // **Where the next widget would go**, in the same coordinates the painting
    // calls take — i.e. relative to the panel's top-left.
    //
    // Without this the painting calls can only draw one thing. Their origin is
    // the panel's top-left and it does not move as widgets are added, so a
    // second painted card lands exactly on top of the first no matter how much
    // space was reserved between them. A list of painted rows — a chat log, a
    // stack of cards — is not expressible until the cursor can be read.
    t.set(
        "cursor",
        scope.create_function(move |lua, ()| {
            let (x, y) = with(slot, |ui| {
                let o = ui.min_rect().min;
                let c = ui.cursor().min;
                (c.x - o.x, c.y - o.y)
            })?;
            let t = lua.create_table()?;
            t.set("x", x)?;
            t.set("y", y)?;
            Ok(t)
        })?,
    )?;

    // ---- feedback ----------------------------------------------------------
    t.set(
        "progress",
        scope.create_function(move |_, (frac, text): (f32, Option<String>)| {
            with(slot, |ui| {
                let mut bar = egui::ProgressBar::new(frac.clamp(0.0, 1.0));
                if let Some(t) = &text {
                    bar = bar.text(t.clone());
                }
                ui.add(bar);
            })
        })?,
    )?;
    t.set("spinner", scope.create_function(move |_, ()| with(slot, |ui| ui.spinner()).map(|_| ()))?)?;
    // A boxed note: `"info"` (the default), `"warn"` or `"error"`. Its own
    // widget rather than a coloured label, because a tool that has something to
    // tell you should look like it is telling you something.
    t.set(
        "helpBox",
        scope.create_function(move |_, (text, kind): (String, Option<String>)| {
            with(slot, |ui| {
                let (mark, col) = match kind.as_deref() {
                    Some("error") => ("✖", egui::Color32::from_rgb(230, 120, 110)),
                    Some("warn") => ("⚠", egui::Color32::from_rgb(228, 190, 105)),
                    _ => ("ℹ", ui.visuals().weak_text_color()),
                };
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(col, mark);
                        ui.add(egui::Label::new(egui::RichText::new(text).color(col)).wrap());
                    });
                });
            })
        })?,
    )?;

    // ---- painting ----------------------------------------------------------
    // Enough to draw a heatmap, a radar chart or a chat bubble without inventing
    // a widget for each. Coordinates are pixels within the panel: (0,0) is its
    // top-left, which is what a chart author expects and what a screen-space
    // rect in Unity's `GUI` space already is.
    t.set(
        "rectFilled",
        scope.create_function(
            move |_,
                  (x, y, w, h, r, g, b, a, round): (
                f32,
                f32,
                f32,
                f32,
                f64,
                f64,
                f64,
                Option<f64>,
                Option<f32>,
            )| {
                with(slot, |ui| {
                    let o = ui.min_rect().min;
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(o.x + x, o.y + y),
                        egui::vec2(w, h),
                    );
                    ui.painter().rect_filled(rect, round.unwrap_or(0.0), color(r, g, b, a));
                })
            },
        )?,
    )?;
    t.set(
        "rectOutline",
        scope.create_function(
            move |_,
                  (x, y, w, h, r, g, b, a, px): (
                f32,
                f32,
                f32,
                f32,
                f64,
                f64,
                f64,
                Option<f64>,
                Option<f32>,
            )| {
                with(slot, |ui| {
                    let o = ui.min_rect().min;
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(o.x + x, o.y + y),
                        egui::vec2(w, h),
                    );
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(px.unwrap_or(1.0), color(r, g, b, a)),
                        egui::StrokeKind::Inside,
                    );
                })
            },
        )?,
    )?;
    t.set(
        "line",
        scope.create_function(
            move |_,
                  (x1, y1, x2, y2, r, g, b, a, px): (
                f32,
                f32,
                f32,
                f32,
                f64,
                f64,
                f64,
                Option<f64>,
                Option<f32>,
            )| {
                with(slot, |ui| {
                    let o = ui.min_rect().min;
                    ui.painter().line_segment(
                        [egui::pos2(o.x + x1, o.y + y1), egui::pos2(o.x + x2, o.y + y2)],
                        egui::Stroke::new(px.unwrap_or(1.0), color(r, g, b, a)),
                    );
                })
            },
        )?,
    )?;
    t.set(
        "circle",
        scope.create_function(
            move |_, (x, y, rad, r, g, b, a): (f32, f32, f32, f64, f64, f64, Option<f64>)| {
                with(slot, |ui| {
                    let o = ui.min_rect().min;
                    ui.painter().circle_filled(
                        egui::pos2(o.x + x, o.y + y),
                        rad,
                        color(r, g, b, a),
                    );
                })
            },
        )?,
    )?;
    // A filled polygon, from a flat run of `x, y` pairs.
    //
    // The scene side has had `handles.poly` since the beginning; a panel could
    // only outline. That gap is why hand-drawn charts here fill an area by
    // stacking one-pixel rectangles under it — which costs a draw call per
    // column, cannot follow a diagonal edge cleanly, and is impossible for a
    // shape that is not a function of x at all. A radar chart is exactly that
    // shape, and this painter's own promise above names one.
    //
    // Convex, matching `handles.poly`: egui fills a concave outline as its
    // convex hull rather than failing, so a caller who needs a concave shape
    // splits it into triangles — the same rule the scene side already has.
    t.set(
        "poly",
        scope.create_function(
            move |_, (pts, r, g, b, a): (Vec<f32>, f64, f64, f64, Option<f64>)| {
                if pts.len() % 2 != 0 {
                    return Err(mlua::Error::runtime(
                        "gui.poly wants a flat list of x, y pairs, so an even number of \
                         values — got an odd one",
                    ));
                }
                with(slot, |ui| {
                    let o = ui.min_rect().min;
                    let points: Vec<egui::Pos2> = pts
                        .chunks_exact(2)
                        .map(|p| egui::pos2(o.x + p[0], o.y + p[1]))
                        .collect();
                    // Two points are a line and one is nothing: egui would draw
                    // a degenerate shape rather than complain, and a silently
                    // invisible fill reads as "the data was empty".
                    if points.len() >= 3 {
                        ui.painter().add(egui::Shape::convex_polygon(
                            points,
                            color(r, g, b, a),
                            egui::Stroke::NONE,
                        ));
                    }
                })
            },
        )?,
    )?;
    t.set(
        "textAt",
        scope.create_function(
            move |_, (x, y, text, size, r, g, b, a): TextAtArgs| {
                with(slot, |ui| {
                    let o = ui.min_rect().min;
                    let col = match (r, g, b) {
                        (Some(r), Some(g), Some(b)) => color(r, g, b, a),
                        _ => ui.visuals().text_color(),
                    };
                    ui.painter().text(
                        egui::pos2(o.x + x, o.y + y),
                        egui::Align2::LEFT_TOP,
                        text,
                        egui::FontId::proportional(size.unwrap_or(13.0)),
                        col,
                    );
                })
            },
        )?,
    )?;
    // Claim space so painted output is not drawn over by the next widget — the
    // one call a hand-painted chart needs and the one nobody remembers.
    t.set(
        "reserve",
        scope.create_function(move |_, (w, h): (f32, f32)| {
            with(slot, |ui| {
                ui.allocate_space(egui::vec2(w, h));
            })
        })?,
    )?;

    // ---- input -------------------------------------------------------------
    t.set(
        "mouse",
        scope.create_function(move |lua, ()| {
            let (x, y, inside) = with(slot, |ui| {
                let o = ui.min_rect().min;
                match ui.ctx().pointer_latest_pos() {
                    Some(p) => (p.x - o.x, p.y - o.y, ui.min_rect().contains(p)),
                    None => (0.0, 0.0, false),
                }
            })?;
            let t = lua.create_table()?;
            t.set("x", x)?;
            t.set("y", y)?;
            t.set("inside", inside)?;
            Ok(t)
        })?,
    )?;
    // Which modifiers are held, and whether Enter/Escape went down this frame.
    //
    // A package cannot otherwise tell Shift+Enter from Enter, so it cannot
    // offer "Enter for a newline, Shift+Enter to send" — the convention every
    // chat box uses. Read-only and frame-local; it says nothing about the
    // machine beyond which keys are down while its own panel is drawing.
    t.set(
        "keys",
        scope.create_function(move |lua, ()| {
            let (shift, ctrl, alt, enter, escape) = with(slot, |ui| {
                ui.ctx().input(|i| {
                    (
                        i.modifiers.shift,
                        i.modifiers.ctrl || i.modifiers.command,
                        i.modifiers.alt,
                        i.key_pressed(egui::Key::Enter),
                        i.key_pressed(egui::Key::Escape),
                    )
                })
            })?;
            let t = lua.create_table()?;
            t.set("shift", shift)?;
            t.set("ctrl", ctrl)?;
            t.set("alt", alt)?;
            t.set("enter", enter)?;
            t.set("escape", escape)?;
            Ok(t)
        })?,
    )?;
    t.set(
        "clicked",
        scope.create_function(move |_, ()| {
            with(slot, |ui| ui.ctx().input(|i| i.pointer.primary_clicked()))
        })?,
    )?;

    Ok(t)
}
