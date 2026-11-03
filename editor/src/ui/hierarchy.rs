//! Панель Hierarchy: дерево объектов сцены.

use crate::scene_edit::{EditorScene, NewObject};
use crate::state::{Action, AssetDrag, EditorState};
use engine_scene::SceneObject;
use std::collections::HashMap;

/// Цвет экземпляров префабов и их объектов.
const PREFAB_COLOR: egui::Color32 = egui::Color32::from_rgb(110, 170, 255);

/// Подпись строки: имя или последний сегмент id, чтобы объекты экземпляра без имени
/// показывались как в префабе (`handle`, а не `door1.handle`).
fn row_name(object: &SceneObject) -> &str {
    match &object.name {
        Some(name) => name,
        None => object.id.rsplit('.').next().unwrap_or(&object.id),
    }
}

/// Перетаскиваемый объект иерархии.
#[derive(Clone, Debug)]
struct ObjectDrag(String);

/// Изменения, собранные при обходе дерева и применяемые после него.
enum Command {
    Select(String),
    SetParent(String, Option<String>),
    CreateFromAsset(String, Option<String>),
    CreateChild(String),
    Delete(String),
    Duplicate(String),
    BeginRename(String, String),
    CommitRename(String, String),
    CancelRename,
    CreatePrefab(String),
    Unpack(String),
    OpenPrefab(String),
}

/// Дети каждого объекта в порядке документа; ключ `None` — корневые объекты.
fn children_map(objects: &[SceneObject]) -> HashMap<Option<&str>, Vec<&SceneObject>> {
    let mut children: HashMap<Option<&str>, Vec<&SceneObject>> = HashMap::new();
    for object in objects {
        children
            .entry(object.parent.as_deref())
            .or_default()
            .push(object);
    }
    children
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some(scene) = state.scene.as_ref() else {
        ui.label("Сцена не открыта");
        return;
    };
    let editable = !state.is_playing();
    let mut commands = Vec::new();
    let rename = state.hierarchy_rename.clone();
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        ui.add_enabled_ui(editable, |ui| {
            let children = children_map(&scene.view().objects);
            let tree = TreeContext {
                scene,
                children: &children,
                rename: rename.as_ref(),
            };
            if let Some(roots) = children.get(&None) {
                for object in roots {
                    tree.row(ui, object, &mut commands);
                }
            }
            // Оставшаяся область принимает объекты, делая их корневыми, и ресурсы.
            let rest = ui.allocate_response(
                egui::vec2(ui.available_width(), ui.available_height().max(24.0)),
                egui::Sense::click(),
            );
            if let Some(dragged) = rest.dnd_release_payload::<ObjectDrag>() {
                commands.push(Command::SetParent(dragged.0.clone(), None));
            }
            if let Some(asset) = rest.dnd_release_payload::<AssetDrag>() {
                commands.push(Command::CreateFromAsset(asset.0.clone(), None));
            }
            if rest.clicked() {
                commands.push(Command::Select(String::new()));
            }
            rest.context_menu(|ui| {
                if ui.button("Create Empty").clicked() {
                    commands.push(Command::CreateChild(String::new()));
                }
            });
        });
    });
    for command in commands {
        apply(state, command);
    }
}

struct TreeContext<'a> {
    scene: &'a EditorScene,
    children: &'a HashMap<Option<&'a str>, Vec<&'a SceneObject>>,
    rename: Option<&'a (String, String)>,
}

