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
    let frame_seconds = ui.input(|input| input.stable_dt);
    egui::Panel::bottom("status_bar").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(scene_status(state, frame_seconds));
                ui.separator();
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if let Some(entry) = state.log.entries().next_back() {
                        ui.add(egui::Label::new(console::entry_text(entry)).truncate());
                    }
                });
            });
        });
    });
}

/// Сводка справа в строке состояния: сцена, признак правок, число объектов и FPS в Play.
fn scene_status(state: &EditorState, frame_seconds: f32) -> String {
    let Some(scene) = &state.scene else {
        return String::new();
    };
    let mut parts = vec![
        scene
            .path()
            .map_or("untitled", |path| path.as_str())
            .to_owned(),
    ];
    if scene.is_dirty() {
        parts.push("* unsaved".to_owned());
    }
    parts.push(format!("{} objects", scene.document().objects.len()));
    if let Some(play) = &state.play {
        if play.is_paused() {
            parts.push("Paused".to_owned());
        } else if frame_seconds > 0.0 {
            parts.push(format!("{:.0} FPS", 1.0 / frame_seconds));
        }
    }
    parts.join("  |  ")
}

fn show_welcome(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.35);
        ui.heading("RVE Editor");
        ui.add_space(12.0);
        if ui.button("Open Project…").clicked() {
            state.action(Action::OpenProject);
        }
        if !state.recent_projects.is_empty() {
            ui.add_space(16.0);
            ui.label("Recent Projects");
            ui.add_space(4.0);
            recent_projects(ui, state);
        }
    });
}

/// Список недавних проектов. Отсутствующие проекты помечены и недоступны;
/// Remove Missing убирает их из списка.
pub(crate) fn recent_projects(ui: &mut egui::Ui, state: &mut EditorState) {
    let is_project = |dir: &std::path::Path| dir.join("project.rveproject").is_file();
    let mut any_missing = false;
    for dir in &state.recent_projects {
        let exists = is_project(dir);
        any_missing |= !exists;
        let label = if exists {
            dir.display().to_string()
        } else {
            format!("{} (не найден)", dir.display())
        };
        if ui.add_enabled(exists, egui::Button::new(label)).clicked() {
            state.actions.push(Action::OpenProjectAt(dir.clone()));
            ui.close();
        }
    }
    if any_missing && ui.button("Remove Missing").clicked() {
        state.recent_projects.retain(|dir| is_project(dir));
    }
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
