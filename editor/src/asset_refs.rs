//! Ссылки сцен, префабов и `prefabs.rveprefabs` на файлы ресурсов: поиск перед переименованием
//! или удалением в панели Project и переписывание после переименования.
//!
//! Ссылкой считается любое строковое значение TOML, равное virtual path файла (`/a/b.fbx`) или
//! начинающееся с пути папки и `/`. Файлы переписываются через `toml_edit`, поэтому меняются
//! только сами строки, а комментарии и форматирование сохраняются.

use crate::assets_fs::{AssetEntry, AssetKind, AssetsDir};
use crate::scene_edit::write_atomic;
use engine_project::{PREFAB_CATALOG_FILE_NAME, PrefabCatalogFile};
use engine_scene::SceneDocument;
use std::path::Path;
use toml_edit::visit_mut::VisitMut;

/// Переименование ресурса: пути от корня `Assets/` без ведущей черты, как в [`AssetEntry`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathRemap {
    old: String,
    new: String,
}

impl PathRemap {
    pub fn new(old: &str, new: &str) -> Self {
        Self {
            old: format!("/{}", old.trim_start_matches('/')),
            new: format!("/{}", new.trim_start_matches('/')),
        }
    }

    /// Новый virtual path для `value`, если он указывает на переименованный файл или внутрь папки.
    pub fn map(&self, value: &str) -> Option<String> {
        if value == self.old {
            return Some(self.new.clone());
        }
        let rest = value.strip_prefix(&self.old)?.strip_prefix('/')?;
        Some(format!("{}/{rest}", self.new))
    }

    /// Как [`Self::map`], но для пути от корня `Assets/` без ведущей черты.
    pub fn map_asset_path(&self, path: &str) -> Option<String> {
        self.map(&format!("/{path}"))
            .map(|mapped| mapped.trim_start_matches('/').to_owned())
    }
}

/// Файл, который ссылается на ресурс.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReferenceSource {
    /// Сцена или префаб, путь от корня `Assets/`.
    Asset(String),
    /// `prefabs.rveprefabs` в корне проекта.
    ProjectPrefabs,
}

impl std::fmt::Display for ReferenceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Asset(path) => write!(f, "Assets/{path}"),
            Self::ProjectPrefabs => f.write_str(PREFAB_CATALOG_FILE_NAME),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reference {
    pub source: ReferenceSource,
    /// Число строк-ссылок в файле.
    pub count: usize,
}

/// Считает ссылки на `remap.old` в тексте TOML.
pub fn count_in_text(text: &str, remap: &PathRemap) -> Result<usize, String> {
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| error.to_string())?;
    let mut visitor = Rewrite {
        remap,
        replace: false,
        count: 0,
    };
    visitor.visit_document_mut(&mut document);
    Ok(visitor.count)
}

/// Переписывает ссылки в тексте TOML; `None` — ссылок нет и текст не меняется.
pub fn rewrite_text(text: &str, remap: &PathRemap) -> Result<Option<String>, String> {
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| error.to_string())?;
    let mut visitor = Rewrite {
        remap,
        replace: true,
        count: 0,
    };
    visitor.visit_document_mut(&mut document);
    Ok((visitor.count > 0).then(|| document.to_string()))
}

struct Rewrite<'a> {
    remap: &'a PathRemap,
    replace: bool,
    count: usize,
}

impl VisitMut for Rewrite<'_> {
    fn visit_string_mut(&mut self, node: &mut toml_edit::Formatted<String>) {
        if let Some(mapped) = self.remap.map(node.value()) {
            self.count += 1;
            if self.replace {
                let decor = node.decor().clone();
                *node = toml_edit::Formatted::new(mapped);
                *node.decor_mut() = decor;
            }
        }
    }
}

