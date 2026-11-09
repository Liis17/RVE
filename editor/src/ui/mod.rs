//! Панели и диалоги редактора.

pub mod asset_inspector;
pub mod console;
mod debug_view;
pub mod game_view;
pub mod hierarchy;
pub mod inspector;
pub mod material_preview;
pub mod profiler;
pub mod project_panel;
pub mod project_prefabs;
pub mod scene_assets;
pub mod scene_view;
pub mod toolbar;

#[cfg(test)]
mod tests;

use crate::layout::{EditorTabs, Tab};
use crate::state::{Action, AssetDrag, EditorState};
use egui_dock::{DockArea, DockState, Style};

/// Строит весь интерфейс кадра.
pub fn show(ui: &mut egui::Ui, state: &mut EditorState, dock: &mut DockState<Tab>) {
    // Видимость и наведение панелей известны только с прошлого кадра: сброс ниже их стирает.
    // В Play numpad-команды Scene действуют, лишь если Scene показана и курсор не над Game.
    if state.is_playing() && (!state.scene_view.visible || state.game_view.hovered) {
        state.view_commands.clear();
    }
    state.scene_view.visible = false;
    state.scene_view.hovered = false;
    state.scene_view.rect = None;
    state.game_view.visible = false;
    state.game_view.hovered = false;
    state.game_view.rect = None;
    if state.ui_editor_rect.is_none() {
        state.ui_editor_active = false;
    }
    if ui.input(|i| i.pointer.any_pressed()) {
        state.ui_editor_active = state
            .ui_editor_rect
            .is_some_and(|r| ui.input(|i| i.pointer.interact_pos().is_some_and(|p| r.contains(p))));
    }
    state.material_preview.visible = false;
    if std::mem::take(&mut state.preview_requested) {
        if let Some(path) = dock.find_tab(&Tab::MaterialPreview) {
            let _ = dock.set_active_tab(path);
        } else {
            dock.push_to_first_leaf(Tab::MaterialPreview);
        }
    }
    toolbar::handle_shortcuts(ui.ctx(), state);
    state.ui_editor_rect = None;
    if std::mem::take(&mut state.ui_editor_requested) {
        if !crate::layout::is_open(dock, Tab::UiEditor) {
            if let Some(scene) = dock
                .find_tab(&Tab::Scene)
                .or_else(|| dock.find_tab(&Tab::Game))
            {
                dock.set_focused_node_and_surface(scene.node_path());
            }
            dock.push_to_focused_leaf(Tab::UiEditor);
        }
        if let Some(path) = dock.find_tab(&Tab::UiEditor) {
            let _ = dock.set_active_tab(path);
        }
    }
    state.ui_authoring = dock.find_tab(&Tab::UiEditor).is_some_and(|path| {
        dock.leaf(path.node_path())
            .is_ok_and(|leaf| leaf.active == path.tab && !leaf.collapsed)
    }) && state.ui_document.is_some();
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
    show_apply_prompt(ui.ctx(), state);
    show_recovery_prompt(ui.ctx(), state);
    project_panel::show_dialog(ui.ctx(), state);
    crate::ui_editor::pending_dialog(ui.ctx(), state);
    show_new_project_dialog(ui.ctx(), state);
    state.smart_zoom = false;
    state.text_focused_last_frame = ui.ctx().egui_wants_keyboard_input();
}

