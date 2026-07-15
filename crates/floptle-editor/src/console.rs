//! The engine Console: the log store (severity, merge-duplicates, source
//! locations) and its dock tab UI (filter toolbar, search, jump-to-source).

use crate::EditorTabViewer;

/// One line in the engine Console. Consecutive identical lines are merged at ingest
/// (`count`), and `source` (script name + line) drives double-click-to-source.
pub(crate) struct ConsoleEntry {
    pub(crate) level: floptle_script::LogLevel,
    pub(crate) msg: String,
    pub(crate) source: Option<(String, u32)>,
    pub(crate) count: u32,
}

/// Console view state: which severities show, the search filter, and whether to
/// merge non-adjacent duplicates into one counted row.
pub(crate) struct ConsoleState {
    pub(crate) entries: Vec<ConsoleEntry>,
    pub(crate) show_debug: bool,
    pub(crate) show_warn: bool,
    pub(crate) show_error: bool,
    pub(crate) search: String,
    pub(crate) collapse: bool,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            show_debug: true,
            show_warn: true,
            show_error: true,
            search: String::new(),
            collapse: true,
        }
    }
}

impl ConsoleState {
    /// Append a line, merging it into the previous row if identical (so a per-frame
    /// repeat becomes a count, not a flood). Caps retained history.
    pub(crate) fn push(&mut self, level: floptle_script::LogLevel, msg: String, source: Option<(String, u32)>) {
        if let Some(last) = self.entries.last_mut()
            && last.level == level && last.msg == msg {
                last.count += 1;
                return;
            }
        self.entries.push(ConsoleEntry { level, msg, source, count: 1 });
        const MAX: usize = 2000;
        if self.entries.len() > MAX {
            let drop = self.entries.len() - MAX;
            self.entries.drain(0..drop);
        }
    }
}
impl EditorTabViewer<'_> {
    /// The engine Console: a filterable, searchable feed of script `print`/`log`
    /// output, warnings and errors. Double-click a line to open its source.
    pub(crate) fn console_ui(&mut self, ui: &mut egui::Ui) {
        use floptle_script::LogLevel;
        let c = &mut *self.console;

        // Tally per-severity counts (summing merged duplicates).
        let (mut nd, mut nw, mut ne) = (0u32, 0u32, 0u32);
        for e in &c.entries {
            match e.level {
                LogLevel::Debug => nd += e.count,
                LogLevel::Warn => nw += e.count,
                LogLevel::Error => ne += e.count,
            }
        }

        // ---- toolbar: severity toggles, collapse, search, copy, clear ----
        let mut do_copy = false;
        let mut do_clear = false;
        ui.horizontal_wrapped(|ui| {
            ui.toggle_value(&mut c.show_debug, format!("· {nd}")).on_hover_text("messages");
            ui.toggle_value(&mut c.show_warn, format!("Δ {nw}")).on_hover_text("warnings");
            ui.toggle_value(&mut c.show_error, format!("⊗ {ne}")).on_hover_text("errors");
            ui.separator();
            ui.toggle_value(&mut c.collapse, "⊟").on_hover_text("collapse duplicate lines");
            ui.separator();
            ui.label("○");
            ui.add(
                egui::TextEdit::singleline(&mut c.search)
                    .hint_text("search")
                    .desired_width(150.0),
            );
            if !c.search.is_empty() && ui.small_button("×").clicked() {
                c.search.clear();
            }
            ui.separator();
            if ui.button("⎘ Copy").on_hover_text("copy the visible lines").clicked() {
                do_copy = true;
            }
            if ui.button("🗑 Clear").clicked() {
                do_clear = true;
            }
        });
        ui.separator();

        // ---- build the visible row set: filter, then optionally fully collapse ----
        let needle = c.search.to_ascii_lowercase();
        let passes = |e: &ConsoleEntry| {
            let on = match e.level {
                LogLevel::Debug => c.show_debug,
                LogLevel::Warn => c.show_warn,
                LogLevel::Error => c.show_error,
            };
            if !on {
                return false;
            }
            if needle.is_empty() {
                return true;
            }
            e.msg.to_ascii_lowercase().contains(&needle)
                || e.source.as_ref().is_some_and(|(n, _)| n.to_ascii_lowercase().contains(&needle))
        };
        /// One visible row: (level, message, jump-to source, merged count).
        type Row<'a> = (LogLevel, &'a str, Option<&'a (String, u32)>, u32);
        let mut rows: Vec<Row> = Vec::new();
        if c.collapse {
            // Merge identical messages across the whole feed into one counted row.
            let mut idx: std::collections::HashMap<(u8, &str), usize> = std::collections::HashMap::new();
            for e in c.entries.iter().filter(|e| passes(e)) {
                let key = (e.level as u8, e.msg.as_str());
                if let Some(&r) = idx.get(&key) {
                    rows[r].3 += e.count;
                } else {
                    idx.insert(key, rows.len());
                    rows.push((e.level, &e.msg, e.source.as_ref(), e.count));
                }
            }
        } else {
            for e in c.entries.iter().filter(|e| passes(e)) {
                rows.push((e.level, &e.msg, e.source.as_ref(), e.count));
            }
        }

        if do_copy {
            let mut text = String::new();
            for (lvl, msg, src, n) in &rows {
                let tag = match lvl {
                    LogLevel::Debug => "log",
                    LogLevel::Warn => "warn",
                    LogLevel::Error => "error",
                };
                if let Some((name, line)) = src {
                    text.push_str(&format!("[{tag}] {name}:{line}: {msg}"));
                } else {
                    text.push_str(&format!("[{tag}] {msg}"));
                }
                if *n > 1 {
                    text.push_str(&format!("  (x{n})"));
                }
                text.push('\n');
            }
            ui.ctx().copy_text(text);
        }

        // ---- the log list ----
        let mut jump: Option<(String, u32)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.weak("No console output. Press F1 to play — script print/log and errors appear here.");
                }
                for (i, (lvl, msg, src, n)) in rows.iter().enumerate() {
                    let color = match lvl {
                        LogLevel::Debug => egui::Color32::from_gray(205),
                        LogLevel::Warn => egui::Color32::from_rgb(240, 200, 90),
                        LogLevel::Error => egui::Color32::from_rgb(235, 95, 95),
                    };
                    let icon = match lvl {
                        LogLevel::Debug => "·",
                        LogLevel::Warn => "Δ",
                        LogLevel::Error => "⊗",
                    };
                    // Structured prints (tables, nodes — anything multi-line)
                    // fold into a collapsible block titled by their first line,
                    // so a big dump never floods the feed.
                    if msg.contains('\n') {
                        let first = msg.lines().next().unwrap_or("");
                        let lines = msg.lines().count();
                        let title = egui::RichText::new(format!(
                            "{icon} {first} ⋯ ({lines} lines){}",
                            if *n > 1 { format!("  ×{n}") } else { String::new() }
                        ))
                        .color(color)
                        .monospace();
                        let r = egui::CollapsingHeader::new(title)
                            .id_salt(("console-block", i))
                            .show(ui, |ui| {
                                if let Some((name, line)) = src
                                    && ui
                                        .link(
                                            egui::RichText::new(format!("{name}:{line}"))
                                                .monospace(),
                                        )
                                        .clicked()
                                {
                                    jump = Some(((*name).clone(), *line));
                                }
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(*msg).color(color).monospace(),
                                    )
                                    .selectable(true),
                                );
                            });
                        if r.header_response.double_clicked()
                            && let Some((name, line)) = src
                        {
                            jump = Some(((*name).clone(), *line));
                        }
                        continue;
                    }
                    let resp = ui
                        .horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            if let Some((name, line)) = src {
                                // The file:line is a link — a single click jumps to that
                                // exact line in the editor.
                                if ui
                                    .link(egui::RichText::new(format!("{name}:{line}")).monospace())
                                    .on_hover_text("click to open this line in the editor")
                                    .clicked()
                                {
                                    jump = Some(((*name).clone(), *line));
                                }
                            }
                            // Selectable so you can drag-select + copy a line; a
                            // double-click on the row still jumps to its source too.
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!("{icon} {msg}")).color(color).monospace(),
                                )
                                .selectable(true),
                            )
                        })
                        .inner;
                    if *n > 1 {
                        // count badge sits at the row's right edge.
                        let badge = format!("×{n}");
                        ui.painter().text(
                            egui::pos2(resp.rect.right() + 26.0, resp.rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            badge,
                            egui::FontId::monospace(11.0),
                            egui::Color32::from_gray(140),
                        );
                    }
                    if resp.double_clicked()
                        && let Some((name, line)) = src {
                            jump = Some(((*name).clone(), *line));
                        }
                    resp.on_hover_text("click the file:line to open the source (or double-click the row)");
                }
            });

        if do_clear {
            c.entries.clear();
        }
        if let Some(j) = jump {
            self.cmd.open_log_source = Some(j);
        }
    }
}
