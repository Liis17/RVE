use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Validated contents of a `project.rveproject` file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSettings {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub heavy_scripts: Option<String>,
    #[serde(default)]
    pub scenes: Vec<ProjectScene>,
    #[serde(default)]
    pub requirements: ProjectRequirements,
    #[serde(default)]
    pub runtime: RuntimeSettings,
}

/// A scene listed by a project, in project order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectScene {
    pub path: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Minimum hardware resources required by a project.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRequirements {
    #[serde(default)]
    pub min_ram_mb: u64,
    #[serde(default)]
    pub min_cpu_threads: u32,
}

/// Runtime scheduling settings for a project.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    #[serde(default = "default_background_tick_hz")]
    pub background_tick_hz: u32,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            background_tick_hz: default_background_tick_hz(),
        }
    }
}

impl ProjectSettings {
    /// Parses TOML contents, reporting errors against `project.rveproject`.
    pub fn parse(contents: &str) -> Result<Self, String> {
        Self::parse_with_path(contents, "project.rveproject")
    }

    /// Loads and validates a project settings file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let contents =
            fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        Self::parse_with_path(&contents, &path.display().to_string())
    }

    /// Returns the first enabled scene path, which is the project's startup scene.
    pub fn starting_scene(&self) -> Option<&str> {
        self.scenes
            .iter()
            .find(|scene| scene.enabled)
            .map(|scene| scene.path.as_str())
    }

    /// Serializes the validated settings as TOML.
    pub fn to_toml(&self) -> Result<String, String> {
        self.validate()
            .map_err(|error| format!("project.rveproject: {error}"))?;
        toml::to_string(self).map_err(|error| format!("project.rveproject: {error}"))
    }

    fn parse_with_path(contents: &str, path: &str) -> Result<Self, String> {
        let settings: Self =
            toml::from_str(contents).map_err(|error| format!("{path}: {error}"))?;
        settings
            .validate()
            .map_err(|error| format!("{path}: {error}"))?;
        Ok(settings)
    }

    fn validate(&self) -> Result<(), String> {
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err("id должен соответствовать [A-Za-z0-9_-]{1,64}".to_owned());
        }
        if self.name.trim().is_empty() {
            return Err("name не должен быть пустым".to_owned());
        }
        if self.version.trim().is_empty() {
            return Err("version не должен быть пустым".to_owned());
        }
        if let Some(path) = &self.heavy_scripts
            && !is_relative_project_path(path)
        {
            return Err(
                "heavy_scripts должен быть непустым относительным путём внутри каталога проекта"
                    .to_owned(),
            );
        }

        let mut scene_paths = HashSet::new();
        let mut has_enabled_scene = false;
        for scene in &self.scenes {
            let virtual_path = VirtualPath::parse(&scene.path)
                .map_err(|_| format!("некорректный путь сцены {:?}", scene.path))?;
            if !virtual_path.as_str().ends_with(".rvescene") {
                return Err(format!(
                    "путь сцены {:?} должен оканчиваться на .rvescene",
                    scene.path
                ));
            }
            if !scene_paths.insert(virtual_path) {
                return Err(format!("путь сцены {:?} указан повторно", scene.path));
            }
            has_enabled_scene |= scene.enabled;
        }
        if !has_enabled_scene {
            return Err("проект должен содержать хотя бы одну включённую сцену".to_owned());
        }
        if self.runtime.background_tick_hz == 0 {
            return Err("runtime.background_tick_hz должен быть больше нуля".to_owned());
        }

        Ok(())
    }
}

fn is_relative_project_path(value: &str) -> bool {
    let path = PathBuf::from(value);
    let bytes = value.as_bytes();
    let windows_drive_prefix =
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';

    !value.trim().is_empty()
        && !path.is_absolute()
        && !value.starts_with(['/', '\\'])
        && !windows_drive_prefix
        && !value.split(['/', '\\']).any(|component| component == "..")
}

fn default_enabled() -> bool {
    true
}

fn default_background_tick_hz() -> u32 {
    30
}

