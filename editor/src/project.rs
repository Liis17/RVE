//! Открытый проект: настройки, ресурсы в режиме loose и окно по `bootstrap.rves`.

use crate::assets_fs::AssetsDir;
use crate::scene_edit::PrefabLoader;
use engine_assets::Assets;
use engine_files::VirtualPath;
use engine_project::{BootstrapConfig, ProjectSettings};
use engine_resources::{ResourceConfig, ResourceHandle, ResourceStats, ResourceSystem, SourceMode};
use engine_scene::world::ObjectHandle;
use engine_scene::{CameraSettings, SceneDocument, SceneError, World};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Проект игры, открытый в редакторе по той же раскладке, что и dev-запуск player.
pub struct OpenProject {
    /// Абсолютный путь каталога проекта.
    pub root: PathBuf,
    pub settings: ProjectSettings,
    pub assets_dir: AssetsDir,
    pub assets: Assets,
    prefabs: Rc<PrefabCache>,
    camera_settings: RefCell<HashMap<String, CameraSettings>>,
    resources: ResourceSystem,
}

/// Разобранные префабы проекта по пути; изменённый документ заменяется после успешного разбора.
struct PrefabCache {
    handle: ResourceHandle,
    documents: RefCell<HashMap<VirtualPath, SceneDocument>>,
}

impl PrefabCache {
    fn load(&self, path: &VirtualPath) -> Result<SceneDocument, SceneError> {
        if let Some(document) = self.documents.borrow().get(path) {
            return Ok(document.clone());
        }
        let document = self.read(path)?;
        self.documents
            .borrow_mut()
            .insert(path.clone(), document.clone());
        Ok(document)
    }

    fn read(&self, path: &VirtualPath) -> Result<SceneDocument, SceneError> {
        let label = format!("/{}", path.as_str());
        let error = |message: String| SceneError {
            source: label.clone(),
            object_id: None,
            message,
        };
        let bytes = self
            .handle
            .read_all(label.as_str())
            .map_err(|resource| error(resource.to_string()))?
            .wait()
            .map_err(|resource| error(resource.to_string()))?;
        let text = String::from_utf8(bytes)
            .map_err(|utf8| error(format!("файл должен быть UTF-8: {utf8}")))?;
        SceneDocument::parse_prefab(label, &text)
    }

