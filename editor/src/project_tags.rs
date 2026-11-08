//! Черновик тегов и слоёв проекта: правки `[tags]` и `[layers]` в `project.rveproject` до сохранения.

use engine_project::{MAX_LAYERS, ProjectSettings};
use std::path::{Path, PathBuf};

/// Несохранённые списки тегов и слоёв открытого проекта.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagsLayersDraft {
    root: PathBuf,
    tags: Vec<String>,
    layers: Vec<String>,
    saved_tags: Vec<String>,
    saved_layers: Vec<String>,
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

    /// Можно ли добавить слой: проект допускает не больше [`MAX_LAYERS`] имён.
    pub fn can_add_layer(&self) -> bool {
        self.layers.len() < MAX_LAYERS
    }

    pub fn is_dirty(&self) -> bool {
        self.tags != self.saved_tags || self.layers != self.saved_layers
    }

    /// Возвращает списки к сохранённым значениям.
    pub fn revert(&mut self) {
        self.tags = self.saved_tags.clone();
        self.layers = self.saved_layers.clone();
        self.error = None;
    }

    /// Проверяет черновик и записывает `project.rveproject`. Возвращает новые настройки проекта.
    /// Файл пишется заново, поэтому комментарии исходного файла не сохраняются.
    pub fn save(&mut self, settings: &ProjectSettings) -> Result<ProjectSettings, String> {
        let mut next = settings.clone();
        next.tags.names = self.tags.clone();
        next.layers.names = self.layers.clone();
        let text = next
            .to_toml()
            .inspect_err(|error| self.error = Some(error.clone()))?;
        std::fs::write(self.root.join("project.rveproject"), text).map_err(|error| {
            let message = format!("project.rveproject: {error}");
            self.error = Some(message.clone());
            message
        })?;
        self.saved_tags = self.tags.clone();
        self.saved_layers = self.layers.clone();
        self.error = None;
        Ok(next)
    }
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
}
