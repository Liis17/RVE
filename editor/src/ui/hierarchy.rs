//! Панель Hierarchy: дерево объектов сцены.

use crate::icons::{IconCache, object_icon};
use crate::scene_edit::{EditorScene, NewObject};
use crate::state::{Action, AssetDrag, EditorState};
use crate::ui::scene_view;
use engine_scene::SceneObject;
use std::collections::{HashMap, HashSet};

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
    /// Щелчок по строке с модификаторами; пустой id — щелчок мимо объектов.
    Select(String, egui::Modifiers),
    SetParent(String, Option<String>),
    CreateFromAsset(String, Option<String>),
    CreateChild(String),
    Delete(String),
    Duplicate(String),
    BeginRename(String, String),
    /// Изменение текста в открытом поле переименования.
    EditRename(String, String),
    /// Поле переименования получило фокус; повторно его запрашивать не нужно.
    RenameFocused,
    CommitRename(String, String),
    CancelRename,
    /// Двойной щелчок: выделить объект и навести на него камеру Scene.
    Frame(String),
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

/// Объекты, видимые при фильтре: совпадения по имени или id без учёта регистра и их предки.
fn filter_visible(objects: &[SceneObject], query: &str) -> HashSet<String> {
    let query = query.to_lowercase();
    let parents = objects
        .iter()
        .map(|object| (object.id.as_str(), object.parent.as_deref()))
        .collect::<HashMap<_, _>>();
    let mut visible = HashSet::new();
    for object in objects {
        let matches = object.display_name().to_lowercase().contains(&query)
            || object.id.to_lowercase().contains(&query);
        if !matches {
            continue;
        }
        let mut current = Some(object.id.as_str());
        // Предки добавляются до первого уже видимого; цикл родителей схема не допускает.
        while let Some(id) = current {
            if !visible.insert(id.to_owned()) {
                break;
            }
            current = parents.get(id).copied().flatten();
        }
    }
    visible
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    if state.scene.is_none() {
        ui.label("Сцена не открыта");
        return;
    }
    ui.add(
        egui::TextEdit::singleline(&mut state.hierarchy_filter)
            .hint_text("Search")
            .desired_width(f32::INFINITY),
    );
    let Some(scene) = state.scene.as_ref() else {
        return;
    };
    let editable = !state.is_playing();
    let mut commands = Vec::new();
    let rename = state.hierarchy_rename.clone();
    let rename_focus = state.hierarchy_rename_focus;
    let query = state.hierarchy_filter.trim();
    let visible = (!query.is_empty()).then(|| filter_visible(&scene.view().objects, query));
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        ui.add_enabled_ui(editable, |ui| {
            let children = children_map(&scene.view().objects);
            let tree = TreeContext {
                scene,
                icons: &state.icons,
                children: &children,
                rename: rename.as_ref(),
                rename_focus,
                visible: visible.as_ref(),
            };
            if let Some(roots) = children.get(&None) {
                for object in roots.iter().filter(|object| tree.is_visible(object)) {
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
                commands.push(Command::Select(
                    String::new(),
                    ui.input(|input| input.modifiers),
                ));
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
    icons: &'a IconCache,
    children: &'a HashMap<Option<&'a str>, Vec<&'a SceneObject>>,
    rename: Option<&'a (String, String)>,
    /// Полю переименования нужно передать фокус в этом кадре.
    rename_focus: bool,
    /// Видимые при фильтре объекты; `None` — фильтра нет.
    visible: Option<&'a HashSet<String>>,
}

impl TreeContext<'_> {
    fn is_visible(&self, object: &SceneObject) -> bool {
        self.visible
            .is_none_or(|visible| visible.contains(&object.id))
    }

    fn row(&self, ui: &mut egui::Ui, object: &SceneObject, commands: &mut Vec<Command>) {
        let children = self
            .children
            .get(&Some(object.id.as_str()))
            .map(|children| {
                children
                    .iter()
                    .filter(|child| self.is_visible(child))
                    .collect::<Vec<_>>()
            });
        match children {
            Some(children) if !children.is_empty() => {
                // При фильтре узлы раскрыты, а свёрнутость обычного дерева не меняется.
                let salt = if self.visible.is_some() {
                    "hierarchy_filtered"
                } else {
                    "hierarchy"
                };
                let id = ui.make_persistent_id((salt, &object.id));
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
            // Явный id: поле должно оставаться тем же виджетом между кадрами.
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .id(egui::Id::new(("hierarchy_rename", &object.id))),
            );
            if self.rename_focus {
                response.request_focus();
                commands.push(Command::RenameFocused);
            }
            if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                commands.push(Command::CancelRename);
            } else if response.lost_focus() {
                commands.push(Command::CommitRename(object.id.clone(), text));
            } else if response.changed() {
                commands.push(Command::EditRename(object.id.clone(), text));
            }
            return;
        }
        let selected = self.scene.is_selected(&object.id);
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
        // Иконка по компонентам объекта; без текстуры строка остаётся одной подписью.
        if let Some(texture) = object_icon(object).and_then(|name| self.icons.get(name)) {
            let size = ui.spacing().interact_size.y;
            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                texture,
                egui::vec2(size, size),
            )));
        }
        let response =
            ui.add(egui::Button::selectable(selected, text).sense(egui::Sense::click_and_drag()));
        let response = match instance {
            Some(instance) => response.on_hover_text(format!("/{}", instance.prefab.as_str())),
            None => response,
        };
        if response.clicked() {
            let modifiers = ui.input(|input| input.modifiers);
            commands.push(Command::Select(object.id.clone(), modifiers));
        }
        if response.double_clicked() {
            commands.push(Command::Frame(object.id.clone()));
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
        Command::Select(id, modifiers) => {
            if let Some(scene) = &mut state.scene {
                let id = (!id.is_empty()).then_some(id.as_str());
                scene_view::apply_click_selection(scene, id, modifiers);
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
        Command::BeginRename(id, text) => state.begin_hierarchy_rename(id, text),
        Command::EditRename(id, text) => state.hierarchy_rename = Some((id, text)),
        Command::RenameFocused => state.hierarchy_rename_focus = false,
        Command::Frame(id) => {
            if let Some(scene) = &mut state.scene {
                scene.select(Some(id.as_str()));
            }
            scene_view::focus_selection(state);
        }
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

    #[test]
    fn filter_keeps_matches_and_their_ancestors_ignoring_case() {
        let mut scene = EditorScene::new_untitled().unwrap();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        scene
            .edit_object(&child, None, |object| {
                object.name = Some("Wheel Front".to_owned());
                Ok(())
            })
            .unwrap();
        let objects = &scene.document().objects;
        let visible = filter_visible(objects, "wheel");
        assert_eq!(visible, HashSet::from([parent.clone(), child.clone()]));
        // Совпадение по id тоже учитывается; соседние ветки не показываются.
        assert_eq!(
            filter_visible(objects, "SUN"),
            HashSet::from(["sun".to_owned()])
        );
        assert!(filter_visible(objects, "missing").is_empty());
    }
}