    fn reload(
        &self,
        paths: &HashSet<VirtualPath>,
    ) -> (HashMap<VirtualPath, SceneDocument>, Vec<String>) {
        let mut documents = self.documents.borrow_mut();
        let mut previous = HashMap::new();
        let mut errors = Vec::new();
        for path in paths {
            let Some(cached) = documents.get(path).cloned() else {
                continue;
            };
            match self.read(path) {
                Ok(document) => {
                    previous.insert(path.clone(), cached);
                    documents.insert(path.clone(), document);
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        (previous, errors)
    }

    fn restore(&self, previous: HashMap<VirtualPath, SceneDocument>) {
        self.documents.borrow_mut().extend(previous);
    }
}

impl OpenProject {
    pub fn open(dir: &Path) -> Result<Self, String> {
        let dir = std::path::absolute(dir)
            .map(|dir| without_dot_components(&dir))
            .map_err(|error| format!("{}: {error}", dir.display()))?;
        let settings = ProjectSettings::load(dir.join("project.rveproject"))?;
        let root = dir.join("Assets");
        if !root.is_dir() {
            return Err(format!("{}: каталог ресурсов не найден", root.display()));
        }
        let resources = ResourceSystem::start(ResourceConfig {
            root: root.clone(),
            source_mode: SourceMode::Loose,
            ..ResourceConfig::default()
        })
        .map_err(|error| format!("{}: {error}", root.display()))?;
        let assets = Assets::with_resources(resources.handle());
        let prefabs = Rc::new(PrefabCache {
            handle: resources.handle(),
            documents: RefCell::default(),
        });
        Ok(Self {
            root: dir,
            settings,
            assets_dir: AssetsDir::new(root),
            assets,
            prefabs,
            camera_settings: RefCell::default(),
            resources,
        })
    }

    /// Снимок статистики запросов ресурсов проекта для вкладки Profiler.
    pub fn resource_stats(&self) -> ResourceStats {
        self.resources.handle().stats()
    }

    /// Ресурсы проекта для звуковой подсистемы Play.
    pub fn resource_handle(&self) -> ResourceHandle {
        self.resources.handle()
    }

    /// Читает текстовый ресурс, например исходник Lua или файл сцены.
    pub fn read_text(&self, path: &str) -> Result<String, String> {
        let bytes = self
            .resources
            .handle()
            .read_all(path)
            .map_err(|error| format!("{path}: {error}"))?
            .wait()
            .map_err(|error| format!("{path}: {error}"))?;
        String::from_utf8(bytes).map_err(|error| format!("{path}: файл должен быть UTF-8: {error}"))
    }

    /// Writes an edited `.camerasettings` asset without creating an unassigned file.
    pub fn save_camera_settings(
        &self,
        path: &VirtualPath,
        settings: &CameraSettings,
    ) -> Result<(), String> {
        if !path
            .as_str()
            .to_ascii_lowercase()
            .ends_with(".camerasettings")
        {
            return Err(format!("{path}: ожидается ресурс .camerasettings"));
        }
        settings.validate().map_err(|error| error.to_string())?;
        let file = self
            .assets_dir
            .resolve(path.as_str())
            .map_err(|error| format!("{path}: {error}"))?;
        if !file.is_file() {
            return Err(format!("{path}: файл настроек камеры не найден"));
        }
        let source = settings.to_toml().map_err(|error| error.to_string())?;
        std::fs::write(file, source).map_err(|error| format!("{path}: {error}"))?;
        self.camera_settings
            .borrow_mut()
            .insert(path.as_str().to_owned(), *settings);
        Ok(())
    }

    /// Загрузчик префабов проекта для [`crate::scene_edit::EditorScene`].
    pub fn prefab_loader(&self) -> PrefabLoader {
        let cache = Rc::clone(&self.prefabs);
        Rc::new(move |path| cache.load(path))
    }

    /// Забывает разобранные префабы: следующее раскрытие прочитает файлы заново.
    pub fn clear_prefabs(&self) {
        self.prefabs.documents.borrow_mut().clear();
    }

    /// Перечитывает изменённые префабы, уже находящиеся в кэше.
    pub fn reload_prefabs(
        &self,
        paths: &HashSet<VirtualPath>,
    ) -> (HashMap<VirtualPath, SceneDocument>, Vec<String>) {
        self.prefabs.reload(paths)
    }

    /// Возвращает пути документов, уже загруженных в кэш префабов.
    pub fn cached_prefab_paths(&self) -> HashSet<VirtualPath> {
        self.prefabs.documents.borrow().keys().cloned().collect()
    }

    /// Возвращает прежние документы, если раскрытие сцены после обновления не удалось.
    pub fn restore_prefabs(&self, previous: HashMap<VirtualPath, SceneDocument>) {
        self.prefabs.restore(previous);
    }

    /// Загружает общие `.camerasettings` файлы и назначает их камерам мира.
    pub fn apply_camera_settings(&self, world: &mut World) -> Result<(), String> {
        let cameras = camera_paths(world);
        let mut settings_by_path = BTreeMap::new();
        for (_, path) in &cameras {
            if !settings_by_path.contains_key(path) {
                let source = self.read_text(path)?;
                let settings =
                    CameraSettings::parse(path, &source).map_err(|error| error.to_string())?;
                settings_by_path.insert(path.clone(), settings);
            }
        }
        for (handle, path) in cameras {
            world
                .set_camera_settings(handle, settings_by_path[&path])
                .map_err(|error| error.to_string())?;
        }
        self.camera_settings.borrow_mut().extend(settings_by_path);
        Ok(())
    }

    /// Применяет последние валидные настройки камер для мира редактора.
    pub fn apply_camera_settings_preview(&self, world: &mut World) -> Vec<String> {
        let cameras = camera_paths(world);
        let mut settings_by_path = BTreeMap::new();
        let mut errors = Vec::new();
        for (_, path) in &cameras {
            if settings_by_path.contains_key(path) {
                continue;
            }
            let settings = self.read_text(path).and_then(|source| {
                CameraSettings::parse(path, &source).map_err(|error| error.to_string())
            });
            match settings {
                Ok(settings) => {
                    settings_by_path.insert(path.clone(), settings);
                    self.camera_settings
                        .borrow_mut()
                        .insert(path.clone(), settings);
                }
                Err(error) => {
                    errors.push(error);
                    if let Some(settings) = self.camera_settings.borrow().get(path).copied() {
                        settings_by_path.insert(path.clone(), settings);
                    }
                }
            }
        }
        for (handle, path) in cameras {
            if let Some(settings) = settings_by_path.get(&path) {
                let _ = world.set_camera_settings(handle, *settings);
            }
        }
        errors
    }

    /// Настройки окна проекта; отсутствующий файл не является ошибкой.
    pub fn bootstrap(dir: &Path) -> Result<BootstrapConfig, String> {
        let path = dir.join("bootstrap.rves");
        if path.is_file() {
            BootstrapConfig::load(path)
        } else {
            Ok(BootstrapConfig::default())
        }
    }
}

fn camera_paths(world: &World) -> Vec<(ObjectHandle, String)> {
    world
        .objects()
        .iter()
        .filter_map(|object| {
            Some((
                object.handle,
                object
                    .camera
                    .as_ref()?
                    .settings
                    .as_ref()?
                    .as_str()
                    .to_owned(),
            ))
        })
        .collect()
}

/// Убирает `.` и `..` из абсолютного пути без обращения к диску: один проект,
/// открытый по разным относительным путям, получает один путь в недавних проектах.
fn without_dot_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

impl Drop for OpenProject {
    fn drop(&mut self) {
        if let Err(error) = self.resources.shutdown() {
            eprintln!("не удалось остановить систему ресурсов: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_edit::EditorScene;
    use engine_scene::{SceneDocument, World};
    use glam::Vec3;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempProject(PathBuf);

    impl TempProject {
        fn new(settings: Option<&str>, assets: bool) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rve-editor-project-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            if let Some(settings) = settings {
                fs::write(path.join("project.rveproject"), settings).unwrap();
            }
            if assets {
                fs::create_dir_all(path.join("Assets")).unwrap();
            }
            Self(path)
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const SETTINGS: &str = "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n\n[[scenes]]\npath = \"/main.rvescene\"\n";

    #[test]
    fn open_reads_settings_and_sees_files_added_later() {
        let temp = TempProject::new(Some(SETTINGS), true);
        let project = OpenProject::open(&temp.0).unwrap();
        assert_eq!(project.settings.name, "Game");
        fs::write(temp.0.join("Assets/new.lua"), "return {}").unwrap();
        assert_eq!(project.read_text("new.lua").unwrap(), "return {}");
        assert!(project.read_text("missing.lua").is_err());
    }

    #[test]
    fn project_root_has_no_dot_components() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/./demo-game");
        let project = OpenProject::open(Path::new(dir)).unwrap();
        let expected = std::path::absolute(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../examples/demo-game"
        ))
        .unwrap();
        assert!(project.root.is_absolute());
        assert!(project.root.components().all(|component| !matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )));
        assert_eq!(project.root, without_dot_components(&expected));
        assert!(project.root.ends_with("examples/demo-game"));
    }

    #[test]
    fn open_requires_settings_and_assets_directory() {
        let without_settings = TempProject::new(None, true);
        assert!(OpenProject::open(&without_settings.0).is_err());
        let without_assets = TempProject::new(Some(SETTINGS), false);
        assert!(
            OpenProject::open(&without_assets.0)
                .err()
                .unwrap()
                .contains("Assets")
        );
    }

    #[test]
    fn missing_bootstrap_uses_defaults_and_invalid_one_is_an_error() {
        let temp = TempProject::new(Some(SETTINGS), true);
        assert_eq!(
            OpenProject::bootstrap(&temp.0).unwrap(),
            BootstrapConfig::default()
        );
        fs::write(temp.0.join("bootstrap.rves"), "WindowSizeX=abc\n").unwrap();
        assert!(OpenProject::bootstrap(&temp.0).is_err());
    }

    #[test]
    fn demo_models_are_large_enough_to_see_from_the_scene_cameras() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let project = OpenProject::open(Path::new(dir)).unwrap();
        for model in ["draft-cube", "props-cube", "shared-cube"] {
            let path = format!("models/{model}.fbx");
            let (min, max) = project.assets.load_model(&path).unwrap().bounds().unwrap();
            assert_eq!(max[0] - min[0], 2.0, "{path}");
        }
    }

    #[test]
    fn demo_model_normals_point_out_of_the_cube() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let project = OpenProject::open(Path::new(dir)).unwrap();
        for model in ["draft-cube", "props-cube", "shared-cube"] {
            let path = format!("models/{model}.fbx");
            let model = project.assets.load_model(&path).unwrap();
            for vertex in model.meshes.iter().flat_map(|mesh| &mesh.vertices) {
                let outward: f32 = vertex
                    .position
                    .iter()
                    .zip(vertex.normal)
                    .map(|(p, n)| p * n)
                    .sum();
                assert!(outward > 0.0, "{path}: {vertex:?}");
            }
        }
    }

    #[test]
    fn demo_scenes_stand_on_the_floor_under_a_sun_shining_from_above() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let project = OpenProject::open(Path::new(dir)).unwrap();
        for scene in ["main", "second"] {
            let path = format!("{dir}/Assets/scenes/{scene}.rvescene");
            let text = std::fs::read_to_string(&path).unwrap();
            let document = SceneDocument::parse(path.as_str(), &text).unwrap();
            let loader = project.prefab_loader();
            let expanded = engine_scene::expand(&document, &mut |path| loader(path))
                .unwrap()
                .document;
            let mut world = World::from_document(&expanded).unwrap();
            let (frame, _) = world.build_frame();
            assert!(frame.light.direction[1] < 0.0, "{path}");
            assert!(!frame.meshes.is_empty(), "{path}");
            for instance in &frame.meshes {
                let object = world.object(instance.object).unwrap();
                let model = object.mesh_renderer.as_ref().unwrap().model.as_str();
                let model = project
                    .assets
                    .load_model(model.trim_start_matches('/'))
                    .unwrap();
                let (min, _) = model.bounds().unwrap();
                let lowest = instance.model.transform_point3(Vec3::from_array(min)).y;
                assert!(lowest >= -1e-5, "{path}: {lowest}");
            }
        }
    }

    #[test]
    fn demo_project_opens_and_lists_its_start_scene() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let project = OpenProject::open(Path::new(dir)).unwrap();
        let start = project.settings.starting_scene().unwrap();
        assert!(project.read_text(start).unwrap().contains("[[objects]]"));
    }

