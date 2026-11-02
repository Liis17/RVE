//! Состояние редактора, общее для панелей и обработчиков действий; не зависит от GPU.

use crate::assets_fs::{AssetEntry, AssetKind};
use crate::console::ConsoleLog;
use crate::play::PlaySession;
use crate::previews::PreviewCache;
use crate::project::OpenProject;
use crate::scene_edit::{EditorScene, NewObject};
use crate::viewport::camera::EditorCamera;
use crate::viewport::gizmo::{GizmoDrag, GizmoSpace, GizmoTool};
use engine_assets::Model;
use engine_files::VirtualPath;
use engine_scene::World;
use engine_script::ScriptLibrary;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Действие, которое выполняется после построения UI: диалоги ОС блокируют кадр,
/// а смена проекта или сцены затрагивает данные, заимствованные панелями.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    NewScene,
    /// `None` — выбрать файл в диалоге.
    OpenScene(Option<String>),
    OpenProject,
    Save,
    SaveAs,
    Exit,
    Play,
    Stop,
    ImportDialog,
    ImportFiles(Vec<PathBuf>),
    /// Файловая структура изменилась: сбросить кэши ресурсов и прочитать дерево заново.
    RefreshAssets,
    /// Сохранить сцену и при успехе выполнить действие.
    SaveThen(Box<Action>),
    /// Выполнить действие, отбросив несохранённые изменения.
    Discard(Box<Action>),
}

/// Подтверждение действия при несохранённых изменениях сцены.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsavedPrompt {
    pub then: Action,
}

/// Перетаскиваемый ресурс из панели Project.
#[derive(Clone, Debug)]
pub struct AssetDrag(pub String);

/// Сведения о панели Scene за текущий кадр.
#[derive(Clone, Copy, Debug, Default)]
pub struct SceneViewInfo {
    /// Панель показана и её изображение нужно нарисовать в этом кадре.
    pub visible: bool,
    /// Курсор над панелью: ввод Play-режима относится к сцене.
    pub hovered: bool,
    /// Размер изображения в физических пикселях.
    pub size: [u32; 2],
}

/// Раздельные заимствования состояния для подготовки кадра сцены.
pub struct FrameParts<'a> {
    pub world: &'a mut World,
    pub project: &'a OpenProject,
    pub models: &'a mut HashMap<String, Arc<Model>>,
    pub log: &'a mut ConsoleLog,
}

pub struct EditorState {
    pub project: Option<OpenProject>,
    pub scene: Option<EditorScene>,
    pub log: ConsoleLog,
    pub tool: GizmoTool,
    pub space: GizmoSpace,
    pub camera: EditorCamera,
    pub gizmo_drag: Option<GizmoDrag>,
    pub play: Option<PlaySession>,
    pub scripts_path: Option<PathBuf>,
    pub library: Option<ScriptLibrary>,
    pub actions: Vec<Action>,
    pub unsaved_prompt: Option<UnsavedPrompt>,
    pub asset_tree: Vec<AssetEntry>,
    pub current_folder: String,
    pub project_dialog: Option<crate::ui::project_panel::ProjectDialog>,
    pub hierarchy_rename: Option<(String, String)>,
    pub previews: PreviewCache,
    pub viewport_texture: Option<egui::TextureId>,
    pub scene_view: SceneViewInfo,
    /// CPU-модели, загруженные для отрисовки: по ним работают picking и фокус камеры.
    pub models: HashMap<String, Arc<Model>>,
    edit_world: Option<(u64, World)>,
}

impl EditorState {
    pub fn new(scripts_path: Option<PathBuf>) -> Self {
        Self {
            project: None,
            scene: None,
            log: ConsoleLog::default(),
            tool: GizmoTool::Move,
            space: GizmoSpace::Global,
            camera: EditorCamera::default(),
            gizmo_drag: None,
            play: None,
            scripts_path,
            library: None,
            actions: Vec::new(),
            unsaved_prompt: None,
            asset_tree: Vec::new(),
            current_folder: String::new(),
            project_dialog: None,
            hierarchy_rename: None,
            previews: PreviewCache::default(),
            viewport_texture: None,
            scene_view: SceneViewInfo::default(),
            models: HashMap::new(),
            edit_world: None,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.play.is_some()
    }

    pub fn action(&mut self, action: Action) {
        self.actions.push(action);
    }

    /// Мир текущего кадра: копия Play-режима либо мир, собранный из документа.
    pub fn world_mut(&mut self) -> Option<&mut World> {
        if let Some(play) = &mut self.play {
            return Some(play.world_mut());
        }
        let scene = self.scene.as_ref()?;
        let revision = scene.revision();
        let stale = self
            .edit_world
            .as_ref()
            .is_none_or(|(built, _)| *built != revision);
        if stale {
            match World::from_document(scene.document()) {
                Ok(world) => self.edit_world = Some((revision, world)),
                Err(error) => {
                    self.log.error(error.to_string());
                    return None;
                }
            }
        }
        self.edit_world.as_mut().map(|(_, world)| world)
    }

    /// Заимствует всё, что нужно для подготовки кадра сцены.
    pub fn frame_parts(&mut self) -> Option<FrameParts<'_>> {
        self.world_mut()?;
        let world = match &mut self.play {
            Some(play) => play.world_mut(),
            None => &mut self.edit_world.as_mut()?.1,
        };
        Some(FrameParts {
            world,
            project: self.project.as_ref()?,
            models: &mut self.models,
            log: &mut self.log,
        })
    }

