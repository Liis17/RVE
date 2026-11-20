use crate::archives::validate_project_archives;
use crate::error::BuildError;
use crate::player::player_file_name;
use crate::project::{ProjectBuild, prepare_project};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Сводка успешной проверки проекта.
pub(crate) struct CheckSummary {
    project: String,
    scenes: usize,
    resources: usize,
    /// Архивы в порядке сборки и число назначенных им ресурсов.
    archives: Vec<(String, usize)>,
}

/// Проверяет проект тем же разбором, что и `build`, но без сборки архивов, player и скриптов.
/// Ничего не записывает на диск и не запускает Cargo.
pub(crate) fn check_project(project_dir: &Path) -> Result<CheckSummary, BuildError> {
    let project = prepare_project(project_dir)?;
    validate_project_archives(&project)?;
    player_file_name(&project.settings.name, &project.settings.id)?;
    Ok(CheckSummary::new(&project))
}

impl CheckSummary {
    fn new(project: &ProjectBuild) -> Self {
        let mut counts = BTreeMap::<&str, usize>::new();
        for archive in project.assignments.values() {
            *counts.entry(archive.as_str()).or_default() += 1;
        }
        Self {
            project: project.settings.name.clone(),
            scenes: project
                .settings
                .scenes
                .iter()
                .filter(|scene| scene.enabled)
                .count(),
            resources: project.assignments.len(),
            archives: project
                .archives
                .iter()
                .map(|name| {
                    let count = counts.get(name.as_str()).copied().unwrap_or(0);
                    (name.clone(), count)
                })
                .collect(),
        }
    }
}

