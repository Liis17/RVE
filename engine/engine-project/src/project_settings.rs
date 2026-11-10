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
    /// Звуковой вывод проекта: `[audio]`. Значения по умолчанию не сериализуются.
    #[serde(default, skip_serializing_if = "AudioSettings::is_default")]
    pub audio: AudioSettings,
    /// Горячие клавиши окна игры: `[window]`. Значения по умолчанию не сериализуются.
    #[serde(default, skip_serializing_if = "WindowSettings::is_default")]
    pub window: WindowSettings,
    /// Значения по умолчанию не сериализуются, поэтому проекты без `[network]` не меняются.
    #[serde(default, skip_serializing_if = "NetworkSettings::is_default")]
    pub network: NetworkSettings,
    #[serde(default)]
    pub input: InputSettings,
    /// Теги объектов: `[tags] names = [...]`. Пустой список не сериализуется.
    #[serde(default, skip_serializing_if = "TagSettings::is_default")]
    pub tags: TagSettings,
    /// Слои объектов: `[layers] names = [...]`. Индекс имени — значение `layer` объекта.
    #[serde(default, skip_serializing_if = "LayerSettings::is_default")]
    pub layers: LayerSettings,
}

/// Наибольшее число слоёв проекта; `layer` объекта меньше этого значения.
pub const MAX_LAYERS: usize = 32;

/// Теги проекта, которые можно назначить объектам сцены.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TagSettings {
    /// Имена тегов `[A-Za-z0-9_-]{1,64}`, без повторов.
    pub names: Vec<String>,
}

impl TagSettings {
    fn is_default(&self) -> bool {
        self.names.is_empty()
    }

    fn validate(&self) -> Result<(), String> {
        validate_names("tags", &self.names, usize::MAX, false)
    }
}

/// Слои проекта: до 32 имён, индекс имени совпадает со значением `layer` объекта.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayerSettings {
    /// Имена слоёв `[A-Za-z0-9_-]{1,64}`, без повторов; не более [`MAX_LAYERS`]. Пустая строка —
    /// слот без имени: индексы остальных слоёв от этого не меняются.
    pub names: Vec<String>,
}

impl LayerSettings {
    fn is_default(&self) -> bool {
        self.names.is_empty()
    }

    fn validate(&self) -> Result<(), String> {
        validate_names("layers", &self.names, MAX_LAYERS, true)
    }
}

fn validate_names(
    section: &str,
    names: &[String],
    limit: usize,
    allow_empty: bool,
) -> Result<(), String> {
    if names.len() > limit {
        return Err(format!(
            "[{section}] names допускает не больше {limit} имён"
        ));
    }
    let mut seen = HashSet::with_capacity(names.len());
    for name in names {
        if allow_empty && name.is_empty() {
            continue;
        }
        let valid = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if !valid {
            return Err(format!(
                "имя в [{section}] {name:?} должно соответствовать [A-Za-z0-9_-]{{1,64}}"
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err(format!("имя в [{section}] {name:?} указано повторно"));
        }
    }
    Ok(())
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

/// Звуковые настройки проекта: `[audio] mono_output = true` сводит весь вывод к моно.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioSettings {
    pub mono_output: bool,
}

impl AudioSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Встроенные горячие клавиши окна игры. Обе включены по умолчанию.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowSettings {
    /// F11 переключает полноэкранный режим без рамки и обратно.
    pub fullscreen_hotkey: bool,
    /// F12 сохраняет PNG-скриншот окна в пользовательскую папку данных проекта.
    pub screenshot_hotkey: bool,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            fullscreen_hotkey: true,
            screenshot_hotkey: true,
        }
    }
}

impl WindowSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Fixed-step simulation settings for a project.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhysicsSettings {
    pub gravity: [f32; 3],
    pub fixed_hz: u32,
    pub max_substeps: u32,
    /// Матрица слоёв: `[physics.collision_matrix] masks = [32 чисел]`. Значения по умолчанию
    /// не сериализуются, поэтому проекты без таблицы сталкиваются все со всеми.
    #[serde(default, skip_serializing_if = "is_default_collision_matrix")]
    pub collision_matrix: CollisionMatrixSettings,
}