    /// Сбрасывает данные, связанные с прежней сценой.
    pub fn scene_replaced(&mut self) {
        self.edit_world = None;
        self.gizmo_drag = None;
        self.hierarchy_rename = None;
    }

    /// Выполняет правку сцены и пишет ошибку в консоль; в Play-режиме документ не меняется.
    pub fn edit_scene<T>(
        &mut self,
        change: impl FnOnce(&mut EditorScene) -> Result<T, String>,
    ) -> Option<T> {
        if self.is_playing() {
            return None;
        }
        let scene = self.scene.as_mut()?;
        match change(scene) {
            Ok(value) => Some(value),
            Err(error) => {
                self.log.error(error);
                None
            }
        }
    }

    pub fn create_object(&mut self, kind: &NewObject, parent: Option<&str>) {
        self.edit_scene(|scene| scene.create_object(kind, parent));
    }

    /// Создаёт объект из перетащенного ресурса, если это модель.
    pub fn create_from_asset(&mut self, path: &str, parent: Option<&str>) {
        if AssetKind::of(path) != AssetKind::Model {
            self.log
                .error(format!("{path}: на сцену можно перетащить только .fbx"));
            return;
        }
        match VirtualPath::parse(path) {
            Ok(path) => self.create_object(&NewObject::Mesh(path), parent),
            Err(_) => self.log.error(format!("{path}: некорректный путь ресурса")),
        }
    }

    /// Пути ресурсов заданного вида для выпадающих списков Inspector.
    pub fn assets_of_kind(&self, kind: AssetKind) -> Vec<String> {
        fn collect(entries: &[AssetEntry], kind: AssetKind, paths: &mut Vec<String>) {
            for entry in entries {
                if entry.is_dir {
                    collect(&entry.children, kind, paths);
                } else if AssetKind::of(&entry.path) == kind {
                    paths.push(entry.path.clone());
                }
            }
        }
        let mut paths = Vec::new();
        collect(&self.asset_tree, kind, &mut paths);
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_edit::EditorScene;

    fn state_with_scene() -> EditorState {
        let mut state = EditorState::new(None);
        state.scene = Some(EditorScene::new_untitled().unwrap());
        state
    }

    #[test]
    fn world_is_rebuilt_only_after_the_document_changes() {
        let mut state = state_with_scene();
        assert_eq!(state.world_mut().unwrap().objects().len(), 2);
        state.create_object(&NewObject::Empty, None);
        assert_eq!(state.world_mut().unwrap().objects().len(), 3);
    }

    #[test]
    fn failed_edit_is_logged_and_leaves_the_scene_unchanged() {
        let mut state = state_with_scene();
        state.create_object(&NewObject::Camera, None);
        assert_eq!(state.log.entries().count(), 1);
        assert_eq!(state.scene.as_ref().unwrap().document().objects.len(), 2);
    }

    #[test]
    fn dropping_a_non_model_asset_is_rejected() {
        let mut state = state_with_scene();
        state.create_from_asset("textures/a.png", None);
        assert_eq!(state.log.entries().count(), 1);
        state.create_from_asset("models/a.fbx", None);
        assert_eq!(state.scene.as_ref().unwrap().document().objects.len(), 3);
    }

    #[test]
    fn assets_are_filtered_by_kind_through_nested_folders() {
        let mut state = EditorState::new(None);
        let file = |path: &str| AssetEntry {
            name: path.rsplit('/').next().unwrap().to_owned(),
            path: path.to_owned(),
            is_dir: false,
            children: Vec::new(),
        };
        state.asset_tree = vec![
            AssetEntry {
                name: "m".to_owned(),
                path: "m".to_owned(),
                is_dir: true,
                children: vec![file("m/a.fbx"), file("m/a.png")],
            },
            file("b.fbx"),
        ];
        assert_eq!(state.assets_of_kind(AssetKind::Model), ["m/a.fbx", "b.fbx"]);
        assert_eq!(state.assets_of_kind(AssetKind::Script), [] as [&str; 0]);
    }
}
