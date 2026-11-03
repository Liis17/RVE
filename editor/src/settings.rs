//! Настройки пользователя между запусками: недавние проекты и раскладка панелей.
//!
//! Файл `settings.json` лежит в каталоге настроек ОС (`rve-editor/`). Повреждённое или
//! устаревшее поле заменяется значением по умолчанию и не мешает читать остальные.

use crate::layout::{Tab, default_layout};
use crate::scene_edit::write_atomic;
use egui_dock::DockState;
use std::path::{Path, PathBuf};

/// Сколько недавних проектов хранится.
pub const MAX_RECENT_PROJECTS: usize = 10;
const FILE_NAME: &str = "settings.json";

#[derive(Debug, Default)]
pub struct UserSettings {
    /// Каталоги проектов, последний открытый — первым.
    pub recent_projects: Vec<PathBuf>,
    /// Раскладка панелей; `None` — раскладка по умолчанию.
    pub layout: Option<DockState<Tab>>,
}

impl UserSettings {
    /// Поднимает проект в начало списка недавних без повторов.
    pub fn remember_project(recent: &mut Vec<PathBuf>, dir: PathBuf) {
        recent.retain(|known| known != &dir);
        recent.insert(0, dir);
        recent.truncate(MAX_RECENT_PROJECTS);
    }

    /// Сохранённая раскладка, если в ней нет повторяющихся панелей, иначе раскладка по умолчанию.
    pub fn layout_or_default(&self) -> DockState<Tab> {
        self.layout
            .as_ref()
            .filter(|layout| has_unique_tabs(layout))
            .cloned()
            .unwrap_or_else(default_layout)
    }
}

fn has_unique_tabs(layout: &DockState<Tab>) -> bool {
    Tab::ALL.iter().all(|tab| {
        layout
            .iter_all_tabs()
            .filter(|(_, open)| *open == tab)
            .count()
            <= 1
    })
}

/// Каталог настроек ОС: `%APPDATA%`, `~/Library/Application Support` или `$XDG_CONFIG_HOME`.
pub fn config_dir() -> Option<PathBuf> {
    let env = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if cfg!(target_os = "windows") {
        env("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".config")))
    }
}

/// Файл настроек в заданном каталоге; тесты передают изолированный каталог.
pub struct SettingsStore {
    file: PathBuf,
}

impl SettingsStore {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            file: dir.join(FILE_NAME),
        }
    }

    pub fn default_location() -> Option<Self> {
        Some(Self::in_dir(&config_dir()?.join("rve-editor")))
    }

    /// Читает настройки. Отсутствующий файл — не ошибка; иначе вместе с настройками
    /// возвращается описание проблемы для Console.
    pub fn load(&self) -> (UserSettings, Option<String>) {
        let text = match std::fs::read_to_string(&self.file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (UserSettings::default(), None);
            }
            Err(error) => {
                let message = format!("{}: {error}", self.file.display());
                return (UserSettings::default(), Some(message));
            }
        };
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => value,
            Err(error) => {
                let message = format!("{}: {error}; настройки сброшены", self.file.display());
                return (UserSettings::default(), Some(message));
            }
        };
        let mut problems = Vec::new();
        let settings = UserSettings {
            recent_projects: field(&value, "recent_projects", &mut problems).unwrap_or_default(),
            layout: field(&value, "layout", &mut problems),
        };
        let message = (!problems.is_empty()).then(|| {
            format!(
                "{}: {}; поля сброшены",
                self.file.display(),
                problems.join(", ")
            )
        });
        (settings, message)
    }

    /// Записывает настройки атомарно, создавая каталог при необходимости.
    pub fn save(&self, settings: &UserSettings) -> Result<(), String> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        }
        let value = serde_json::json!({
            "recent_projects": settings.recent_projects,
            "layout": settings.layout,
        });
        let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
        write_atomic(&self.file, text.as_bytes())
    }
}

