//! Открытый проект: настройки, ресурсы в режиме loose и окно по `bootstrap.rves`.

use crate::assets_fs::AssetsDir;
use engine_assets::Assets;
use engine_project::{BootstrapConfig, ProjectSettings};
use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
use std::path::Path;

/// Проект игры, открытый в редакторе по той же раскладке, что и dev-запуск player.
pub struct OpenProject {
    pub settings: ProjectSettings,
    pub assets_dir: AssetsDir,
    pub assets: Assets,
    resources: ResourceSystem,
}

impl OpenProject {
    pub fn open(dir: &Path) -> Result<Self, String> {
        let dir =
            std::path::absolute(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
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
    fn demo_project_opens_and_lists_its_start_scene() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let project = OpenProject::open(Path::new(dir)).unwrap();
        let start = project.settings.starting_scene().unwrap();
        assert!(project.read_text(start).unwrap().contains("[[objects]]"));
    }
}
