//! Состояние смены сцен, общее для хоста и транзакций вызовов.
//!
//! Хост знает объявленные включённые сцены проекта и текущую сцену. Вызов `rve.scene.load` или
//! `ctx.load_scene` проверяет путь по списку и ставит запрос; хост отдаёт его вызывающему коду
//! только после конца кадра. Текущая сцена меняется лишь при успешной замене.

use engine_files::VirtualPath;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Default)]
pub(crate) struct SceneState {
    /// Канонические пути включённых сцен проекта.
    pub(crate) enabled: Vec<String>,
    /// Имя текущей сцены, как его получил хост при загрузке или замене.
    pub(crate) current: String,
    /// Последний принятый запрос на загрузку; ждёт конца кадра.
    pub(crate) request: Option<String>,
    /// Запрос штатного завершения приложения, принятый успешным callback.
    pub(crate) quit_requested: bool,
}

pub(crate) type SharedSceneState = Rc<RefCell<SceneState>>;

/// Канонический путь сцены: ведущий слеш необязателен, как в `[[scenes]]` проекта.
pub(crate) fn canonical_scene_path(path: &str) -> Result<String, String> {
    VirtualPath::parse(path)
        .map(|path| path.as_str().to_owned())
        .map_err(|_| format!("scene path {path:?} is not a valid virtual path"))
}

/// Проверяет, что сцена объявлена и включена в проекте; возвращает канонический путь.
pub(crate) fn check_scene_request(state: &SceneState, path: &str) -> Result<String, String> {
    let canonical = canonical_scene_path(path)?;
    if state.enabled.contains(&canonical) {
        Ok(canonical)
    } else {
        Err(format!(
            "scene {path:?} is not an enabled scene of the project"
        ))
    }
}
