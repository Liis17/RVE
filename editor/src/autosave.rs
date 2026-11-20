//! Автосохранение несохранённой сцены в служебный каталог проекта и её восстановление.
//!
//! Копии лежат в `<проект>/.rve/editor/autosave/`: вне `Assets/`, поэтому не попадают
//! в ресурсы и сборку, а `.rve/` исключён из Git. Копия удаляется после Save и после отказа
//! от изменений; оставшаяся копия означает, что редактор завершился без сохранения.

use crate::scene_edit::write_atomic;
use engine_files::VirtualPath;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Как часто записывать копию изменённой сцены.
pub const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);
/// Имя копии несохранённой сцены; реальный virtual path не начинается с `~`.
const UNTITLED: &str = "~untitled";

/// Нужно ли записать копию: сцена изменена после последней копии и интервал истёк.
pub fn autosave_due(
    dirty: bool,
    revision: u64,
    autosaved_revision: Option<u64>,
    since_last: Duration,
) -> bool {
    dirty && autosaved_revision != Some(revision) && since_last >= AUTOSAVE_INTERVAL
}

/// Исправная копия сцены.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutosaveCopy {
    pub contents: String,
}

/// Найденное для одной сцены: копии и ошибки чтения, которые нельзя молча пропустить.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct AutosaveCopies {
    pub copies: Vec<AutosaveCopy>,
    pub errors: Vec<String>,
}

/// Каталог копий одного проекта.
pub struct AutosaveStore {
    dir: PathBuf,
}

impl AutosaveStore {
    pub fn for_project(root: &Path) -> Self {
        Self {
            dir: root.join(".rve").join("editor").join("autosave"),
        }
    }

    /// Файл копии сцены. `~` и `/` пути экранируются как `~7E` и `~2F`, поэтому разные
    /// сцены не получают одно имя, а `~untitled` не совпадает ни с одним путём.
    fn file(&self, scene: Option<&VirtualPath>) -> PathBuf {
        let name = scene.map_or_else(
            || UNTITLED.to_owned(),
            |path| path.as_str().replace('~', "~7E").replace('/', "~2F"),
        );
        self.dir.join(format!("{name}.autosave"))
    }

    /// Атомарно записывает текст сцены.
    pub fn write(&self, scene: Option<&VirtualPath>, contents: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("{}: {error}", self.dir.display()))?;
        write_atomic(&self.file(scene), contents.as_bytes())
    }

    /// Копии сцены. Отсутствие копии не ошибка; неудачное чтение попадает в `errors` с путём файла.
    pub fn read(&self, scene: Option<&VirtualPath>) -> AutosaveCopies {
        let mut found = AutosaveCopies::default();
        match read_generation(&self.file(scene)) {
            Ok(Some(copy)) => found.copies.push(copy),
            Ok(None) => {}
            Err(error) => found.errors.push(error),
        }
        found
    }

    /// Удаляет копию; отсутствие копии не ошибка.
    pub fn remove(&self, scene: Option<&VirtualPath>) -> Result<(), String> {
        let file = self.file(scene);
        match std::fs::remove_file(&file) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("{}: {error}", file.display())),
        }
    }
}

/// Читает файл копии. Отсутствующий файл — `Ok(None)`; любая другая ошибка — `Err` с путём файла.
fn read_generation(file: &Path) -> Result<Option<AutosaveCopy>, String> {
    match std::fs::read_to_string(file) {
        Ok(contents) => Ok(Some(AutosaveCopy { contents })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", file.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_edit::{EditorScene, NewObject};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            Self(std::env::temp_dir().join(format!(
                "rve-editor-autosave-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    /// Текст самой новой исправной копии.
    fn latest(store: &AutosaveStore, scene: Option<&VirtualPath>) -> Option<String> {
        store
            .read(scene)
            .copies
            .into_iter()
            .next()
            .map(|copy| copy.contents)
    }

    #[test]
    fn copies_are_written_found_and_removed_per_scene() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let mut scene = EditorScene::new_untitled().unwrap();
        scene.create_object(&NewObject::Empty, None).unwrap();
        let text = scene.document().to_toml().unwrap();
        let nested = path("/scenes/a.rvescene");
        store.write(Some(&nested), &text).unwrap();
        store.write(None, "untitled").unwrap();

        assert_eq!(
            latest(&store, Some(&nested)).as_deref(),
            Some(text.as_str())
        );
        assert_eq!(latest(&store, None).as_deref(), Some("untitled"));
        assert_eq!(latest(&store, Some(&path("/scenes~a.rvescene"))), None);
        assert!(project.0.join(".rve/editor/autosave").is_dir());

        let restored = EditorScene::recover(Some(nested.clone()), &text).unwrap();
        assert!(restored.is_dirty());
        assert_eq!(restored.path(), Some(&nested));
        assert_eq!(restored.document().objects.len(), 3);

        store.remove(Some(&nested)).unwrap();
        store.remove(Some(&nested)).unwrap();
        assert_eq!(latest(&store, Some(&nested)), None);
        assert_eq!(latest(&store, None).as_deref(), Some("untitled"));
    }

    #[test]
    fn missing_copy_is_neither_a_copy_nor_an_error() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        assert_eq!(store.read(None), AutosaveCopies::default());
        assert_eq!(
            read_generation(&project.0.join("missing.autosave")),
            Ok(None)
        );
    }

    #[test]
    fn unreadable_copy_is_an_error_with_its_file_path() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let file = store.file(None);

        std::fs::create_dir_all(&file).unwrap();
        let found = store.read(None);
        assert!(found.copies.is_empty());
        assert_eq!(found.errors.len(), 1);
        assert!(found.errors[0].starts_with(&file.display().to_string()));

        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        let found = store.read(None);
        assert!(found.copies.is_empty());
        assert_eq!(found.errors.len(), 1);
        assert!(found.errors[0].starts_with(&file.display().to_string()));
    }

    #[test]
    fn copy_is_due_only_for_new_changes_after_the_interval() {
        let late = AUTOSAVE_INTERVAL;
        let early = AUTOSAVE_INTERVAL - Duration::from_secs(1);
        assert!(autosave_due(true, 3, None, late));
        assert!(autosave_due(true, 3, Some(2), late));
        assert!(!autosave_due(true, 3, Some(3), late));
        assert!(!autosave_due(true, 3, None, early));
        assert!(!autosave_due(false, 3, None, late));
    }
}