/// Маски слоёв для столкновений. Слой `a` и слой `b` сталкиваются, только если бит `b` есть в
/// маске `a` и бит `a` есть в маске `b`; поэтому маски задаются симметрично.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollisionMatrixSettings {
    /// Ровно [`MAX_LAYERS`] значений; индекс массива — индекс слоя из `[layers]`.
    #[serde(default = "default_collision_masks")]
    pub masks: [u32; MAX_LAYERS],
}

impl Default for CollisionMatrixSettings {
    fn default() -> Self {
        Self {
            masks: default_collision_masks(),
        }
    }
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
            && self.collision_matrix == other.collision_matrix
    }
}

impl Eq for PhysicsSettings {}

impl Default for PhysicsSettings {
    fn default() -> Self {
        Self {
            gravity: [0.0, -9.81, 0.0],
            fixed_hz: 60,
            max_substeps: 8,
            collision_matrix: CollisionMatrixSettings::default(),
        }
    }
}

fn default_collision_masks() -> [u32; MAX_LAYERS] {
    [u32::MAX; MAX_LAYERS]
}

fn is_default_collision_matrix(matrix: &CollisionMatrixSettings) -> bool {
    *matrix == CollisionMatrixSettings::default()
}

/// Сетевые настройки проекта: `[network]`. Пароль и ключ сервера в проект не записываются.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSettings {
    /// Включает сетевой режим в player; без флагов сети приложение работает offline.
    pub enabled: bool,
    /// Порт сервера по умолчанию, 1..=65535.
    pub port: u16,
    /// Наибольшее число подключённых клиентов, 1..=64.
    pub max_clients: u32,
    /// Частота сетевого тика, 1..=240 Гц.
    pub tick_hz: u32,
    /// Тайм-аут молчащего пира, 1000..=120000 мс.
    pub timeout_ms: u32,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7777,
            max_clients: 8,
            tick_hz: 30,
            timeout_ms: 10_000,
        }
    }
}

