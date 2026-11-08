//! Подокно «Project Prefabs»: список тяжёлых префабов `prefabs.rveprefabs` проекта.

use crate::assets_fs::AssetKind;
use crate::project_prefabs::ProjectPrefabs;
use crate::state::EditorState;
use crate::ui::{committed_text_field, parse_max_instances, prefab_drop_zone};

/// Действие пользователя. Применяется после отрисовки, когда список снят с состояния.
enum Command {
    Add(String),
    Remove(usize),
    Archive(usize, Option<String>),
    MaxInstances(usize, Result<Option<u32>, String>),
    Save,
    Reload,
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some(project) = state.project.as_ref() else {
        ui.label("Откройте проект");
        return;
    };
    let root = project.root.clone();
    // Список выносится из состояния на время кадра: команды меняют его, а Assets читаются из состояния.
    let mut prefabs = match state.project_prefabs.take() {
        Some(prefabs) if prefabs.root() == root => prefabs,
        _ => ProjectPrefabs::open(&root),
    };
    let listed: Vec<String> = prefabs
        .entries()
        .iter()
        .map(|entry| entry.path.trim_start_matches('/').to_owned())
        .collect();
    let options: Vec<String> = state
        .assets_of_kind(AssetKind::Prefab)
        .into_iter()
        .filter(|option| !listed.contains(option))
        .collect();
    let entries = prefabs.entries().to_vec();
    let dirty = prefabs.is_dirty();
    let mut commands = Vec::new();

    ui.horizontal(|ui| {
        ui.menu_button("Add Prefab", |ui| {
            if options.is_empty() {
                ui.label("В Assets нет других .rveprefab");
            }
            for option in &options {
                if ui.button(option).clicked() {
                    commands.push(Command::Add(option.clone()));
                    ui.close();
                }
            }
        });
        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
            commands.push(Command::Save);
        }
        if ui
            .button("Reload")
            .on_hover_text("Перечитать файл; несохранённые правки будут потеряны")
            .clicked()
        {
            commands.push(Command::Reload);
        }
        if dirty {
            ui.label("несохранённые правки");
        }
    });
    if let Some(asset) = prefab_drop_zone(ui, "Перетащите сюда .rveprefab из Project")
    {
        commands.push(Command::Add(asset));
    }

    if entries.is_empty() {
        ui.label("Список пуст: тяжёлые префабы не загружаются во время игры");
    }
    egui::Grid::new("project_prefab_entries")
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            for (index, entry) in entries.iter().enumerate() {
                ui.label(&entry.path);
                let archive_id = ui.make_persistent_id(("project_prefab_archive", index));
                let archive = entry.archive.as_deref().unwrap_or_default();
                if let Some(text) = committed_text_field(ui, archive_id, archive, "archive", 120.0)
                {
                    commands.push(Command::Archive(index, (!text.is_empty()).then_some(text)));
                }
                let max_id = ui.make_persistent_id(("project_prefab_max", index));
                let max = entry
                    .max_instances
                    .map(|max| max.to_string())
                    .unwrap_or_default();
                if let Some(text) = committed_text_field(ui, max_id, &max, "без лимита", 80.0)
                {
                    commands.push(Command::MaxInstances(index, parse_max_instances(&text)));
                }
                if ui.button("Remove").clicked() {
                    commands.push(Command::Remove(index));
                }
                ui.end_row();
            }
        });

    // Отклонённые правки остаются в списке как `error()`, поэтому их результат здесь не нужен.
    for command in commands {
        match command {
            Command::Add(asset) => {
                let _ = prefabs.add(&asset);
            }
            Command::Remove(index) => {
                let _ = prefabs.remove(index);
            }
            Command::Archive(index, archive) => {
                let _ = prefabs.set_archive(index, archive);
            }
            Command::MaxInstances(index, Ok(max)) => {
                let _ = prefabs.set_max_instances(index, max);
            }
            Command::MaxInstances(_, Err(error)) => state.log.error(error),
            Command::Save => {
                let _ = prefabs.save();
            }
            Command::Reload => prefabs = ProjectPrefabs::open(&root),
        }
    }
    if let Some(error) = prefabs.error() {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    state.project_prefabs = Some(prefabs);
}
