use crate::input_settings::InputSettings;
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
    #[serde(default)]
    pub physics: PhysicsSettings,
    #[serde(default)]
    pub input: InputSettings,
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
    #[serde(default)]
    pub fsr_library: Option<String>,
    #[serde(default)]
    pub dlss_library: Option<String>,
    #[serde(default)]
    pub dlss_project_id: Option<String>,
    #[serde(default)]
    pub dlss_application_id: Option<u64>,
    #[serde(default)]
    pub dlss_engine_version: Option<String>,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            background_tick_hz: default_background_tick_hz(),
            fsr_library: None,
            dlss_library: None,
            dlss_project_id: None,
            dlss_application_id: None,
            dlss_engine_version: None,
        }
    }
}

/// Fixed-step simulation settings for a project.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhysicsSettings {
    pub gravity: [f32; 3],
    pub fixed_hz: u32,
    pub max_substeps: u32,
}

impl PartialEq for PhysicsSettings {
    fn eq(&self, other: &Self) -> bool {
        self.gravity
            .into_iter()
            .zip(other.gravity)
            .all(|(left, right)| {
                left == right
                    || (left.is_nan() && right.is_nan() && left.to_bits() == right.to_bits())
            })
            && self.fixed_hz == other.fixed_hz
            && self.max_substeps == other.max_substeps
    }
}

impl Eq for PhysicsSettings {}

impl Default for PhysicsSettings {
    fn default() -> Self {
        Self {
            gravity: [0.0, -9.81, 0.0],
            fixed_hz: 60,
            max_substeps: 8,
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
        self.input.validate()?;
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
        if !self.physics.gravity.into_iter().all(f32::is_finite) {
            return Err("physics.gravity должен содержать конечные числа".to_owned());
        }
        if !(1..=1000).contains(&self.physics.fixed_hz) {
            return Err("physics.fixed_hz должен быть в диапазоне 1..=1000".to_owned());
        }
        if !(1..=64).contains(&self.physics.max_substeps) {
            return Err("physics.max_substeps должен быть в диапазоне 1..=64".to_owned());
        }
        if let Some(path) = &self.runtime.fsr_library
            && (!is_relative_project_path(path)
                || !Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("dll")))
        {
            return Err(
                "runtime.fsr_library должен быть относительным путём к DLL внутри каталога проекта"
                    .to_owned(),
            );
        }
        if let Some(path) = &self.runtime.dlss_library
            && (!is_relative_project_path(path)
                || !Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case("nvngx_dlss.dll")))
        {
            return Err(
                "runtime.dlss_library должен указывать на nvngx_dlss.dll внутри каталога проекта"
                    .to_owned(),
            );
        }
        if self.runtime.dlss_library.is_some() {
            validate_dlss_identity(
                self.runtime.dlss_project_id.as_deref(),
                self.runtime.dlss_application_id,
                self.runtime.dlss_engine_version.as_deref(),
            )?;
        } else if self.runtime.dlss_project_id.is_some()
            || self.runtime.dlss_application_id.is_some()
            || self.runtime.dlss_engine_version.is_some()
        {
            return Err("runtime DLSS identifiers require runtime.dlss_library".to_owned());
        }

        Ok(())
    }
}

