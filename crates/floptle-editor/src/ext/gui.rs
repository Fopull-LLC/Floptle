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

fn color(r: f64, g: f64, b: f64, a: Option<f64>) -> egui::Color32 {
    let f = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    egui::Color32::from_rgba_unmultiplied(f(r), f(g), f(b), f(a.unwrap_or(1.0)))
}

/// Build the `gui` table for one callback. Every function is scoped: it stops
/// working the moment the call returns.
pub(crate) fn bind<'scope, 'env: 'scope>(
    lua: &Lua,
    scope: &'scope Scope<'scope, 'env>,
    slot: &'env RefCell<UiSlot>,
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
                    v
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
    t.set(
        "clicked",
        scope.create_function(move |_, ()| {
            with(slot, |ui| ui.ctx().input(|i| i.pointer.primary_clicked()))
        })?,
    )?;

    Ok(t)
}
