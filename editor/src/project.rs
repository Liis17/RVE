//! Открытый проект: настройки, ресурсы в режиме loose и окно по `bootstrap.rves`.

use crate::assets_fs::AssetsDir;
use engine_assets::Assets;
use engine_project::{BootstrapConfig, ProjectSettings};
use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
use std::path::{Path, PathBuf};

/// Проект игры, открытый в редакторе по той же раскладке, что и dev-запуск player.
pub struct OpenProject {
    /// Абсолютный путь каталога проекта.
    pub root: PathBuf,
    pub settings: ProjectSettings,
    pub assets_dir: AssetsDir,
    pub assets: Assets,
    resources: ResourceSystem,
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
        Ok(Self {
            root: dir,
            settings,
            assets_dir: AssetsDir::new(root),
            assets,
            resources,
        })
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
            let mut world = World::from_document(&document).unwrap();
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
}