fn validate_dlss_identity(
    project_id: Option<&str>,
    application_id: Option<u64>,
    engine_version: Option<&str>,
) -> Result<(), String> {
    if project_id.is_some() == application_id.is_some() {
        return Err(
            "runtime DLSS requires exactly one of dlss_project_id or dlss_application_id"
                .to_owned(),
        );
    }
    if let Some(project_id) = project_id {
        let bytes = project_id.as_bytes();
        if bytes.len() != 36
            || !bytes.iter().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
        {
            return Err(
                "runtime.dlss_project_id должен быть GUID-подобным hex идентификатором".to_owned(),
            );
        }
        if engine_version.is_none_or(|version| {
            version.trim().is_empty() || version.len() > 64 || version.contains('\0')
        }) {
            return Err(
                "runtime.dlss_engine_version должен содержать от 1 до 64 символов без NUL"
                    .to_owned(),
            );
        }
    } else if application_id == Some(0) {
        return Err("runtime.dlss_application_id должен быть больше нуля".to_owned());
    } else if engine_version.is_some() {
        return Err("runtime.dlss_engine_version требует runtime.dlss_project_id".to_owned());
    }
    Ok(())
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
    use super::{PhysicsSettings, ProjectSettings, RuntimeSettings};
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
             [runtime]\nbackground_tick_hz = 45\n\
             [physics]\ngravity = [0.0, -4.0, 0.0]\nfixed_hz = 120\nmax_substeps = 6\n",
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
        assert_eq!(
            settings.physics,
            PhysicsSettings {
                gravity: [0.0, -4.0, 0.0],
                fixed_hz: 120,
                max_substeps: 6,
            }
        );
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
        assert_eq!(settings.runtime.fsr_library, None);
        assert_eq!(settings.runtime.dlss_library, None);
        assert_eq!(settings.runtime.dlss_project_id, None);
        assert_eq!(settings.runtime.dlss_application_id, None);
        assert_eq!(settings.runtime.dlss_engine_version, None);
        assert_eq!(settings.physics, PhysicsSettings::default());
        assert_eq!(settings.starting_scene(), Some("scenes/start.rvescene"));
    }

    #[test]
    fn rejects_invalid_physics_settings() {
        for (field, value) in [
            ("gravity", "[0.0, nan, 0.0]"),
            ("fixed_hz", "0"),
            ("fixed_hz", "1001"),
            ("max_substeps", "0"),
            ("max_substeps", "65"),
        ] {
            let contents = format!(
                "{}\n[physics]\n{field} = {value}\n",
                project("[[scenes]]\npath = \"scene.rvescene\"\n")
            );
            let error = ProjectSettings::parse(&contents).unwrap_err();
            assert!(error.contains("physics"), "{field}={value}: {error}");
        }
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
                fsr_library: None,
                ..RuntimeSettings::default()
            },
            ..settings.clone()
        };
        assert!(invalid.to_toml().is_err());

        let invalid = ProjectSettings {
            physics: PhysicsSettings {
                fixed_hz: 0,
                ..PhysicsSettings::default()
            },
            ..settings
        };
        assert!(invalid.to_toml().is_err());
    }

    #[test]
    fn validates_optional_fsr_loader_path() {
        let settings = ProjectSettings::parse(&format!(
            "{}\n[runtime]\nfsr_library = \"vendor/amd_fidelityfx_loader_vk.dll\"\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        ))
        .unwrap();
        assert_eq!(
            settings.runtime.fsr_library.as_deref(),
            Some("vendor/amd_fidelityfx_loader_vk.dll")
        );
        assert_eq!(
            ProjectSettings::parse(&settings.to_toml().unwrap()).unwrap(),
            settings
        );

        for path in [
            "",
            "../loader.dll",
            "C:/vendor/loader.dll",
            "vendor/loader.bin",
        ] {
            let contents = format!(
                "{}\n[runtime]\nfsr_library = {path:?}\n",
                project("[[scenes]]\npath = \"scene.rvescene\"\n")
            );
            assert!(
                ProjectSettings::parse(&contents)
                    .unwrap_err()
                    .contains("runtime.fsr_library"),
                "{path:?}"
            );
        }
    }

    #[test]
    fn validates_optional_dlss_runtime_and_identity() {
        let project_id = "a0f57b54-1daf-4934-90ae-c4035c19df04";
        let settings = ProjectSettings::parse(&format!(
            "{}\n[runtime]\ndlss_library = \"vendor/nvngx_dlss.dll\"\ndlss_project_id = {project_id:?}\ndlss_engine_version = \"0.1.0\"\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        ))
        .unwrap();
        assert_eq!(
            settings.runtime.dlss_library.as_deref(),
            Some("vendor/nvngx_dlss.dll")
        );
        assert_eq!(
            settings.runtime.dlss_project_id.as_deref(),
            Some(project_id)
        );
        assert_eq!(
            ProjectSettings::parse(&settings.to_toml().unwrap()).unwrap(),
            settings
        );

        let app_id = ProjectSettings::parse(&format!(
            "{}\n[runtime]\ndlss_library = \"vendor/nvngx_dlss.dll\"\ndlss_application_id = 42\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        ))
        .unwrap();
        assert_eq!(app_id.runtime.dlss_application_id, Some(42));

        for runtime in [
            "dlss_library = \"../nvngx_dlss.dll\"\ndlss_application_id = 42",
            "dlss_library = \"vendor/other.dll\"\ndlss_application_id = 42",
            "dlss_library = \"vendor/nvngx_dlss.dll\"",
            "dlss_library = \"vendor/nvngx_dlss.dll\"\ndlss_project_id = \"bad\"\ndlss_engine_version = \"0.1\"",
            "dlss_library = \"vendor/nvngx_dlss.dll\"\ndlss_project_id = \"a0f57b54-1daf-4934-90ae-c4035c19df04\"\ndlss_application_id = 42\ndlss_engine_version = \"0.1\"",
            "dlss_library = \"vendor/nvngx_dlss.dll\"\ndlss_application_id = 0",
            "dlss_library = \"vendor/nvngx_dlss.dll\"\ndlss_project_id = \"a0f57b54-1daf-4934-90ae-c4035c19df04\"\ndlss_engine_version = \" \"",
        ] {
            let contents = format!(
                "{}\n[runtime]\n{runtime}\n",
                project("[[scenes]]\npath = \"scene.rvescene\"\n")
            );
            let error = ProjectSettings::parse(&contents).unwrap_err();
            assert!(error.to_ascii_lowercase().contains("dlss"), "{runtime:?}");
        }
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