/// Все сцены и префабы в `Assets/` и `prefabs.rveprefabs`, ссылающиеся на `remap.old`.
/// Нечитаемые файлы попадают в `errors` и не считаются ссылками.
pub fn scan(project_root: &Path, assets: &AssetsDir, remap: &PathRemap) -> ScanResult {
    let mut result = ScanResult::default();
    let mut files = Vec::new();
    match assets.list() {
        Ok(entries) => collect_documents(&entries, &mut files),
        Err(error) => result.errors.push(format!("Assets: {error}")),
    }
    let mut sources: Vec<(ReferenceSource, std::path::PathBuf)> = Vec::new();
    for path in files {
        match assets.resolve(&path) {
            Ok(file) => sources.push((ReferenceSource::Asset(path), file)),
            Err(error) => result.errors.push(format!("{path}: {error}")),
        }
    }
    let catalog = project_root.join(PREFAB_CATALOG_FILE_NAME);
    if catalog.is_file() {
        sources.push((ReferenceSource::ProjectPrefabs, catalog));
    }
    for (source, file) in sources {
        let counted = std::fs::read_to_string(&file)
            .map_err(|error| error.to_string())
            .and_then(|text| count_in_text(&text, remap));
        match counted {
            Ok(0) => {}
            Ok(count) => result.references.push(Reference { source, count }),
            Err(error) => result.errors.push(format!("{source}: {error}")),
        }
    }
    result
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScanResult {
    pub references: Vec<Reference>,
    pub errors: Vec<String>,
}

fn collect_documents(entries: &[AssetEntry], files: &mut Vec<String>) {
    for entry in entries {
        if entry.is_dir {
            collect_documents(&entry.children, files);
        } else if matches!(
            AssetKind::of(&entry.path),
            AssetKind::Scene | AssetKind::Prefab
        ) {
            files.push(entry.path.clone());
        }
    }
}

/// Переписывает ссылки в найденных до переименования файлах. Вызывается после переименования,
/// поэтому файл внутри переименованной папки ищется по новому пути. Результат проверяется тем же
/// разбором, что и при загрузке; файл, не прошедший проверку, не меняется.
/// Возвращает переписанные файлы и ошибки.
pub fn rewrite_files(
    project_root: &Path,
    assets: &AssetsDir,
    references: &[Reference],
    remap: &PathRemap,
) -> (Vec<ReferenceSource>, Vec<String>) {
    let mut written = Vec::new();
    let mut errors = Vec::new();
    for reference in references {
        let source = match &reference.source {
            ReferenceSource::Asset(path) => {
                ReferenceSource::Asset(remap.map_asset_path(path).unwrap_or_else(|| path.clone()))
            }
            ReferenceSource::ProjectPrefabs => ReferenceSource::ProjectPrefabs,
        };
        let file = match &source {
            ReferenceSource::Asset(path) => assets.resolve(path).map_err(|error| error.to_string()),
            ReferenceSource::ProjectPrefabs => Ok(project_root.join(PREFAB_CATALOG_FILE_NAME)),
        };
        let result = file.and_then(|file| {
            let text = std::fs::read_to_string(&file).map_err(|error| error.to_string())?;
            let Some(rewritten) = rewrite_text(&text, remap)? else {
                return Ok(false);
            };
            validate(&source, &rewritten)?;
            write_atomic(&file, rewritten.as_bytes())?;
            Ok(true)
        });
        match result {
            Ok(true) => written.push(source),
            Ok(false) => {}
            Err(error) => errors.push(format!("{source}: {error}")),
        }
    }
    (written, errors)
}

fn validate(source: &ReferenceSource, text: &str) -> Result<(), String> {
    match source {
        ReferenceSource::Asset(path) if AssetKind::of(path) == AssetKind::Prefab => {
            SceneDocument::parse_prefab(path, text)
                .map(drop)
                .map_err(|error| error.to_string())
        }
        ReferenceSource::Asset(path) => SceneDocument::parse(path, text)
            .map(drop)
            .map_err(|error| error.to_string()),
        ReferenceSource::ProjectPrefabs => PrefabCatalogFile::parse(text).map(drop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn remap_matches_the_file_and_paths_inside_a_folder_only() {
        let file = PathRemap::new("models/a.fbx", "models/b.fbx");
        assert_eq!(file.map("/models/a.fbx").as_deref(), Some("/models/b.fbx"));
        assert_eq!(file.map("/models/a.fbx.meta"), None);
        assert_eq!(file.map("/models/a.fbxx"), None);

        let folder = PathRemap::new("models", "meshes");
        assert_eq!(
            folder.map("/models/sub/a.fbx").as_deref(),
            Some("/meshes/sub/a.fbx")
        );
        assert_eq!(folder.map("/models2/a.fbx"), None);
        assert_eq!(folder.map("models/a.fbx"), None);
        assert_eq!(
            folder.map_asset_path("models/x.rvescene").as_deref(),
            Some("meshes/x.rvescene")
        );
    }

    #[test]
    fn rewrite_keeps_comments_and_formatting_and_touches_only_matching_strings() {
        let text = "# Заголовок\n[[assets]]\npath = \"/models/a.fbx\" # модель\n\n\
                    [[objects]]\nid = \"cube\"\n[objects.mesh_renderer]\nmodel   =   '/models/a.fbx'\n\
                    [objects.extra]\nlist = [\"/models/a.fbx\", \"/other.png\"]\nname = \"/models/a.fbx2\"\n";
        let remap = PathRemap::new("models/a.fbx", "models/b.fbx");
        assert_eq!(count_in_text(text, &remap).unwrap(), 3);
        let rewritten = rewrite_text(text, &remap).unwrap().unwrap();
        assert_eq!(
            rewritten,
            "# Заголовок\n[[assets]]\npath = \"/models/b.fbx\" # модель\n\n\
             [[objects]]\nid = \"cube\"\n[objects.mesh_renderer]\nmodel   =   \"/models/b.fbx\"\n\
             [objects.extra]\nlist = [\"/models/b.fbx\", \"/other.png\"]\nname = \"/models/a.fbx2\"\n"
        );
        assert_eq!(
            rewrite_text("a = \"/x.png\"\n", &remap).unwrap(),
            None,
            "без ссылок текст не переписывается"
        );
        assert!(count_in_text("a = [", &remap).is_err());
    }

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "rve-editor-refs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("Assets")).unwrap();
            Self(root)
        }

        fn assets(&self) -> AssetsDir {
            AssetsDir::new(self.0.join("Assets"))
        }

        fn write(&self, path: &str, contents: &str) {
            let file = self.0.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, contents).unwrap();
        }

        fn read(&self, path: &str) -> String {
            fs::read_to_string(self.0.join(path)).unwrap()
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const SCENE: &str = "# сцена\n[[assets]]\npath = \"/prefabs/p.rveprefab\"\n\n\
                         [[instances]]\nid = \"p\"\nprefab = \"/prefabs/p.rveprefab\"\n";

    #[test]
    fn folder_rename_rewrites_scenes_prefabs_and_catalog_including_moved_documents() {
        let project = TempProject::new();
        let assets = project.assets();
        project.write("Assets/scenes/main.rvescene", SCENE);
        project.write("Assets/prefabs/p.rveprefab", "[[objects]]\nid = \"root\"\n");
        project.write(
            "Assets/prefabs/q.rveprefab",
            "[[assets]]\npath = \"/prefabs/p.rveprefab\"\n\n\
             [[instances]]\nid = \"inner\"\nprefab = \"/prefabs/p.rveprefab\"\n",
        );
        project.write("Assets/scenes/other.rvescene", "[[objects]]\nid = \"a\"\n");
        project.write(
            "prefabs.rveprefabs",
            "# каталог\n[[prefabs]]\npath = \"/prefabs/q.rveprefab\"\n",
        );
        project.write("Assets/broken.rvescene", "objects = [");

        let remap = PathRemap::new("prefabs", "heavy");
        let scan = scan(&project.0, &assets, &remap);
        let mut sources = scan
            .references
            .iter()
            .map(|reference| (reference.source.to_string(), reference.count))
            .collect::<Vec<_>>();
        sources.sort();
        assert_eq!(
            sources,
            [
                ("Assets/prefabs/q.rveprefab".to_owned(), 2),
                ("Assets/scenes/main.rvescene".to_owned(), 2),
                ("prefabs.rveprefabs".to_owned(), 1),
            ]
        );
        assert_eq!(scan.errors.len(), 1, "{:?}", scan.errors);
        assert!(scan.errors[0].contains("broken.rvescene"));

        assets.rename("prefabs", "heavy").unwrap();
        let (written, errors) = rewrite_files(&project.0, &assets, &scan.references, &remap);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(written.len(), 3);
        assert_eq!(
            project.read("Assets/scenes/main.rvescene"),
            SCENE.replace("/prefabs/", "/heavy/")
        );
        assert!(
            project
                .read("Assets/heavy/q.rveprefab")
                .contains("prefab = \"/heavy/p.rveprefab\"")
        );
        assert_eq!(
            project.read("prefabs.rveprefabs"),
            "# каталог\n[[prefabs]]\npath = \"/heavy/q.rveprefab\"\n"
        );
    }

    #[test]
    fn rewrite_that_breaks_validation_leaves_the_file_unchanged() {
        let project = TempProject::new();
        let assets = project.assets();
        let scene = "[[assets]]\npath = \"/models/a.fbx\"\n\n[[objects]]\nid = \"a\"\n\
                     [objects.mesh_renderer]\nmodel = \"/models/a.fbx\"\n";
        project.write("Assets/s.rvescene", scene);
        // Модель, переименованная в PNG, больше не модель: сцена с такой ссылкой невалидна.
        let remap = PathRemap::new("models/a.fbx", "models/a.png");
        let scan = scan(&project.0, &assets, &remap);
        assert_eq!(scan.references.len(), 1);
        let (written, errors) = rewrite_files(&project.0, &assets, &scan.references, &remap);
        assert!(written.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(project.read("Assets/s.rvescene"), scene);
    }
}
