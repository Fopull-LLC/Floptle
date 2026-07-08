//! Shared editor widgets.

/// A ComboBox over a long asset list with a search box. The search field
/// AUTO-FOCUSES when the popup opens (type immediately, no click needed),
/// clicking inside the popup does NOT close it (CloseOnClickOutside), and
/// picking an entry closes explicitly. Returns `Some(pick)` when something was
/// chosen this frame — `Some(None)` is the `none_label` entry.
///
/// Every long-list dropdown (textures, models) goes through here so they all
/// behave identically.
pub(crate) fn searchable_picker(
    ui: &mut egui::Ui,
    id: egui::Id,
    selected_text: &str,
    none_label: Option<&str>,
    items: &[String],
    width: f32,
) -> Option<Option<String>> {
    let mut picked: Option<Option<String>> = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected_text.to_string())
        .width(width)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            let sid = id.with("search");
            let mut q: String = ui.data_mut(|d| d.get_temp(sid).unwrap_or_default());
            let resp = ui.add(
                egui::TextEdit::singleline(&mut q).hint_text("🔍 search…").desired_width(f32::INFINITY),
            );
            // Grab the keyboard the moment the popup opens.
            if ui.memory(|m| m.focused().is_none()) {
                resp.request_focus();
            }
            ui.data_mut(|d| d.insert_temp(sid, q.clone()));
            let done = |ui: &mut egui::Ui| {
                ui.data_mut(|d| d.insert_temp(sid, String::new()));
                ui.close();
            };
            if let Some(nl) = none_label
                && ui.selectable_label(selected_text == nl, nl).clicked()
            {
                picked = Some(None);
                done(ui);
            }
            let ql = q.to_lowercase();
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for p in items.iter().filter(|p| ql.is_empty() || p.to_lowercase().contains(&ql)) {
                    let name = p.rsplit('/').next().unwrap_or(p);
                    if ui
                        .selectable_label(selected_text == name, name)
                        .on_hover_text(p.as_str())
                        .clicked()
                    {
                        picked = Some(Some(p.clone()));
                        done(ui);
                    }
                }
            });
        });
    picked
}