impl NetworkSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
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
        self.tags.validate()?;
        self.layers.validate()?;
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
        if self.network.port == 0 {
            return Err("network.port должен быть в диапазоне 1..=65535".to_owned());
        }
        if !(1..=64).contains(&self.network.max_clients) {
            return Err("network.max_clients должен быть в диапазоне 1..=64".to_owned());
        }
        if !(1..=240).contains(&self.network.tick_hz) {
            return Err("network.tick_hz должен быть в диапазоне 1..=240".to_owned());
        }
        if !(1000..=120_000).contains(&self.network.timeout_ms) {
            return Err("network.timeout_ms должен быть в диапазоне 1000..=120000".to_owned());
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
    use super::{NetworkSettings, PhysicsSettings, ProjectSettings, RuntimeSettings};
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
    fn tags_and_layers_parse_round_trip_and_stay_out_of_default_files() {
        let plain =
            ProjectSettings::parse(&project("[[scenes]]\npath = \"scene.rvescene\"\n")).unwrap();
        assert!(plain.tags.names.is_empty());
        assert!(plain.layers.names.is_empty());
        let serialized = plain.to_toml().unwrap();
        assert!(!serialized.contains("tags"), "{serialized}");
        assert!(!serialized.contains("layers"), "{serialized}");

        let settings = ProjectSettings::parse(&format!(
            "{}\n[tags]\nnames = [\"Player\", \"enemy_1\"]\n[layers]\nnames = [\"Default\", \"UI\"]\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        ))
        .unwrap();
        assert_eq!(settings.tags.names, ["Player", "enemy_1"]);
        assert_eq!(settings.layers.names, ["Default", "UI"]);
        let again = ProjectSettings::parse(&settings.to_toml().unwrap()).unwrap();
        assert_eq!(again, settings);
    }

    #[test]
    fn cleared_layer_slots_are_allowed_but_empty_tags_and_duplicates_are_not() {
        let scene = "[[scenes]]\npath = \"scene.rvescene\"\n";
        let settings = ProjectSettings::parse(&project(&format!(
            "{scene}[layers]\nnames = [\"Default\", \"\", \"UI\", \"\"]\n"
        )))
        .unwrap();
        assert_eq!(settings.layers.names, ["Default", "", "UI", ""]);
        let text = settings.to_toml().unwrap();
        assert_eq!(
            ProjectSettings::parse(&text).unwrap().layers.names,
            settings.layers.names
        );

        let error = ProjectSettings::parse(&project(&format!("{scene}[tags]\nnames = [\"\"]\n")))
            .unwrap_err();
        assert!(error.contains("[tags]"), "{error}");
        let error = ProjectSettings::parse(&project(&format!(
            "{scene}[layers]\nnames = [\"UI\", \"UI\"]\n"
        )))
        .unwrap_err();
        assert!(error.contains("[layers]"), "{error}");
    }

    #[test]
    fn layers_accept_exactly_32_names_and_reject_more() {
        let names = (0..32)
            .map(|index| format!("\"layer{index}\""))
            .collect::<Vec<_>>();
        let accepted = format!(
            "{}\n[layers]\nnames = [{}]\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n"),
            names.join(", ")
        );
        assert_eq!(
            ProjectSettings::parse(&accepted)
                .unwrap()
                .layers
                .names
                .len(),
            32
        );

        let rejected = format!(
            "{}\n[layers]\nnames = [{}, \"layer32\"]\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n"),
            names.join(", ")
        );
        let error = ProjectSettings::parse(&rejected).unwrap_err();
        assert!(error.contains("[layers]"), "{error}");
    }

    #[test]
    fn tag_and_layer_names_must_be_unique_identifiers() {
        let scene = project("[[scenes]]\npath = \"scene.rvescene\"\n");
        let long_name = "x".repeat(65);
        let sections = [
            "[tags]\nnames = [\"Player\", \"Player\"]\n".to_owned(),
            "[tags]\nnames = [\"bad name\"]\n".to_owned(),
            "[tags]\nnames = [\"\"]\n".to_owned(),
            "[layers]\nnames = [\"UI\", \"UI\"]\n".to_owned(),
            format!("[layers]\nnames = [\"{long_name}\"]\n"),
        ];
        for section in sections {
            let error = ProjectSettings::parse(&format!("{scene}\n{section}")).unwrap_err();
            assert!(
                error.contains("[tags]") || error.contains("[layers]"),
                "{section}: {error}"
            );
        }
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
                collision_matrix: Default::default(),
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
    fn collision_matrix_defaults_to_all_pairs_and_stays_out_of_default_files() {
        let settings =
            ProjectSettings::parse(&project("[[scenes]]\npath = \"scene.rvescene\"\n")).unwrap();

        assert_eq!(
            settings.physics.collision_matrix.masks,
            [u32::MAX; super::MAX_LAYERS]
        );
        assert!(!settings.to_toml().unwrap().contains("collision_matrix"));
    }

    #[test]
    fn collision_matrix_parses_and_round_trips() {
        let mut masks = [u32::MAX; super::MAX_LAYERS];
        masks[0] = 0b01;
        masks[1] = 0b10;
        let list = masks
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let contents = format!(
            "{}\n[physics.collision_matrix]\nmasks = [{list}]\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        );

        let settings = ProjectSettings::parse(&contents).unwrap();
        assert_eq!(settings.physics.collision_matrix.masks, masks);
        let reparsed = ProjectSettings::parse(&settings.to_toml().unwrap()).unwrap();
        assert_eq!(reparsed.physics, settings.physics);
    }

    #[test]
    fn collision_matrix_requires_exactly_one_mask_per_layer() {
        let contents = format!(
            "{}\n[physics.collision_matrix]\nmasks = [1, 2]\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        );

        let error = ProjectSettings::parse(&contents).unwrap_err();
        assert!(error.contains("array of length 32"), "{error}");
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
    fn window_hotkeys_default_on_and_can_be_disabled() {
        const SCENE: &str = "[[scenes]]\npath = \"scene.rvescene\"\n";
        let defaults = ProjectSettings::parse(&project(SCENE)).unwrap();
        assert!(defaults.window.fullscreen_hotkey);
        assert!(defaults.window.screenshot_hotkey);
        assert!(!defaults.to_toml().unwrap().contains("[window]"));

        let custom = ProjectSettings::parse(&format!(
            "{}\n[window]\nfullscreen_hotkey = false\n",
            project(SCENE)
        ))
        .unwrap();
        assert!(!custom.window.fullscreen_hotkey);
        assert!(custom.window.screenshot_hotkey);
        assert_eq!(
            ProjectSettings::parse(&custom.to_toml().unwrap()).unwrap(),
            custom
        );

        let unknown =
            ProjectSettings::parse(&format!("{}\n[window]\nvsync = true\n", project(SCENE)));
        assert!(unknown.unwrap_err().contains("vsync"));
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
    fn network_section_has_documented_defaults_and_is_not_serialized_when_default() {
        let settings =
            ProjectSettings::parse(&project("[[scenes]]\npath = \"scene.rvescene\"\n")).unwrap();
        assert_eq!(settings.network, NetworkSettings::default());
        assert!(!settings.network.enabled);
        assert_eq!(settings.network.port, 7777);
        assert_eq!(settings.network.max_clients, 8);
        assert_eq!(settings.network.tick_hz, 30);
        assert_eq!(settings.network.timeout_ms, 10_000);

        let serialized = settings.to_toml().unwrap();
        assert!(!serialized.contains("network"), "{serialized}");
        let again = ProjectSettings::parse(&serialized).unwrap();
        assert_eq!(again, settings);
    }

    #[test]
    fn network_section_round_trips_and_checks_ranges() {
        let settings = ProjectSettings::parse(&format!(
            "{}\n[network]\nenabled = true\nport = 9000\nmax_clients = 64\ntick_hz = 240\ntimeout_ms = 120000\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        ))
        .unwrap();
        assert_eq!(
            settings.network,
            NetworkSettings {
                enabled: true,
                port: 9000,
                max_clients: 64,
                tick_hz: 240,
                timeout_ms: 120_000,
            }
        );
        let serialized = settings.to_toml().unwrap();
        assert!(serialized.contains("[network]"), "{serialized}");
        assert_eq!(ProjectSettings::parse(&serialized).unwrap(), settings);

        for (field, value) in [
            ("port", "0"),
            ("max_clients", "0"),
            ("max_clients", "65"),
            ("tick_hz", "0"),
            ("tick_hz", "241"),
            ("timeout_ms", "999"),
            ("timeout_ms", "120001"),
        ] {
            let contents = format!(
                "{}\n[network]\n{field} = {value}\n",
                project("[[scenes]]\npath = \"scene.rvescene\"\n")
            );
            let error = ProjectSettings::parse(&contents).unwrap_err();
            assert!(
                error.contains(&format!("network.{field}")),
                "{field}={value}: {error}"
            );
        }

        // Порт вне u16 отсекает разбор TOML до проверки диапазона.
        assert!(
            ProjectSettings::parse(&format!(
                "{}\n[network]\nport = 65536\n",
                project("[[scenes]]\npath = \"scene.rvescene\"\n")
            ))
            .is_err()
        );

        let unknown = ProjectSettings::parse(&format!(
            "{}\n[network]\nsecret = \"x\"\n",
            project("[[scenes]]\npath = \"scene.rvescene\"\n")
        ))
        .unwrap_err();
        assert!(unknown.contains("secret"), "{unknown}");
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