impl fmt::Display for CheckSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "проверка пройдена: проект {:?}, сцен {}, ресурсов {}",
            self.project, self.scenes, self.resources
        )?;
        for (name, count) in &self.archives {
            writeln!(formatter, "архив {name}: ресурсов {count}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::check_project;
    use crate::archives::tests::create_demo_like_project;
    use crate::temporary::TempDir;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    const ROTATE: &str =
        "return { properties = { speed = { type = \"float\", default = 30, min = 0 } } }\n";

    /// Все пути проекта: по ним видно, что проверка ничего не создала и не изменила.
    fn tree(root: &Path) -> BTreeSet<PathBuf> {
        let mut found = BTreeSet::new();
        let mut pending = vec![root.to_owned()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path.clone());
                }
                found.insert(path);
            }
        }
        found
    }

    /// Минимальный проект: одна включённая сцена `/scenes/main.rvescene` с заданным текстом.
    fn small_project(root: &Path, name: &str, scene: &str) {
        let assets = root.join("Assets");
        fs::create_dir_all(assets.join("scenes")).unwrap();
        fs::write(
            root.join("project.rveproject"),
            format!(
                "id = \"game\"\nname = {name:?}\nversion = \"1\"\n[[scenes]]\npath = \"/scenes/main.rvescene\"\n"
            ),
        )
        .unwrap();
        fs::write(root.join("bootstrap.rves"), "").unwrap();
        fs::write(assets.join("scenes/main.rvescene"), scene).unwrap();
    }

    fn write_asset(root: &Path, path: &str, text: &str) {
        let file = root.join("Assets").join(path.trim_start_matches('/'));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }

    /// Добавляет `[[assets]]` с путём `path` перед первой записью главной сцены демо-проекта.
    fn declare_in_main_scene(root: &Path, path: &str) {
        let scene = root.join("Assets/scenes/main.rvescene");
        let text = fs::read_to_string(&scene).unwrap();
        let patched = text.replacen(
            "[[assets]]",
            &format!("[[assets]]\npath = \"{path}\"\n[[assets]]"),
            1,
        );
        assert_ne!(patched, text);
        fs::write(scene, patched).unwrap();
    }

    #[test]
    fn valid_demo_project_passes_and_reports_its_archives_without_writing() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-check-demo").unwrap();
        create_demo_like_project(root.path());
        let before = tree(root.path());

        let summary = check_project(root.path()).expect("демо-проект проходит проверку");
        let text = summary.to_string();

        assert_eq!(tree(root.path()), before, "проверка не должна писать файлы");
        assert!(text.contains("сцен 2"), "{text}");
        assert!(text.contains("ресурсов 11"), "{text}");
        for archive in ["base", "shared", "props", "scene-main"] {
            assert!(text.contains(&format!("архив {archive}:")), "{text}");
        }
    }

    #[test]
    fn missing_declared_resource_is_reported_with_its_virtual_path() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-check-missing-asset").unwrap();
        create_demo_like_project(root.path());
        declare_in_main_scene(root.path(), "/textures/missing.png");

        let error = check_project(root.path()).err().unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(
            error.to_string().contains("/textures/missing.png"),
            "{error}"
        );
    }

    #[test]
    fn missing_scene_file_is_reported_with_its_virtual_path() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-check-missing-scene").unwrap();
        create_demo_like_project(root.path());
        fs::remove_file(root.path().join("Assets/scenes/main.rvescene")).unwrap();

        let error = check_project(root.path()).err().unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(
            error.to_string().contains("/scenes/main.rvescene"),
            "{error}"
        );
        assert!(error.to_string().contains("не найден"), "{error}");
    }

    #[test]
    fn invalid_lite_params_name_the_object_and_script() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-check-params").unwrap();
        let scene = format!(
            "[[assets]]\npath = \"/scripts/rotate.lua\"\n{CAMERA}[[objects.lite_scripts]]\npath = \"/scripts/rotate.lua\"\nparams = {{ speed = -1.0 }}\n"
        );
        small_project(root.path(), "Game", &scene);
        write_asset(root.path(), "/scripts/rotate.lua", ROTATE);

        let error = check_project(root.path()).err().unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(error.to_string().contains("\"camera\""), "{error}");
        assert!(error.to_string().contains("/scripts/rotate.lua"), "{error}");
    }

    #[test]
    fn reserved_release_path_in_assets_is_rejected() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-check-reserved").unwrap();
        let scene = format!("[[assets]]\npath = \"/rve/assets.toml\"\n{CAMERA}");
        small_project(root.path(), "Game", &scene);
        write_asset(root.path(), "/rve/assets.toml", "archives = []\n");

        let error = check_project(root.path()).err().unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(error.to_string().contains("/rve/assets.toml"), "{error}");
    }

    #[test]
    fn heavy_reference_is_not_compiled_so_a_missing_heavy_directory_passes() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-check-heavy").unwrap();
        let scene = format!(
            "{CAMERA}[[objects]]\nid = \"mesh\"\n[[objects.heavy_scripts]]\nname = \"Spin\"\n"
        );
        small_project(root.path(), "Game", &scene);
        fs::write(
            root.path().join("project.rveproject"),
            "id = \"game\"\nname = \"Game\"\nversion = \"1\"\nheavy_scripts = \"HeavyScripts\"\n[[scenes]]\npath = \"/scenes/main.rvescene\"\n",
        )
        .unwrap();

        let summary = check_project(root.path()).expect("cdylib не собирается и не проверяется");
        assert!(summary.to_string().contains("сцен 1"));
        assert!(!root.path().join("HeavyScripts").exists());
    }

    #[test]
    fn heavy_reference_without_heavy_scripts_setting_is_a_project_error() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-check-heavy").unwrap();
        let scene = format!(
            "{CAMERA}[[objects]]\nid = \"mesh\"\n[[objects.heavy_scripts]]\nname = \"Spin\"\n"
        );
        small_project(root.path(), "Game", &scene);

        let error = check_project(root.path()).err().unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(error.to_string().contains("heavy_scripts"), "{error}");
        assert!(error.to_string().contains("Spin"), "{error}");
    }

    #[test]
    fn project_name_that_cannot_be_an_executable_is_rejected() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-check-name").unwrap();
        small_project(root.path(), "Assets", CAMERA);

        let error = check_project(root.path()).err().unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(error.to_string().contains("Assets"), "{error}");
    }
}