/// Поле настроек; неразборчивое значение пропускается с записью в `problems`.
fn field<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    name: &str,
    problems: &mut Vec<String>,
) -> Option<T> {
    let field = value.get(name).filter(|field| !field.is_null())?.clone();
    match serde_json::from_value(field) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            problems.push(format!("{name}: {error}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::EditorTabs;
    use crate::state::EditorState;
    use egui_dock::DockArea;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rve-editor-settings-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tabs(layout: &DockState<Tab>) -> Vec<Tab> {
        layout.iter_all_tabs().map(|(_, tab)| *tab).collect()
    }

    #[test]
    fn missing_file_gives_defaults_without_a_message() {
        let dir = TempDir::new();
        let (settings, message) = SettingsStore::in_dir(&dir.0).load();
        assert!(settings.recent_projects.is_empty());
        assert!(settings.layout.is_none());
        assert_eq!(message, None);
    }

    #[test]
    fn settings_round_trip_including_a_layout_used_by_the_interface() {
        let dir = TempDir::new();
        let store = SettingsStore::in_dir(&dir.0.join("nested"));
        // Раскладка после кадра UI содержит вычисленные прямоугольники панелей.
        let mut layout = default_layout();
        let ctx = egui::Context::default();
        let mut state = EditorState::new(None);
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 720.0),
                )),
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                DockArea::new(&mut layout).show_inside(ui, &mut EditorTabs { state: &mut state });
            });
            output.textures_delta.clear();
        }
        // Закрытая панель не должна вернуться после перезапуска.
        let console = layout.find_tab(&Tab::Console).unwrap();
        layout.remove_tab(console);
        let settings = UserSettings {
            recent_projects: vec![PathBuf::from("a"), PathBuf::from("b")],
            layout: Some(layout.clone()),
        };
        store.save(&settings).unwrap();
        let (loaded, message) = store.load();
        assert_eq!(message, None);
        assert_eq!(loaded.recent_projects, settings.recent_projects);
        assert_eq!(tabs(&loaded.layout_or_default()), tabs(&layout));
        assert!(
            !std::fs::read_dir(dir.0.join("nested"))
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".tmp")
                })
        );
    }

    #[test]
    fn corrupt_file_or_field_falls_back_to_defaults_with_a_message() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        let store = SettingsStore::in_dir(&dir.0);
        std::fs::write(dir.0.join(FILE_NAME), "{ not json").unwrap();
        let (settings, message) = store.load();
        assert!(settings.recent_projects.is_empty());
        assert!(message.unwrap().contains("сброшены"));

        std::fs::write(
            dir.0.join(FILE_NAME),
            r#"{"recent_projects": ["kept"], "layout": {"surfaces": 5}}"#,
        )
        .unwrap();
        let (settings, message) = store.load();
        assert_eq!(settings.recent_projects, [PathBuf::from("kept")]);
        assert!(settings.layout.is_none());
        assert!(message.unwrap().contains("layout"));
    }

    #[test]
    fn layout_with_duplicate_panels_is_replaced_by_the_default() {
        let mut layout = DockState::new(vec![Tab::Scene, Tab::Scene]);
        layout.push_to_first_leaf(Tab::Console);
        let settings = UserSettings {
            recent_projects: Vec::new(),
            layout: Some(layout),
        };
        assert_eq!(tabs(&settings.layout_or_default()), tabs(&default_layout()));
    }

    #[test]
    fn remembered_projects_move_to_the_front_without_duplicates_and_are_capped() {
        let mut recent = Vec::new();
        for index in 0..MAX_RECENT_PROJECTS + 3 {
            UserSettings::remember_project(&mut recent, PathBuf::from(format!("p{index}")));
        }
        assert_eq!(recent.len(), MAX_RECENT_PROJECTS);
        UserSettings::remember_project(&mut recent, PathBuf::from("p5"));
        assert_eq!(recent[0], PathBuf::from("p5"));
        assert_eq!(
            recent
                .iter()
                .filter(|path| path.as_path() == Path::new("p5"))
                .count(),
            1
        );
    }
}
