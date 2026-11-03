//! Панель Project: файловый менеджер каталога `Assets/`.

use crate::assets_fs::{AssetEntry, AssetKind};
use crate::state::{Action, AssetDrag, EditorState};

const TILE_SIZE: f32 = 88.0;

/// Открытый диалог файловой операции.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectDialog {
    NewFolder { name: String },
    Rename { path: String, name: String },
    Delete { path: String, is_dir: bool },
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        if ui.button("Refresh").clicked() {
            state.action(Action::RefreshAssets);
        }
        if ui.button("New Folder").clicked() {
            state.project_dialog = Some(ProjectDialog::NewFolder {
                name: String::new(),
            });
        }
        if ui.button("Import…").clicked() {
            state.action(Action::ImportDialog);
        }
        ui.separator();
        let folder = if state.current_folder.is_empty() {
            "Assets".to_owned()
        } else {
            format!("Assets/{}", state.current_folder)
        };
        ui.label(folder);
    });
    ui.separator();
    egui::Panel::left("project_tree")
        .default_size(180.0)
        .show(ui, |ui| {
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                let root_selected = state.current_folder.is_empty();
                if ui.selectable_label(root_selected, "Assets").clicked() {
                    state.current_folder.clear();
                }
                let tree = std::mem::take(&mut state.asset_tree);
                folder_tree(ui, state, &tree);
                state.asset_tree = tree;
            });
        });
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                let tree = std::mem::take(&mut state.asset_tree);
                let entries = find_folder(&tree, &state.current_folder).unwrap_or(&tree);
                ui.horizontal_wrapped(|ui| {
                    for entry in entries {
                        tile(ui, state, entry);
                    }
                });
                state.asset_tree = tree;
            });
    });
}

fn folder_tree(ui: &mut egui::Ui, state: &mut EditorState, entries: &[AssetEntry]) {
    for entry in entries.iter().filter(|entry| entry.is_dir) {
        let selected = state.current_folder == entry.path;
        let has_folders = entry.children.iter().any(|child| child.is_dir);
        if has_folders {
            let id = ui.make_persistent_id(("project_folder", &entry.path));
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
                .show_header(ui, |ui| {
                    if ui.selectable_label(selected, &entry.name).clicked() {
                        state.current_folder = entry.path.clone();
                    }
                })
                .body(|ui| folder_tree(ui, state, &entry.children));
        } else {
            ui.horizontal(|ui| {
                ui.add_space(ui.spacing().indent);
                if ui.selectable_label(selected, &entry.name).clicked() {
                    state.current_folder = entry.path.clone();
                }
            });
        }
    }
}

/// Содержимое папки по пути; пустой путь — корень.
pub fn find_folder<'a>(tree: &'a [AssetEntry], path: &str) -> Option<&'a [AssetEntry]> {
    if path.is_empty() {
        return Some(tree);
    }
    let mut entries = tree;
    for component in path.split('/') {
        entries = &entries
            .iter()
            .find(|entry| entry.is_dir && entry.name == component)?
            .children;
    }
    Some(entries)
}

fn kind_label(entry: &AssetEntry) -> &'static str {
    if entry.is_dir {
        return "DIR";
    }
    match AssetKind::of(&entry.path) {
        AssetKind::Scene => "SCENE",
        AssetKind::CameraSettings => "CAMERA",
        AssetKind::Model => "FBX",
        AssetKind::Texture => "PNG",
        AssetKind::Script => "LUA",
        AssetKind::Other => "FILE",
    }
}

