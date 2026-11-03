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

    /// Текст копии, если она есть.
    pub fn read(&self, scene: Option<&VirtualPath>) -> Option<String> {
        std::fs::read_to_string(self.file(scene)).ok()
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

        assert_eq!(store.read(Some(&nested)).as_deref(), Some(text.as_str()));
        assert_eq!(store.read(None).as_deref(), Some("untitled"));
        assert_eq!(store.read(Some(&path("/scenes~a.rvescene"))), None);
        assert!(project.0.join(".rve/editor/autosave").is_dir());

        let restored = EditorScene::recover(Some(nested.clone()), &text).unwrap();
        assert!(restored.is_dirty());
        assert_eq!(restored.path(), Some(&nested));
        assert_eq!(restored.document().objects.len(), 3);

        store.remove(Some(&nested)).unwrap();
        store.remove(Some(&nested)).unwrap();
        assert_eq!(store.read(Some(&nested)), None);
        assert_eq!(store.read(None).as_deref(), Some("untitled"));
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
