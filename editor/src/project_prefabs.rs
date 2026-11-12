//! Список тяжёлых префабов проекта `prefabs.rveprefabs`: правка в редакторе и явное сохранение.

use crate::asset_refs::PathRemap;
use crate::scene_edit::write_atomic;
use engine_project::{PREFAB_CATALOG_FILE_NAME, PrefabCatalogEntry, PrefabCatalogFile};
use std::path::{Path, PathBuf};

/// Редактируемая копия списка. Каждая правка проверяется тем же разбором, что и файл при Play,
/// поэтому принятый список всегда можно сохранить. Undo у списка нет: изменения попадают на
/// диск только по [`ProjectPrefabs::save`].
pub struct ProjectPrefabs {
    root: PathBuf,
    path: PathBuf,
    file: PrefabCatalogFile,
    /// Содержимое, которое сейчас лежит на диске; по нему виден признак несохранённых правок.
    saved: PrefabCatalogFile,
    /// Файл прочитан без ошибок. Иначе список пуст и не записывается, чтобы не затереть файл.
    loaded: bool,
    /// Причина последней отклонённой правки или ошибки чтения.
    error: Option<String>,
}

impl ProjectPrefabs {
    /// Читает `<root>/prefabs.rveprefabs`; отсутствующий файл означает пустой список.
    pub fn open(root: &Path) -> Self {
        let path = root.join(PREFAB_CATALOG_FILE_NAME);
        let (file, loaded, error) = match PrefabCatalogFile::load(&path) {
            Ok(file) => (file, true, None),
            Err(error) => (PrefabCatalogFile::default(), false, Some(error)),
        };
        Self {
            root: root.to_path_buf(),
            path,
            saved: file.clone(),
            file,
            loaded,
            error,
        }
    }