fn tile(ui: &mut egui::Ui, state: &mut EditorState, entry: &AssetEntry) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(TILE_SIZE, TILE_SIZE),
        egui::Sense::click_and_drag(),
    );
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let painter = ui.painter();
        if response.hovered() {
            painter.rect_filled(rect, 4.0, visuals.bg_fill);
        }
        let image_rect = egui::Rect::from_min_size(
            rect.min + egui::vec2(16.0, 4.0),
            egui::vec2(TILE_SIZE - 32.0, TILE_SIZE - 32.0),
        );
        let preview = (!entry.is_dir && AssetKind::of(&entry.path) == AssetKind::Texture)
            .then(|| {
                let project = state.project.as_ref();
                state.previews.get(&entry.path, || {
                    project.and_then(|project| project.assets_dir.resolve(&entry.path).ok())
                })
            })
            .flatten();
        match preview {
            Some(texture) => {
                painter.image(
                    texture,
                    image_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                painter.rect_filled(image_rect, 3.0, ui.visuals().extreme_bg_color);
                painter.text(
                    image_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    kind_label(entry),
                    egui::FontId::proportional(12.0),
                    visuals.text_color(),
                );
            }
        }
        let name_rect = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 2.0, image_rect.max.y + 2.0),
            rect.max - egui::vec2(2.0, 0.0),
        );
        let galley = painter.layout(
            entry.name.clone(),
            egui::FontId::proportional(11.0),
            visuals.text_color(),
            name_rect.width(),
        );
        painter
            .with_clip_rect(name_rect)
            .galley(name_rect.min, galley, visuals.text_color());
    }
    let response = response.on_hover_text(&entry.path);
    if response.double_clicked() {
        if entry.is_dir {
            state.current_folder = entry.path.clone();
        } else if AssetKind::of(&entry.path) == AssetKind::Scene {
            state.action(Action::OpenScene(Some(entry.path.clone())));
        }
    }
    if !entry.is_dir {
        response.dnd_set_drag_payload(AssetDrag(entry.path.clone()));
    }
    response.context_menu(|ui| {
        if ui.button("Rename").clicked() {
            state.project_dialog = Some(ProjectDialog::Rename {
                path: entry.path.clone(),
                name: entry.name.clone(),
            });
        }
        if ui.button("Delete").clicked() {
            state.project_dialog = Some(ProjectDialog::Delete {
                path: entry.path.clone(),
                is_dir: entry.is_dir,
            });
        }
    });
}

/// Показывает модальный диалог файловой операции и выполняет её при подтверждении.
pub fn show_dialog(ctx: &egui::Context, state: &mut EditorState) {
    let Some(mut dialog) = state.project_dialog.clone() else {
        return;
    };
    let mut confirmed = false;
    let mut close = false;
    egui::Modal::new(egui::Id::new("project_dialog")).show(ctx, |ui| {
        ui.set_max_width(380.0);
        match &mut dialog {
            ProjectDialog::NewFolder { name } => {
                ui.heading("New Folder");
                ui.text_edit_singleline(name).request_focus();
            }
            ProjectDialog::Rename { name, .. } => {
                ui.heading("Rename");
                ui.text_edit_singleline(name).request_focus();
                ui.label("Ссылки в сценах на старый путь не обновляются.");
            }
            ProjectDialog::Delete { path, is_dir } => {
                ui.heading("Delete");
                let what = if *is_dir {
                    "папку с содержимым"
                } else {
                    "файл"
                };
                ui.label(format!("Удалить {what} {path}? Это действие необратимо."));
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let label = match dialog {
                ProjectDialog::Delete { .. } => "Delete",
                _ => "OK",
            };
            confirmed =
                ui.button(label).clicked() || ui.input(|input| input.key_pressed(egui::Key::Enter));
            close = ui.button("Cancel").clicked()
                || ui.input(|input| input.key_pressed(egui::Key::Escape));
        });
    });
    if close {
        state.project_dialog = None;
    } else if confirmed {
        state.project_dialog = None;
        run_dialog(state, &dialog);
    } else {
        state.project_dialog = Some(dialog);
    }
}

fn run_dialog(state: &mut EditorState, dialog: &ProjectDialog) {
    let Some(project) = state.project.as_ref() else {
        return;
    };
    let assets = &project.assets_dir;
    let result = match dialog {
        ProjectDialog::NewFolder { name } => assets
            .create_folder(&state.current_folder, name)
            .map(|path| format!("Создана папка {path}")),
        ProjectDialog::Rename { path, name } => assets
            .rename(path, name)
            .map(|target| format!("{path} переименован в {target}")),
        ProjectDialog::Delete { path, .. } => {
            assets.remove(path).map(|()| format!("Удалён {path}"))
        }
    };
    match result {
        Ok(message) => {
            state.log.info(message);
            state.action(Action::RefreshAssets);
        }
        Err(error) => state.log.error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, path: &str, children: Vec<AssetEntry>) -> AssetEntry {
        AssetEntry {
            name: name.to_owned(),
            path: path.to_owned(),
            is_dir: true,
            children,
        }
    }

    #[test]
    fn nested_folders_are_found_by_path() {
        let tree = vec![folder(
            "a",
            "a",
            vec![folder("b", "a/b", vec![folder("c", "a/b/c", Vec::new())])],
        )];
        assert_eq!(find_folder(&tree, "").unwrap().len(), 1);
        assert_eq!(find_folder(&tree, "a/b").unwrap()[0].name, "c");
        assert!(find_folder(&tree, "a/x").is_none());
    }
}