impl TreeContext<'_> {
    fn row(&self, ui: &mut egui::Ui, object: &SceneObject, commands: &mut Vec<Command>) {
        let children = self.children.get(&Some(object.id.as_str()));
        match children {
            Some(children) if !children.is_empty() => {
                let id = ui.make_persistent_id(("hierarchy", &object.id));
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    id,
                    true,
                )
                .show_header(ui, |ui| self.label(ui, object, commands))
                .body(|ui| {
                    for child in children {
                        self.row(ui, child, commands);
                    }
                });
            }
            _ => {
                ui.horizontal(|ui| {
                    // Отступ совпадает с шириной стрелки сворачивания.
                    ui.add_space(ui.spacing().indent);
                    self.label(ui, object, commands);
                });
            }
        }
    }

    fn label(&self, ui: &mut egui::Ui, object: &SceneObject, commands: &mut Vec<Command>) {
        if let Some((renaming, text)) = self.rename
            && renaming == &object.id
        {
            let mut text = text.clone();
            let response = ui.text_edit_singleline(&mut text);
            response.request_focus();
            if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                commands.push(Command::CancelRename);
            } else if response.lost_focus() {
                commands.push(Command::CommitRename(object.id.clone(), text));
            } else if response.changed() {
                commands.push(Command::BeginRename(object.id.clone(), text));
            }
            return;
        }
        let selected = self.scene.selection() == Some(object.id.as_str());
        let instance = self.scene.instance(&object.id);
        let part = self.scene.is_prefab_part(&object.id);
        // Корень экземпляра помечен значком, объекты префаба — цветом, как в Unity.
        let mut text = egui::RichText::new(match instance {
            Some(_) => format!("📦 {}", row_name(object)),
            None => row_name(object).to_owned(),
        });
        if instance.is_some() || part {
            text = text.color(PREFAB_COLOR);
        }
        let response =
            ui.add(egui::Button::selectable(selected, text).sense(egui::Sense::click_and_drag()));
        let response = match instance {
            Some(instance) => response.on_hover_text(format!("/{}", instance.prefab.as_str())),
            None => response,
        };
        if response.clicked() {
            commands.push(Command::Select(object.id.clone()));
        }
        if response.double_clicked() {
            commands.push(Command::BeginRename(
                object.id.clone(),
                object.display_name().to_owned(),
            ));
        }
        // Объекты внутри экземпляра не перетаскиваются: менять им родителя нельзя.
        if !part {
            response.dnd_set_drag_payload(ObjectDrag(object.id.clone()));
        }
        if let Some(dragged) = response.dnd_release_payload::<ObjectDrag>()
            && dragged.0 != object.id
        {
            commands.push(Command::SetParent(
                dragged.0.clone(),
                Some(object.id.clone()),
            ));
        }
        if let Some(asset) = response.dnd_release_payload::<AssetDrag>() {
            commands.push(Command::CreateFromAsset(
                asset.0.clone(),
                Some(object.id.clone()),
            ));
        }
        response.context_menu(|ui| {
            if ui.button("Rename").clicked() {
                commands.push(Command::BeginRename(
                    object.id.clone(),
                    object.display_name().to_owned(),
                ));
            }
            if ui.button("Create Child").clicked() {
                commands.push(Command::CreateChild(object.id.clone()));
            }
            if !part {
                if ui.button("Duplicate").clicked() {
                    commands.push(Command::Duplicate(object.id.clone()));
                }
                if ui.button("Delete").clicked() {
                    commands.push(Command::Delete(object.id.clone()));
                }
            }
            ui.separator();
            if instance.is_none() && !part && ui.button("Create Prefab").clicked() {
                commands.push(Command::CreatePrefab(object.id.clone()));
            }
            if let Some(instance) = instance {
                if ui.button("Unpack").clicked() {
                    commands.push(Command::Unpack(object.id.clone()));
                }
                if ui.button("Open Prefab").clicked() {
                    commands.push(Command::OpenPrefab(instance.prefab.as_str().to_owned()));
                }
            }
        });
    }
}

fn apply(state: &mut EditorState, command: Command) {
    match command {
        Command::Select(id) => {
            if let Some(scene) = &mut state.scene {
                scene.select((!id.is_empty()).then_some(id.as_str()));
            }
        }
        Command::SetParent(id, parent) => {
            state.edit_scene(|scene| scene.set_parent(&id, parent.as_deref()));
        }
        Command::CreateFromAsset(path, parent) => {
            // Корневой объект появляется в центре вида, дочерний — в начале координат родителя.
            let position = match parent {
                Some(_) => [0.0; 3],
                None => state.camera.target.to_array(),
            };
            state.create_from_asset(&path, parent.as_deref(), position);
        }
        Command::CreateChild(parent) => {
            let parent = (!parent.is_empty()).then_some(parent.as_str());
            state.create_object(&NewObject::Empty, parent);
        }
        Command::Delete(id) => {
            state.edit_scene(|scene| scene.delete_object(&id));
        }
        Command::Duplicate(id) => {
            state.edit_scene(|scene| scene.duplicate_object(&id));
        }
        Command::BeginRename(id, text) => state.hierarchy_rename = Some((id, text)),
        Command::CommitRename(id, text) => {
            state.hierarchy_rename = None;
            state.edit_scene(|scene| {
                scene.edit_object(&id, None, |object| {
                    object.name = (!text.is_empty()).then_some(text);
                    Ok(())
                })
            });
        }
        Command::CancelRename => state.hierarchy_rename = None,
        Command::CreatePrefab(id) => state.action(Action::CreatePrefab(id)),
        Command::Unpack(id) => {
            state.edit_scene(|scene| scene.unpack(&id));
        }
        Command::OpenPrefab(path) => state.action(Action::OpenScene(Some(path))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_are_grouped_by_parent_in_document_order() {
        let mut scene = EditorScene::new_untitled().unwrap();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let first = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        let second = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        let map = children_map(&scene.view().objects);
        assert_eq!(map[&None].len(), 3);
        let children = map[&Some(parent.as_str())]
            .iter()
            .map(|object| object.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(children, [first, second]);
    }
}