    /// Корень проекта, для которого открыт список.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entries(&self) -> &[PrefabCatalogEntry] {
        &self.file.prefabs
    }

    pub fn is_dirty(&self) -> bool {
        self.loaded && self.file != self.saved
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Добавляет префаб по пути относительно `Assets`, например `prefabs/house.rveprefab`.
    pub fn add(&mut self, asset_path: &str) -> Result<(), String> {
        let path = format!("/{}", asset_path.trim_start_matches('/'));
        self.apply(|file| {
            file.prefabs.push(PrefabCatalogEntry {
                path,
                archive: None,
                max_instances: None,
            });
            Ok(())
        })
    }

    pub fn remove(&mut self, index: usize) -> Result<(), String> {
        self.apply(|file| {
            entry_index(file, index)?;
            file.prefabs.remove(index);
            Ok(())
        })
    }

    pub fn set_archive(&mut self, index: usize, archive: Option<String>) -> Result<(), String> {
        self.apply(|file| {
            let entry = entry_index(file, index)?;
            file.prefabs[entry].archive = archive;
            Ok(())
        })
    }

    pub fn set_max_instances(
        &mut self,
        index: usize,
        max_instances: Option<u32>,
    ) -> Result<(), String> {
        self.apply(|file| {
            let entry = entry_index(file, index)?;
            file.prefabs[entry].max_instances = max_instances;
            Ok(())
        })
    }

    /// Число записей списка в памяти, указывающих на переименовываемый или удаляемый ресурс.
    pub fn count_references(&self, remap: &PathRemap) -> usize {
        self.file
            .prefabs
            .iter()
            .filter(|entry| remap.map(&entry.path).is_some())
            .count()
    }

    /// Переводит пути списка на переименованный ресурс. `on_disk` — файл уже переписан тем же
    /// переименованием, поэтому меняется и сохранённая копия: признак несохранённых правок
    /// остаётся прежним.
    pub fn remap_paths(&mut self, remap: &PathRemap, on_disk: bool) {
        let remap_file = |file: &mut PrefabCatalogFile| {
            for entry in &mut file.prefabs {
                if let Some(mapped) = remap.map(&entry.path) {
                    entry.path = mapped;
                }
            }
        };
        remap_file(&mut self.file);
        if on_disk {
            remap_file(&mut self.saved);
        }
    }

    /// Записывает список в `prefabs.rveprefabs` атомарно.
    pub fn save(&mut self) -> Result<(), String> {
        let result = self.ensure_loaded().and_then(|()| {
            let text = self.file.to_toml()?;
            write_atomic(&self.path, text.as_bytes())
        });
        match result {
            Ok(()) => {
                self.saved = self.file.clone();
                self.error = None;
                Ok(())
            }
            Err(error) => {
                self.error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn apply(
        &mut self,
        change: impl FnOnce(&mut PrefabCatalogFile) -> Result<(), String>,
    ) -> Result<(), String> {
        self.ensure_loaded()?;
        let mut next = self.file.clone();
        let result = change(&mut next).and_then(|()| next.to_toml().map(drop));
        match result {
            Ok(()) => {
                self.file = next;
                self.error = None;
                Ok(())
            }
            Err(error) => {
                self.error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn ensure_loaded(&self) -> Result<(), String> {
        if self.loaded {
            Ok(())
        } else {
            Err(format!(
                "{PREFAB_CATALOG_FILE_NAME} не загружен: исправьте файл и перечитайте список"
            ))
        }
    }
}

fn entry_index(file: &PrefabCatalogFile, index: usize) -> Result<usize, String> {
    if index < file.prefabs.len() {
        Ok(index)
    } else {
        Err(format!("записи {index} нет в {PREFAB_CATALOG_FILE_NAME}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Каталог проекта без `Assets`: список читается и пишется напрямую.
    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rve-editor-project-prefabs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, text: &str) {
            fs::write(self.0.join(PREFAB_CATALOG_FILE_NAME), text).unwrap();
        }

        fn read(&self) -> String {
            fs::read_to_string(self.0.join(PREFAB_CATALOG_FILE_NAME)).unwrap()
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn paths(prefabs: &ProjectPrefabs) -> Vec<&str> {
        prefabs
            .entries()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect()
    }

    #[test]
    fn missing_file_opens_as_an_empty_clean_list() {
        let project = TempProject::new();
        let prefabs = ProjectPrefabs::open(&project.0);
        assert!(prefabs.entries().is_empty());
        assert!(prefabs.error().is_none());
        assert!(!prefabs.is_dirty());
    }

    #[test]
    fn entries_are_added_edited_and_removed_with_validation() {
        let project = TempProject::new();
        let mut prefabs = ProjectPrefabs::open(&project.0);
        prefabs.add("prefabs/house.rveprefab").unwrap();
        prefabs.add("/prefabs/tower.rveprefab").unwrap();
        assert_eq!(
            paths(&prefabs),
            ["/prefabs/house.rveprefab", "/prefabs/tower.rveprefab"]
        );
        assert!(prefabs.is_dirty());

        let duplicate = prefabs.add("prefabs/house.rveprefab").unwrap_err();
        assert!(duplicate.contains("указан повторно"), "{duplicate}");
        assert_eq!(prefabs.error(), Some(duplicate.as_str()));
        assert!(prefabs.add("models/a.fbx").is_err());
        assert_eq!(prefabs.entries().len(), 2);

        prefabs
            .set_archive(0, Some("buildings".to_owned()))
            .unwrap();
        prefabs.set_max_instances(1, Some(3)).unwrap();
        assert!(prefabs.set_archive(0, Some("base".to_owned())).is_err());
        assert!(prefabs.set_archive(0, Some("bad name".to_owned())).is_err());
        assert!(prefabs.set_max_instances(1, Some(0)).is_err());
        assert!(prefabs.set_archive(5, None).is_err());
        assert_eq!(prefabs.entries()[0].archive.as_deref(), Some("buildings"));
        assert_eq!(prefabs.entries()[1].max_instances, Some(3));

        prefabs.remove(0).unwrap();
        assert_eq!(paths(&prefabs), ["/prefabs/tower.rveprefab"]);
        assert!(prefabs.error().is_none());
    }

    #[test]
    fn saved_list_reopens_with_the_same_entries_and_is_clean() {
        let project = TempProject::new();
        let mut prefabs = ProjectPrefabs::open(&project.0);
        prefabs.add("prefabs/house.rveprefab").unwrap();
        prefabs
            .set_archive(0, Some("buildings".to_owned()))
            .unwrap();
        prefabs.set_max_instances(0, Some(4)).unwrap();
        assert!(prefabs.is_dirty());
        prefabs.save().unwrap();
        assert!(!prefabs.is_dirty());

        let reopened = ProjectPrefabs::open(&project.0);
        assert_eq!(reopened.entries(), prefabs.entries());
        assert!(!reopened.is_dirty());
        let parsed = PrefabCatalogFile::parse(&project.read()).unwrap();
        assert_eq!(parsed.prefabs.as_slice(), reopened.entries());
    }

    #[test]
    fn broken_file_is_reported_and_never_overwritten() {
        let project = TempProject::new();
        let broken = "[[prefabs]]\npath = \"/models/a.fbx\"\n";
        project.write(broken);

        let mut prefabs = ProjectPrefabs::open(&project.0);
        let error = prefabs.error().unwrap().to_owned();
        assert!(error.contains(PREFAB_CATALOG_FILE_NAME), "{error}");
        assert!(prefabs.entries().is_empty());
        assert!(prefabs.add("prefabs/house.rveprefab").is_err());
        assert!(prefabs.save().is_err());
        assert!(!prefabs.is_dirty());
        assert_eq!(project.read(), broken);
    }

    #[test]
    fn remap_follows_a_renamed_folder_and_keeps_the_unsaved_flag() {
        let temp = TempProject::new();
        temp.write("[[prefabs]]\npath = \"/old/a.rveprefab\"\n");
        let mut prefabs = ProjectPrefabs::open(&temp.0);
        let remap = PathRemap::new("old", "new");
        assert_eq!(prefabs.count_references(&remap), 1);
        prefabs.remap_paths(&remap, true);
        assert_eq!(paths(&prefabs), ["/new/a.rveprefab"]);
        assert!(!prefabs.is_dirty(), "файл уже переписан на диске");

        prefabs.add("old/b.rveprefab").unwrap();
        prefabs.remap_paths(&remap, false);
        assert_eq!(paths(&prefabs), ["/new/a.rveprefab", "/new/b.rveprefab"]);
        assert!(prefabs.is_dirty());
    }
}
