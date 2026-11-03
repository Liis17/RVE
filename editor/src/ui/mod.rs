//! Панели и диалоги редактора.

pub mod console;
pub mod game_view;
pub mod hierarchy;
pub mod inspector;
pub mod project_panel;
pub mod scene_view;
pub mod toolbar;

#[cfg(test)]
mod tests;

use crate::layout::{EditorTabs, Tab};
use crate::state::{Action, EditorState};
use egui_dock::{DockArea, DockState, Style};

/// Строит весь интерфейс кадра.
pub fn show(ui: &mut egui::Ui, state: &mut EditorState, dock: &mut DockState<Tab>) {
    state.scene_view.visible = false;
    state.scene_view.hovered = false;
    state.game_view.visible = false;
    state.game_view.hovered = false;
    toolbar::handle_shortcuts(ui.ctx(), state);
    toolbar::show(ui, state, dock);
    show_status_bar(ui, state);

    if state.project.is_none() {
        egui::CentralPanel::default().show(ui, |ui| show_welcome(ui, state));
    } else {
        DockArea::new(dock)
            .style(Style::from_egui(ui.style().as_ref()))
            .show_inside(ui, &mut EditorTabs { state });
    }

    show_unsaved_prompt(ui.ctx(), state);
    project_panel::show_dialog(ui.ctx(), state);
}

fn show_status_bar(ui: &mut egui::Ui, state: &EditorState) {
    egui::Panel::bottom("status_bar").show(ui, |ui| {
        ui.horizontal(|ui| {
            let latest = state.log.entries().next_back();
            match latest {
                Some(entry) => ui.label(console::entry_text(entry)),
                None => ui.label(""),
            };
        });
    });
}

fn show_welcome(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.35);
        ui.heading("RVE Editor");
        ui.add_space(12.0);
        if ui.button("Open Project…").clicked() {
            state.action(Action::OpenProject);
        }
    });
}

fn show_unsaved_prompt(ctx: &egui::Context, state: &mut EditorState) {
    let Some(prompt) = state.unsaved_prompt.clone() else {
        return;
    };
    let name = state
        .scene
        .as_ref()
        .and_then(|scene| scene.path())
        .map_or("untitled", |path| path.as_str())
        .to_owned();
    egui::Modal::new(egui::Id::new("unsaved_changes")).show(ctx, |ui| {
        ui.set_max_width(360.0);
        ui.heading("Unsaved changes");
        ui.label(format!("Сохранить изменения в {name}?"));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                state.unsaved_prompt = None;
                state.action(Action::SaveThen(Box::new(prompt.then.clone())));
            }
            if ui.button("Discard").clicked() {
                state.unsaved_prompt = None;
                state.action(Action::Discard(Box::new(prompt.then.clone())));
            }
            if ui.button("Cancel").clicked() {
                state.unsaved_prompt = None;
            }
        });
    });
}