    #[test]
    fn all_demo_scenes_load_their_camera_settings_assets() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let project = OpenProject::open(Path::new(dir)).unwrap();
        for (scene, expected_fov) in [("main", 55.0), ("second", 55.0), ("draft", 60.0)] {
            let path = format!("scenes/{scene}.rvescene");
            let source = project.read_text(&path).unwrap();
            let document = SceneDocument::parse(&path, &source).unwrap();
            let loader = project.prefab_loader();
            let expanded = engine_scene::expand(&document, &mut |prefab| loader(prefab))
                .unwrap()
                .document;
            let mut world = World::from_document(&expanded).unwrap();
            project.apply_camera_settings(&mut world).unwrap();
            let camera = world.camera_settings(world.active_camera()).unwrap();
            assert_eq!(camera.projection.fov_deg, expected_fov, "{path}");
            assert_eq!(camera.projection.near, 0.1, "{path}");
            assert_eq!(camera.projection.far, 100.0, "{path}");
        }
    }

    #[test]
    fn camera_settings_asset_is_loaded_into_the_editor_world() {
        let temp = TempProject::new(Some(SETTINGS), true);
        fs::create_dir_all(temp.0.join("Assets/cameras")).unwrap();
        fs::write(
            temp.0.join("Assets/cameras/main.camerasettings"),
            "[projection]\nfov_deg = 42.0\nnear = 0.25\nfar = 250.0\n",
        )
        .unwrap();
        let source = "[[assets]]\npath = \"/cameras/main.camerasettings\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nsettings = \"/cameras/main.camerasettings\"\n";
        let document = SceneDocument::parse("scene.rvescene", source).unwrap();
        let mut world = World::from_document(&document).unwrap();
        let project = OpenProject::open(&temp.0).unwrap();
        project.apply_camera_settings(&mut world).unwrap();
        let camera = world.handle("camera").unwrap();
        assert_eq!(
            world.camera_settings(camera).unwrap().projection.fov_deg,
            42.0
        );
        assert_eq!(world.camera_settings(camera).unwrap().projection.near, 0.25);
    }

    #[test]
    fn prefab_reload_replaces_valid_documents_and_keeps_the_previous_on_error() {
        let temp = TempProject::new(Some(SETTINGS), true);
        fs::create_dir_all(temp.0.join("Assets/prefabs")).unwrap();
        let file = temp.0.join("Assets/prefabs/item.rveprefab");
        fs::write(&file, "[[objects]]\nid = \"old\"\n").unwrap();
        let project = OpenProject::open(&temp.0).unwrap();
        let loader = project.prefab_loader();
        let path = VirtualPath::parse("prefabs/item.rveprefab").unwrap();
        assert_eq!(loader(&path).unwrap().objects[0].id, "old");

        fs::write(&file, "[[objects]]\nid = \"new\"\n").unwrap();
        let paths = HashSet::from([path.clone()]);
        let (previous, errors) = project.reload_prefabs(&paths);
        assert!(errors.is_empty());
        assert_eq!(previous[&path].objects[0].id, "old");
        assert_eq!(loader(&path).unwrap().objects[0].id, "new");

        fs::write(&file, "not a prefab").unwrap();
        let (previous, errors) = project.reload_prefabs(&paths);
        assert!(previous.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(loader(&path).unwrap().objects[0].id, "new");
    }

    #[test]
    fn prefab_cache_rolls_back_when_scene_expansion_fails() {
        let temp = TempProject::new(Some(SETTINGS), true);
        fs::create_dir_all(temp.0.join("Assets/prefabs")).unwrap();
        let file = temp.0.join("Assets/prefabs/item.rveprefab");
        fs::write(&file, "[[objects]]\nid = \"old\"\n").unwrap();
        let project = OpenProject::open(&temp.0).unwrap();
        let path = VirtualPath::parse("prefabs/item.rveprefab").unwrap();
        let source = "[[assets]]\npath = \"/prefabs/item.rveprefab\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n\n[[objects]]\nid = \"root\"\n[[instances]]\nid = \"item\"\nparent = \"root\"\nprefab = \"/prefabs/item.rveprefab\"\n";
        let mut scene = EditorScene::parse(
            VirtualPath::parse("main.rvescene").unwrap(),
            source,
            project.prefab_loader(),
        )
        .unwrap();

        fs::write(
            &file,
            "[[assets]]\npath = \"/prefabs/missing.rveprefab\"\n[[objects]]\nid = \"new\"\n[[instances]]\nid = \"missing\"\nparent = \"new\"\nprefab = \"/prefabs/missing.rveprefab\"\n",
        )
        .unwrap();
        let paths = HashSet::from([path.clone()]);
        let (previous, errors) = project.reload_prefabs(&paths);
        assert!(errors.is_empty());
        assert!(scene.reload_prefabs().is_err());
        project.restore_prefabs(previous);
        let restored = project.prefab_loader()(&path).unwrap();
        assert_eq!(restored.objects[0].id, "old");
        assert_eq!(scene.view().objects.len(), 3);
    }

    #[test]
    fn camera_settings_asset_edits_are_validated_and_saved() {
        let temp = TempProject::new(Some(SETTINGS), true);
        fs::create_dir_all(temp.0.join("Assets/cameras")).unwrap();
        fs::write(
            temp.0.join("Assets/cameras/main.camerasettings"),
            "[projection]\nfov_deg = 42.0\n",
        )
        .unwrap();
        let project = OpenProject::open(&temp.0).unwrap();
        let path = VirtualPath::parse("cameras/main.camerasettings").unwrap();
        let mut settings = CameraSettings::default();
        settings.render.anti_aliasing = engine_scene::AntiAliasing::Taa;
        project.save_camera_settings(&path, &settings).unwrap();
        let saved = CameraSettings::parse(
            path.to_string(),
            &fs::read_to_string(temp.0.join("Assets/cameras/main.camerasettings")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.render.anti_aliasing, engine_scene::AntiAliasing::Taa);

        let missing = VirtualPath::parse("cameras/missing.camerasettings").unwrap();
        assert!(project.save_camera_settings(&missing, &settings).is_err());
    }
}
