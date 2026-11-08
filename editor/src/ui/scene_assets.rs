//! Подокно «Scene Assets»: `.rveprefab` в `[[assets]]` открытой сцены.

use crate::assets_fs::AssetKind;
use crate::state::EditorState;
use crate::ui::{committed_text_field, parse_max_instances, prefab_drop_zone};
use engine_files::VirtualPath;

/// Префаб сцены, снятый с документа до отрисовки: заимствование сцены держать нельзя.
struct Row {
    path: VirtualPath,
    used: bool,
    archive: Option<String>,
    max_instances: Option<u32>,
}

/// Правка строки. Применяется после отрисовки, как и в прочих панелях.
enum Edit {
    Archive(VirtualPath, Option<String>),
    MaxInstances(VirtualPath, Result<Option<u32>, String>),
    Remove(VirtualPath),
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some(scene) = state.scene.as_ref() else {
        ui.label("Сцена не открыта");
        return;
    };
    let document = scene.document();
    let rows: Vec<Row> = document
        .assets
        .iter()
        .filter(|asset| AssetKind::of(asset.path.as_str()) == AssetKind::Prefab)
        .map(|asset| Row {
            path: asset.path.clone(),
            used: document
                .instances
                .iter()
                .any(|instance| instance.prefab == asset.path),
            archive: asset.archive.clone(),
            max_instances: asset.max_instances,
        })
        .collect();
    let options: Vec<String> = state
        .assets_of_kind(AssetKind::Prefab)
        .into_iter()
        .filter(|option| rows.iter().all(|row| row.path.as_str() != option.as_str()))
        .collect();

    let mut added = None;
    let mut edits = Vec::new();
    ui.horizontal(|ui| {
        ui.menu_button("Add Prefab", |ui| {
            if options.is_empty() {
                ui.label("В Assets нет других .rveprefab");
            }
            for option in &options {
                if ui.button(option).clicked() {
                    added = Some(option.clone());
                    ui.close();
                }
            }
        });
    });
    if let Some(asset) = prefab_drop_zone(ui, "Перетащите сюда .rveprefab из Project")
    {
        added = Some(asset);
    }

    if rows.is_empty() {
        ui.label("В ресурсах сцены нет .rveprefab");
    }
    egui::Grid::new("scene_prefab_assets")
        .num_columns(5)
        .striped(true)
        .show(ui, |ui| {
            for row in &rows {
                let key = row.path.as_str();
                ui.label(format!("/{key}"));
                if row.used {
                    ui.label("используется экземпляром");
                } else {
                    ui.label("нет экземпляра в сцене").on_hover_text(
                        "Префаб может спавниться скриптами (rve.spawn) во время игры",
                    );
                }
                let archive_id = ui.make_persistent_id(("scene_prefab_archive", key));
                let archive = row.archive.as_deref().unwrap_or_default();
                if let Some(text) = committed_text_field(ui, archive_id, archive, "archive", 120.0)
                {
                    let archive = (!text.is_empty()).then_some(text);
                    edits.push(Edit::Archive(row.path.clone(), archive));
                }
                let max_id = ui.make_persistent_id(("scene_prefab_max", key));
                let max = row
                    .max_instances
                    .map(|max| max.to_string())
                    .unwrap_or_default();
                if let Some(text) = committed_text_field(ui, max_id, &max, "без лимита", 80.0)
                {
                    edits.push(Edit::MaxInstances(
                        row.path.clone(),
                        parse_max_instances(&text),
                    ));
                }
                let remove = ui
                    .add_enabled(!row.used, egui::Button::new("Remove"))
                    .on_disabled_hover_text(
                        "Префаб используется экземпляром: сначала удалите экземпляр",
                    );
                if remove.clicked() {
                    edits.push(Edit::Remove(row.path.clone()));
                }
                ui.end_row();
            }
        });

    if let Some(asset) = added {
        match VirtualPath::parse(&asset) {
            Ok(path) => {
                state.edit_scene(|scene| scene.add_scene_prefab(path));
            }
            Err(_) => state
                .log
                .error(format!("{asset}: некорректный путь ресурса")),
        }
    }
    for edit in edits {
        match edit {
            Edit::Archive(path, archive) => {
                state.edit_scene(|scene| scene.set_scene_prefab_archive(&path, archive));
            }
            Edit::MaxInstances(path, Ok(max)) => {
                state.edit_scene(|scene| scene.set_scene_prefab_max_instances(&path, max));
            }
            Edit::MaxInstances(_, Err(error)) => state.log.error(error),
            Edit::Remove(path) => {
                state.edit_scene(|scene| scene.remove_scene_prefab(&path));
            }
        }
    }
}
