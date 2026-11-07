//! Вкладка Profiler: замеры renderer, GPU по видам Scene и Game и шага Play.

use crate::state::EditorState;

pub fn show(ui: &mut egui::Ui, state: &EditorState) {
    let report = state.profile.report();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| engine_ui::profiler::show(ui, &report));
}
