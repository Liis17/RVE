use engine_render::DebugView;

const VIEWS: [(DebugView, &str); 6] = [
    (DebugView::Lit, "Lit"),
    (DebugView::Wireframe, "Wireframe"),
    (DebugView::Normals, "Normals"),
    (DebugView::Overdraw, "Overdraw"),
    (DebugView::LodLevels, "LOD Levels"),
    (DebugView::ShadowCascades, "Shadow Cascades"),
];

pub fn picker(ui: &mut egui::Ui, id: &'static str, view: &mut DebugView, wireframe: bool) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(label(*view))
        .show_ui(ui, |ui| {
            for (candidate, name) in choices(wireframe) {
                ui.selectable_value(view, candidate, name);
            }
        });
}

fn choices(wireframe: bool) -> impl Iterator<Item = (DebugView, &'static str)> {
    VIEWS
        .into_iter()
        .filter(move |(view, _)| *view != DebugView::Wireframe || wireframe)
}

pub const fn label(view: DebugView) -> &'static str {
    match view {
        DebugView::Lit => "Lit",
        DebugView::Wireframe => "Wireframe",
        DebugView::Normals => "Normals",
        DebugView::Overdraw => "Overdraw",
        DebugView::LodLevels => "LOD Levels",
        DebugView::ShadowCascades => "Shadow Cascades",
    }
}

#[cfg(test)]
mod tests {
    use super::{VIEWS, choices, label};
    use engine_render::DebugView;

    #[test]
    fn panel_view_labels_cover_every_renderer_debug_mode() {
        assert_eq!(VIEWS.len(), 6);
        assert_eq!(label(DebugView::Lit), "Lit");
        assert_eq!(label(DebugView::ShadowCascades), "Shadow Cascades");
    }

    #[test]
    fn unsupported_wireframe_is_absent_from_view_choices() {
        assert_eq!(choices(false).count(), 5);
        assert_eq!(choices(true).count(), 6);
        assert!(!choices(false).any(|(view, _)| view == DebugView::Wireframe));
    }
}