fn show_new_project_dialog(ctx: &egui::Context, state: &mut EditorState) {
    let Some(mut dialog) = state.new_project_dialog.take() else {
        return;
    };
    let mut close = false;
    egui::Modal::new(egui::Id::new("new_project")).show(ctx, |ui| {
        ui.set_min_width(420.0);
        ui.heading("New Project");
        ui.label("Project name");
        ui.text_edit_singleline(&mut dialog.name);
        ui.add_space(6.0);
        ui.label("Parent folder");
        ui.horizontal(|ui| {
            ui.add_sized(
                [ui.available_width() - 104.0, 24.0],
                egui::TextEdit::singleline(&mut dialog.parent),
            );
            if ui.button("Browse…").clicked()
                && let Some(parent) = rfd::FileDialog::new().pick_folder()
            {
                dialog.parent = parent.display().to_string();
            }
        });
        ui.label(
            std::path::Path::new(&dialog.parent)
                .join(dialog.name.trim())
                .display()
                .to_string(),
        );
        if let Some(error) = &dialog.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!dialog.name.trim().is_empty(), egui::Button::new("Create"))
                .clicked()
            {
                state.action(Action::CreateProject {
                    name: dialog.name.clone(),
                    parent: std::path::PathBuf::from(&dialog.parent),
                });
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if !close {
        state.new_project_dialog = Some(dialog);
    }
}

fn show_recovery_prompt(ctx: &egui::Context, state: &mut EditorState) {
    let Some(prompt) = &state.recovery_prompt else {
        return;
    };
    let name = prompt
        .scene
        .as_ref()
        .map_or("untitled", |path| path.as_str())
        .to_owned();
    egui::Modal::new(egui::Id::new("recover_autosave")).show(ctx, |ui| {
        ui.set_max_width(380.0);
        ui.heading("Autosave found");
        ui.label(format!(
            "Найдены несохранённые изменения {name} от прошлого сеанса. Восстановить их?"
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Restore").clicked() {
                state.action(Action::RestoreAutosave);
            }
            if ui.button("Discard").clicked() {
                state.action(Action::DiscardAutosave);
            }
        });
    });
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

/// Подтверждение переноса overrides в файл префаба: он меняется для всех сцен.
fn show_apply_prompt(ctx: &egui::Context, state: &mut EditorState) {
    let Some(id) = state.apply_prompt.clone() else {
        return;
    };
    egui::Modal::new(egui::Id::new("apply_to_prefab")).show(ctx, |ui| {
        ui.set_max_width(360.0);
        ui.heading("Apply to Prefab");
        ui.label(format!(
            "Перенести overrides {id:?} в файл префаба? Изменение увидят все экземпляры, undo сцены его не отменит."
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                state.apply_prompt = None;
                state.action(Action::ApplyToPrefab(id.clone()));
            }
            if ui.button("Cancel").clicked() {
                state.apply_prompt = None;
            }
        });
    });
}

/// Текстовое поле, которое отдаёт значение только при уходе фокуса или Enter: промежуточный
/// текст не попадает в документ. Пока фокуса нет, поле показывает текущее значение. Уход фокуса
/// без изменения значения ничего не отдаёт, поэтому не создаёт пустую правку в undo.
pub(crate) fn committed_text_field(
    ui: &mut egui::Ui,
    id: egui::Id,
    current: &str,
    hint: &str,
    width: f32,
) -> Option<String> {
    let mut text = ui
        .ctx()
        .data(|data| data.get_temp::<String>(id))
        .unwrap_or_else(|| current.to_owned());
    let response = ui.add(
        egui::TextEdit::singleline(&mut text)
            .hint_text(hint)
            .desired_width(width),
    );
    let committed = (response.lost_focus() && text != current).then(|| text.clone());
    if response.has_focus() {
        ui.ctx().data_mut(|data| data.insert_temp(id, text));
    } else {
        ui.ctx().data_mut(|data| data.remove::<String>(id));
    }
    committed
}

/// Разбор лимита экземпляров: пустой текст — без лимита, иначе целое число. Ноль отклоняет
/// валидация документа, поэтому здесь он проходит.
pub(crate) fn parse_max_instances(text: &str) -> Result<Option<u32>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u32>()
        .map(Some)
        .map_err(|_| format!("max_instances: ожидается целое число, получено {text:?}"))
}

/// Полоса, принимающая `.rveprefab`, перетащенный из Project. Возвращает путь относительно
/// `Assets` при отпускании; тип ресурса проверяет модель, которая получит путь.
pub(crate) fn prefab_drop_zone(ui: &mut egui::Ui, text: &str) -> Option<String> {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    let stroke = if response.dnd_hover_payload::<AssetDrag>().is_some() {
        ui.visuals().selection.stroke
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    let color = ui.visuals().text_color();
    let painter = ui.painter();
    painter.rect_stroke(rect, 4.0, stroke, egui::StrokeKind::Inside);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(12.0),
        color,
    );
    response
        .dnd_release_payload::<AssetDrag>()
        .map(|asset| asset.0.clone())
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
