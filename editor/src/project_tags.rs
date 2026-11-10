//! Черновик тегов и слоёв проекта: правки `[tags]` и `[layers]` в `project.rveproject` до сохранения.
//! Слой — фиксированный слот: очистка имени оставляет индекс занятым, поэтому `layer` объектов
//! не сдвигается.

use engine_project::{MAX_LAYERS, ProjectSettings};
use std::path::{Path, PathBuf};

const PROJECT_FILE: &str = "project.rveproject";

/// Несохранённые списки тегов и слоёв открытого проекта.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagsLayersDraft {
    root: PathBuf,
    tags: Vec<String>,
    layers: Vec<String>,
    saved_tags: Vec<String>,
    saved_layers: Vec<String>,
    masks: [u32; MAX_LAYERS],
    saved_masks: [u32; MAX_LAYERS],
    /// Причина последнего неудачного сохранения; сбрасывается при следующем сохранении или откате.
    pub error: Option<String>,
}

impl TagsLayersDraft {
    /// Черновик с текущими списками проекта `root`.
    pub fn open(root: &Path, settings: &ProjectSettings) -> Self {
        Self {
            root: root.to_path_buf(),
            tags: settings.tags.names.clone(),
            layers: settings.layers.names.clone(),
            saved_tags: settings.tags.names.clone(),
            saved_layers: settings.layers.names.clone(),
            masks: settings.physics.collision_matrix.masks,
            saved_masks: settings.physics.collision_matrix.masks,
            error: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tags_mut(&mut self) -> &mut Vec<String> {
        &mut self.tags
    }

    pub fn layers_mut(&mut self) -> &mut Vec<String> {
        &mut self.layers
    }

    pub fn layers(&self) -> &[String] {
        &self.layers
    }

    /// Сталкиваются ли слои `first` и `second`: обе маски должны разрешать пару.
    pub fn pair_enabled(&self, first: usize, second: usize) -> bool {
        (self.masks[first] >> second) & 1 != 0 && (self.masks[second] >> first) & 1 != 0
    }

    /// Разрешает или запрещает пару симметрично, чтобы обе маски оставались согласованными.
    pub fn set_pair(&mut self, first: usize, second: usize, enabled: bool) {
        for (row, column) in [(first, second), (second, first)] {
            if enabled {
                self.masks[row] |= 1 << column;
            } else {
                self.masks[row] &= !(1 << column);
            }
        }
    }

    /// Удаляет тег из списка и возвращает его имя. Объекты сцены с этим тегом сохраняют значение,
    /// но тег больше не записан в `[tags]`.
    pub fn remove_tag(&mut self, index: usize) -> Option<String> {
        (index < self.tags.len()).then(|| self.tags.remove(index))
    }

    /// Очищает имя слоя и не трогает остальные слоты: их индексы остаются прежними.
    pub fn clear_layer(&mut self, index: usize) {
        if let Some(name) = self.layers.get_mut(index) {
            name.clear();
        }
    }

    /// Можно ли добавить слой: проект допускает не больше [`MAX_LAYERS`] имён.
    pub fn can_add_layer(&self) -> bool {
        self.layers.len() < MAX_LAYERS
    }

    pub fn is_dirty(&self) -> bool {
        self.tags != self.saved_tags
            || self.layers != self.saved_layers
            || self.masks != self.saved_masks
    }

    /// Возвращает списки и матрицу к сохранённым значениям.
    pub fn revert(&mut self) {
        self.tags = self.saved_tags.clone();
        self.layers = self.saved_layers.clone();
        self.masks = self.saved_masks;
        self.error = None;
    }

    /// Проверяет черновик и записывает `project.rveproject` через временный файл. Если файл
    /// расходится с `settings` (правка вне редактора), запись отменяется.
    /// Возвращает новые настройки проекта. Файл пишется заново, поэтому комментарии не сохраняются.
    pub fn save(&mut self, settings: &ProjectSettings) -> Result<ProjectSettings, String> {
        let mut next = settings.clone();
        next.tags.names = self.tags.clone();
        next.layers.names = self.layers.clone();
        next.physics.collision_matrix.masks = self.masks;
        let text = next
            .to_toml()
            .inspect_err(|error| self.error = Some(error.clone()))?;
        let path = self.root.join(PROJECT_FILE);
        self.ensure_unchanged(&path, settings)
            .inspect_err(|error| self.error = Some(error.clone()))?;
        write_atomically(&path, &text).map_err(|error| {
            let message = format!("{PROJECT_FILE}: {error}");
            self.error = Some(message.clone());
            message
        })?;
        self.saved_tags = self.tags.clone();
        self.saved_layers = self.layers.clone();
        self.saved_masks = self.masks;
        self.error = None;
        Ok(next)
    }

    /// Файл на диске разбирается в те же настройки, что у редактора. Расхождение означает правку
    /// вне редактора, в том числе сделанную до открытия вкладки.
    fn ensure_unchanged(&self, path: &Path, settings: &ProjectSettings) -> Result<(), String> {
        if ProjectSettings::load(path).as_ref() == Ok(settings) {
            return Ok(());
        }
        Err(format!(
            "{PROJECT_FILE} изменён вне редактора после открытия проекта; сохранение отменено, откройте проект заново"
        ))
    }
}

/// Пишет `text` во временный файл рядом с `path` и переименовывает его поверх `path`: при сбое
/// записи исходный файл остаётся целым.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempProject(PathBuf);

    impl TempProject {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("rve-project-tags-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const PROJECT: &str = "id = \"demo\"\nname = \"Demo\"\nversion = \"1.0.0\"\n[[scenes]]\npath = \"scene.rvescene\"\n";

    fn settings() -> ProjectSettings {
        ProjectSettings::parse(PROJECT).unwrap()
    }

    #[test]
    fn saving_writes_the_lists_and_reopening_reads_them_back() {
        let temp = TempProject::new("save");
        std::fs::write(temp.0.join("project.rveproject"), PROJECT).unwrap();
        let mut draft = TagsLayersDraft::open(&temp.0, &settings());
        draft.tags_mut().push("Player".to_owned());
        draft.layers_mut().push("UI".to_owned());
        assert!(draft.is_dirty());

        let saved = draft.save(&settings()).unwrap();
        assert!(!draft.is_dirty());
        assert_eq!(saved.tags.names, ["Player"]);
        let reloaded = ProjectSettings::load(temp.0.join("project.rveproject")).unwrap();
        assert_eq!(reloaded.tags.names, ["Player"]);
        assert_eq!(reloaded.layers.names, ["UI"]);
    }

    #[test]
    fn an_invalid_name_is_reported_and_leaves_the_file_untouched() {
        let temp = TempProject::new("invalid");
        std::fs::write(temp.0.join("project.rveproject"), PROJECT).unwrap();
        let mut draft = TagsLayersDraft::open(&temp.0, &settings());
        draft.tags_mut().push("bad name".to_owned());

        let error = draft.save(&settings()).unwrap_err();
        assert!(error.contains("[tags]"), "{error}");
        assert_eq!(draft.error.as_deref(), Some(error.as_str()));
        assert!(draft.is_dirty());
        let text = std::fs::read_to_string(temp.0.join("project.rveproject")).unwrap();
        assert_eq!(text, PROJECT);
    }

    #[test]
    fn layers_stop_growing_at_the_project_limit_and_revert_restores_saved_lists() {
        let mut draft = TagsLayersDraft::open(Path::new("."), &settings());
        for index in 0..MAX_LAYERS {
            assert!(draft.can_add_layer(), "{index}");
            draft.layers_mut().push(format!("layer{index}"));
        }
        assert!(!draft.can_add_layer());

        draft.revert();
        assert!(draft.layers_mut().is_empty());
        assert!(!draft.is_dirty());
    }

    #[test]
    fn an_external_edit_after_open_is_not_overwritten() {
        let temp = TempProject::new("external");
        let path = temp.0.join(PROJECT_FILE);
        std::fs::write(&path, PROJECT).unwrap();
        let mut draft = TagsLayersDraft::open(&temp.0, &settings());
        draft.tags_mut().push("Player".to_owned());
        let edited = PROJECT.replace("name = \"Demo\"", "name = \"Edited\"");
        std::fs::write(&path, &edited).unwrap();

        let error = draft.save(&settings()).unwrap_err();
        assert!(error.contains("изменён вне редактора"), "{error}");
        assert_eq!(draft.error.as_deref(), Some(error.as_str()));
        assert!(draft.is_dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    }

    #[test]
    fn repeated_saves_are_accepted_and_leave_no_temporary_file() {
        let temp = TempProject::new("repeat");
        let path = temp.0.join(PROJECT_FILE);
        std::fs::write(&path, PROJECT).unwrap();
        let mut draft = TagsLayersDraft::open(&temp.0, &settings());
        draft.tags_mut().push("Player".to_owned());
        let saved = draft.save(&settings()).unwrap();
        draft.tags_mut().push("Enemy".to_owned());
        draft.save(&saved).unwrap();

        let reloaded = ProjectSettings::load(&path).unwrap();
        assert_eq!(reloaded.tags.names, ["Player", "Enemy"]);
        assert!(!temp.0.join(format!("{PROJECT_FILE}.tmp")).exists());
    }

    #[test]
    fn a_change_made_before_the_draft_was_opened_is_refused() {
        let temp = TempProject::new("before-open");
        let path = temp.0.join(PROJECT_FILE);
        let loaded = settings();
        let edited = PROJECT.replace("name = \"Demo\"", "name = \"Edited\"");
        std::fs::write(&path, &edited).unwrap();
        let mut draft = TagsLayersDraft::open(&temp.0, &loaded);
        draft.tags_mut().push("Player".to_owned());

        let error = draft.save(&loaded).unwrap_err();
        assert!(error.contains("изменён вне редактора"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    }

    #[test]
    fn cleared_layer_keeps_the_slots_of_the_other_layers() {
        let temp = TempProject::new("slots");
        let path = temp.0.join(PROJECT_FILE);
        std::fs::write(&path, PROJECT).unwrap();
        let mut draft = TagsLayersDraft::open(&temp.0, &settings());
        draft
            .layers_mut()
            .extend(["Default", "UI", "FX"].map(str::to_owned));

        draft.clear_layer(1);
        assert_eq!(draft.layers_mut().clone(), ["Default", "", "FX"]);
        draft.save(&settings()).unwrap();
        let reloaded = ProjectSettings::load(&path).unwrap();
        assert_eq!(reloaded.layers.names, ["Default", "", "FX"]);
    }

    #[test]
    fn removing_a_tag_returns_its_name_and_out_of_range_is_ignored() {
        let mut draft = TagsLayersDraft::open(Path::new("."), &settings());
        draft
            .tags_mut()
            .extend(["Player", "Enemy"].map(str::to_owned));

        assert_eq!(draft.remove_tag(0).as_deref(), Some("Player"));
        assert_eq!(draft.tags_mut().clone(), ["Enemy"]);
        assert_eq!(draft.remove_tag(5), None);
    }

    #[test]
    fn collision_matrix_pairs_are_symmetric_saved_and_revertible() {
        let temp = TempProject::new("matrix");
        std::fs::write(temp.0.join("project.rveproject"), PROJECT).unwrap();
        let mut draft = TagsLayersDraft::open(&temp.0, &settings());
        assert!(draft.pair_enabled(0, 1), "default is all pairs");
        assert!(!draft.is_dirty());

        draft.set_pair(0, 1, false);
        assert!(!draft.pair_enabled(0, 1));
        assert!(
            !draft.pair_enabled(1, 0),
            "the pair is cleared on both sides"
        );
        assert!(draft.is_dirty());

        draft.revert();
        assert!(draft.pair_enabled(0, 1));
        assert!(!draft.is_dirty());

        draft.set_pair(2, 3, false);
        let saved = draft.save(&settings()).unwrap();
        let reloaded = ProjectSettings::load(temp.0.join("project.rveproject")).unwrap();
        assert_eq!(
            reloaded.physics.collision_matrix,
            saved.physics.collision_matrix
        );
        assert_ne!(reloaded.physics.collision_matrix.masks[2], u32::MAX);
        assert_eq!(reloaded.physics.collision_matrix.masks[2] & (1 << 3), 0);
        assert_eq!(reloaded.physics.collision_matrix.masks[3] & (1 << 2), 0);
    }
}
