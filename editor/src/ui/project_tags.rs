//! Вкладка «Project Settings»: теги и слои проекта. Слой объекта — индекс в списке `[layers]`.

use crate::project_tags::TagsLayersDraft;
use crate::state::EditorState;

/// Действие пользователя. Применяется после отрисовки, когда черновик снят с состояния.
enum Command {
    AddTag,
    RemoveTag(usize),
    AddLayer,
    RemoveLayer(usize),
    Save,
    Revert,
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some(project) = state.project.as_ref() else {
        ui.label("Откройте проект");
        return;
    };
    let root = project.root.clone();
    let mut draft = match state.tags_layers.take() {
        Some(draft) if draft.root() == root => draft,
        _ => TagsLayersDraft::open(&root, &project.settings),
    };
    let dirty = draft.is_dirty();
    let can_add_layer = draft.can_add_layer();
    let mut commands = Vec::new();

    ui.horizontal(|ui| {
        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
            commands.push(Command::Save);
        }
        if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
            commands.push(Command::Revert);
        }
    });
    if let Some(error) = &draft.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    ui.separator();

    ui.strong("Tags");
    ui.label("Назначаются объектам в Inspector; пустой тег — Untagged.");
    for (index, name) in draft.tags_mut().iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.text_edit_singleline(name);
            if ui.small_button("✕").on_hover_text("Удалить тег").clicked() {
                commands.push(Command::RemoveTag(index));
            }
        });
    }
    if ui.button("Add Tag").clicked() {
        commands.push(Command::AddTag);
    }
    ui.separator();

    ui.strong("Layers");
    ui.label(
        "Индекс — значение layer объекта. ✕ очищает имя, индексы остальных слоёв не меняются.",
    );
    for (index, name) in draft.layers_mut().iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.label(format!("{index}"));
            ui.text_edit_singleline(name);
            if ui
                .small_button("✕")
                .on_hover_text("Очистить имя слоя")
                .clicked()
            {
                commands.push(Command::RemoveLayer(index));
            }
        });
    }
    if ui
        .add_enabled(can_add_layer, egui::Button::new("Add Layer"))
        .clicked()
    {
        commands.push(Command::AddLayer);
    }

    ui.separator();
    ui.strong("Collision Matrix");
    ui.label("Пара слоёв сталкивается, если отмечена в обе стороны. По умолчанию все со всеми.");
    let named: Vec<usize> = draft
        .layers()
        .iter()
        .enumerate()
        .filter(|(_, name)| !name.is_empty())
        .map(|(index, _)| index)
        .collect();
    if named.is_empty() {
        ui.label("Назначьте имена слоям, чтобы редактировать матрицу.");
    } else {
        egui::ScrollArea::both().max_height(360.0).show(ui, |ui| {
            egui::Grid::new("collision_matrix").show(ui, |ui| {
                ui.label("");
                for &column in &named {
                    ui.label(format!("{column}"));
                }
                ui.end_row();
                for &row in &named {
                    ui.label(format!("{row} {}", draft.layers()[row]));
                    for &column in &named {
                        let mut enabled = draft.pair_enabled(row, column);
                        if ui.checkbox(&mut enabled, "").changed() {
                            draft.set_pair(row, column, enabled);
                        }
                    }
                    ui.end_row();
                }
            });
        });
    }

    for command in commands {
        match command {
            Command::AddTag => {
                let next = format!("tag_{}", draft.tags_mut().len() + 1);
                draft.tags_mut().push(next);
            }
            Command::RemoveTag(index) => {
                if let Some(name) = draft.remove_tag(index) {
                    state.log.warn(format!(
                        "тег «{name}» удалён из [tags]: объекты сцены с ним сохранят тег, но он не записан в списке"
                    ));
                }
            }
            Command::AddLayer => {
                let next = format!("layer_{}", draft.layers_mut().len());
                draft.layers_mut().push(next);
            }
            Command::RemoveLayer(index) => draft.clear_layer(index),
            Command::Revert => draft.revert(),
            Command::Save => {
                if let Some(project) = state.project.as_mut() {
                    match draft.save(&project.settings) {
                        Ok(settings) => project.settings = settings,
                        Err(error) => state.log.error(error),
                    }
                }
            }
        }
    }
    state.tags_layers = Some(draft);
}