#[cfg(test)]
mod tests {
    use super::{ProjectSettings, RuntimeSettings};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

    fn temporary_file() -> PathBuf {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rve-project-{}-{id}.rveproject",
            std::process::id()
        ))
    }

    fn project(scene_lines: &str) -> String {
        format!("id = \"demo-game\"\nname = \"Demo Game\"\nversion = \"1.2.3\"\n{scene_lines}")
    }

    #[test]
    fn parses_project_scenes_settings_and_startup_scene() {
        let settings = ProjectSettings::parse(
            "id = \"demo-game\"\nname = \"Demo Game\"\nversion = \"1.2.3\"\nheavy_scripts = \"HeavyScripts\"\n\
             [[scenes]]\npath = \"/levels/menu.rvescene\"\nenabled = false\n\
             [[scenes]]\npath = \"levels/main.rvescene\"\n\
             [requirements]\nmin_ram_mb = 512\nmin_cpu_threads = 4\n\
             [runtime]\nbackground_tick_hz = 45\n",
        )
        .unwrap();

        assert_eq!(settings.id, "demo-game");
        assert_eq!(settings.name, "Demo Game");
        assert_eq!(settings.version, "1.2.3");
        assert_eq!(settings.heavy_scripts.as_deref(), Some("HeavyScripts"));
        assert_eq!(settings.scenes.len(), 2);
        assert!(!settings.scenes[0].enabled);
        assert!(settings.scenes[1].enabled);
        assert_eq!(settings.starting_scene(), Some("levels/main.rvescene"));
        assert_eq!(settings.requirements.min_ram_mb, 512);
        assert_eq!(settings.requirements.min_cpu_threads, 4);
        assert_eq!(settings.runtime.background_tick_hz, 45);
    }

    #[test]
    fn defaults_optional_project_values_and_scene_enabled() {
        let settings =
            ProjectSettings::parse(&project("[[scenes]]\npath = \"scenes/start.rvescene\"\n"))
                .unwrap();

        assert_eq!(settings.heavy_scripts, None);
        assert!(settings.scenes[0].enabled);
        assert_eq!(settings.requirements.min_ram_mb, 0);
        assert_eq!(settings.requirements.min_cpu_threads, 0);
        assert_eq!(settings.runtime, RuntimeSettings::default());
        assert_eq!(settings.runtime.background_tick_hz, 30);
        assert_eq!(settings.starting_scene(), Some("scenes/start.rvescene"));
    }

    #[test]
    fn validates_project_ids_names_and_versions() {
        for (field, value) in [
            ("id", ""),
            ("id", "bad id"),
            ("id", "bad.id"),
            ("name", ""),
            ("name", "   "),
            ("version", ""),
            ("version", "  "),
        ] {
            let mut contents = project("[[scenes]]\npath = \"scene.rvescene\"\n");
            let marker = match field {
                "id" => "id = \"demo-game\"",
                "name" => "name = \"Demo Game\"",
                _ => "version = \"1.2.3\"",
            };
            contents = contents.replacen(marker, &format!("{field} = {value:?}"), 1);
            let error = ProjectSettings::parse(&contents).unwrap_err();
            assert!(error.contains(field), "{error}");
        }

        let long_id = "a".repeat(65);
        let valid_long_id = "a".repeat(64);
        let contents =
            project("[[scenes]]\npath = \"scene.rvescene\"\n").replace("demo-game", &valid_long_id);
        assert_eq!(ProjectSettings::parse(&contents).unwrap().id.len(), 64);
        let contents =
            project("[[scenes]]\npath = \"scene.rvescene\"\n").replace("demo-game", &long_id);
        assert!(ProjectSettings::parse(&contents).is_err());
    }

    #[test]
    fn requires_at_least_one_enabled_scene_and_unique_virtual_paths() {
        for scenes in [
            "",
            "[[scenes]]\npath = \"scene.rvescene\"\nenabled = false\n",
            "[[scenes]]\npath = \"scene.rvescene\"\n[[scenes]]\npath = \"/scene.rvescene\"\n",
        ] {
            let error = ProjectSettings::parse(&project(scenes)).unwrap_err();
            assert!(error.contains("project.rveproject"), "{error}");
        }
        let duplicate = ProjectSettings::parse(&project(
            "[[scenes]]\npath = \"scene.rvescene\"\n[[scenes]]\npath = \"/scene.rvescene\"\n",
        ))
        .unwrap_err();
        assert!(duplicate.contains("повторно"), "{duplicate}");
    }

    #[test]
    fn rejects_invalid_scene_paths_heavy_script_paths_and_zero_background_rate() {
        for path in [
            "",
            "/",
            "//scene.rvescene",
            "scene//main.rvescene",
            "scene/./main.rvescene",
            "scene/../main.rvescene",
            "scene\\main.rvescene",
            "scene:name.rvescene",
            "scene.txt",
        ] {
            let contents = project(&format!("[[scenes]]\npath = {path:?}\n"));
            let error = ProjectSettings::parse(&contents).unwrap_err();
            assert!(error.contains("сцены"), "{path:?}: {error}");
        }

        for path in [
            "",
            "/scripts",
            "C:/scripts",
            "\\\\server\\share",
            "../scripts",
            "scripts/../source",
            "..\\scripts",
            "scripts\\..\\source",
        ] {
            let contents = format!(
                "id = \"demo-game\"\nname = \"Demo Game\"\nversion = \"1\"\nheavy_scripts = {path:?}\n[[scenes]]\npath = \"scene.rvescene\"\n"
            );
            let error = ProjectSettings::parse(&contents).unwrap_err();
            assert!(error.contains("heavy_scripts"), "{path:?}: {error}");
        }

        let error = ProjectSettings::parse(&format!(
            "{}\n[runtime]\nbackground_tick_hz = 0\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        ))
        .unwrap_err();
        assert!(error.contains("background_tick_hz"), "{error}");
    }

    #[test]
    fn rejects_unknown_fields_and_non_string_version() {
        let unknown = ProjectSettings::parse(
            "id = \"demo-game\"\nname = \"Demo Game\"\nversion = \"1\"\nextra = 1\n\
             [[scenes]]\npath = \"scene.rvescene\"\n",
        )
        .unwrap_err();
        assert!(unknown.contains("project.rveproject"), "{unknown}");

        let nested_unknown = ProjectSettings::parse(&format!(
            "{}\n[requirements]\nunknown = 1\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        ))
        .unwrap_err();
        assert!(nested_unknown.contains("unknown"), "{nested_unknown}");

        let non_string = project("[[scenes]]\npath = \"scene.rvescene\"\n")
            .replace("version = \"1.2.3\"", "version = 123");
        assert!(ProjectSettings::parse(&non_string).is_err());
    }

    #[test]
    fn serializes_and_reloads_project_settings() {
        let settings =
            ProjectSettings::parse(&project("[[scenes]]\npath = \"scene/start.rvescene\"\n"))
                .unwrap();
        let serialized = settings.to_toml().unwrap();
        assert_eq!(ProjectSettings::parse(&serialized).unwrap(), settings);

        let invalid = ProjectSettings {
            runtime: RuntimeSettings {
                background_tick_hz: 0,
            },
            ..settings
        };
        assert!(invalid.to_toml().is_err());
    }

    #[test]
    fn loads_from_file_and_reports_read_errors_with_the_path() {
        let path = temporary_file();
        fs::write(
            &path,
            project("[[scenes]]\npath = \"scene/start.rvescene\"\n"),
        )
        .unwrap();
        let settings = ProjectSettings::load(&path).unwrap();
        assert_eq!(settings.starting_scene(), Some("scene/start.rvescene"));
        fs::remove_file(&path).unwrap();

        let invalid_utf8 = temporary_file();
        fs::write(&invalid_utf8, [0xff]).unwrap();
        let error = ProjectSettings::load(&invalid_utf8).unwrap_err();
        assert!(
            error.contains(&invalid_utf8.display().to_string()),
            "{error}"
        );
        fs::remove_file(invalid_utf8).unwrap();
    }
}
