use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

/// Имя файла списка тяжёлых префабов в каталоге проекта.
pub const PREFAB_CATALOG_FILE_NAME: &str = "prefabs.rveprefabs";
/// Путь списка внутри `base` release-сборки. Этот путь зарезервирован: `Assets` его не содержит.
pub const PREFAB_CATALOG_RELEASE_PATH: &str = "/rve/prefabs.toml";

const PREFAB_EXTENSION: &str = "rveprefab";

/// Содержимое `prefabs.rveprefabs`: префабы проекта, которые грузятся с диска во время игры.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefabCatalogFile {
    #[serde(default)]
    pub prefabs: Vec<PrefabCatalogEntry>,
}

/// Тяжёлый префаб проекта.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefabCatalogEntry {
    /// Виртуальный путь `.rveprefab` в `Assets`, например `/prefabs/house.rveprefab`.
    pub path: String,
    /// Явный архив файла префаба и его зависимостей. Без него действуют правила сцены, а если
    /// ассет никакой сцене не нужен, то `shared`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    /// Лимит живых spawn-экземпляров; должен быть больше нуля.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_instances: Option<u32>,
}

impl PrefabCatalogFile {
    /// Разбирает TOML `prefabs.rveprefabs`; ошибки отнесены к имени файла.
    pub fn parse(contents: &str) -> Result<Self, String> {
        Self::parse_labeled(contents, PREFAB_CATALOG_FILE_NAME)
    }

    /// Читает список из `path`. Отсутствующий файл означает пустой список.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(contents) => Self::parse_labeled(&contents, &path.display().to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Сериализует проверенный список в TOML с сохранением порядка записей.
    pub fn to_toml(&self) -> Result<String, String> {
        self.validate()
            .map_err(|error| format!("{PREFAB_CATALOG_FILE_NAME}: {error}"))?;
        toml::to_string(self).map_err(|error| format!("{PREFAB_CATALOG_FILE_NAME}: {error}"))
    }

    fn parse_labeled(contents: &str, label: &str) -> Result<Self, String> {
        let file: Self = toml::from_str(contents).map_err(|error| format!("{label}: {error}"))?;
        file.validate()
            .map_err(|error| format!("{label}: {error}"))?;
        Ok(file)
    }

    fn validate(&self) -> Result<(), String> {
        let mut paths = HashSet::with_capacity(self.prefabs.len());
        for entry in &self.prefabs {
            let path = VirtualPath::parse(&entry.path)
                .map_err(|_| format!("некорректный путь префаба {:?}", entry.path))?;
            let is_prefab = Path::new(path.as_str())
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case(PREFAB_EXTENSION));
            if !is_prefab {
                return Err(format!(
                    "путь префаба {:?} должен оканчиваться на .{PREFAB_EXTENSION}",
                    entry.path
                ));
            }
            if !paths.insert(path) {
                return Err(format!("путь префаба {:?} указан повторно", entry.path));
            }
            if let Some(archive) = &entry.archive
                && !valid_archive_name(archive)
            {
                return Err(format!(
                    "некорректное имя архива {archive:?} префаба {:?}; допустимы 1–64 ASCII-букв, цифр, _ и -, кроме base и shared",
                    entry.path
                ));
            }
            if entry.max_instances == Some(0) {
                return Err(format!(
                    "max_instances префаба {:?} должен быть больше нуля",
                    entry.path
                ));
            }
        }
        Ok(())
    }
}

/// Правило `SceneAsset::archive` из engine-scene: ASCII-имя 1–64 байта, `base` и `shared`
/// запрещены в любом регистре.
fn valid_archive_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        && !["base", "shared"]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

#[cfg(test)]
mod tests {
    use super::{PREFAB_CATALOG_FILE_NAME, PrefabCatalogEntry, PrefabCatalogFile};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

    fn unique_path() -> PathBuf {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rve-prefab-catalog-{}-{id}.rveprefabs",
            std::process::id()
        ))
    }

    fn parse_error(contents: &str) -> String {
        PrefabCatalogFile::parse(contents).unwrap_err()
    }

    #[test]
    fn parses_entries_in_file_order_with_optional_fields() {
        let catalog = PrefabCatalogFile::parse(
            "[[prefabs]]\npath = \"/prefabs/house.rveprefab\"\narchive = \"buildings\"\nmax_instances = 4\n\n[[prefabs]]\npath = \"/prefabs/rock.rveprefab\"\n",
        )
        .unwrap();
        assert_eq!(
            catalog.prefabs,
            [
                PrefabCatalogEntry {
                    path: "/prefabs/house.rveprefab".to_owned(),
                    archive: Some("buildings".to_owned()),
                    max_instances: Some(4),
                },
                PrefabCatalogEntry {
                    path: "/prefabs/rock.rveprefab".to_owned(),
                    archive: None,
                    max_instances: None,
                },
            ]
        );
    }

    #[test]
    fn empty_contents_is_an_empty_list() {
        assert!(PrefabCatalogFile::parse("").unwrap().prefabs.is_empty());
        assert!(
            PrefabCatalogFile::parse("prefabs = []\n")
                .unwrap()
                .prefabs
                .is_empty()
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = parse_error("[[prefabs]]\npath = \"/a.rveprefab\"\npriority = 1\n");
        assert!(error.contains("priority"), "{error}");
        assert!(parse_error("version = 1\n").contains("version"));
    }

    #[test]
    fn rejects_duplicate_paths_even_when_spelled_differently() {
        let error = parse_error(
            "[[prefabs]]\npath = \"/prefabs/a.rveprefab\"\n[[prefabs]]\npath = \"prefabs/a.rveprefab\"\n",
        );
        assert!(error.contains("указан повторно"), "{error}");
    }

    #[test]
    fn requires_a_prefab_extension_case_insensitively() {
        let error = parse_error("[[prefabs]]\npath = \"/scenes/main.rvescene\"\n");
        assert!(error.contains(".rveprefab"), "{error}");
        let catalog = PrefabCatalogFile::parse("[[prefabs]]\npath = \"/prefabs/A.RVEPREFAB\"\n");
        assert!(catalog.is_ok());
    }

    #[test]
    fn rejects_invalid_virtual_paths() {
        let error = parse_error("[[prefabs]]\npath = \"/prefabs/../a.rveprefab\"\n");
        assert!(error.contains("некорректный путь"), "{error}");
    }

    #[test]
    fn rejects_zero_max_instances() {
        let error = parse_error("[[prefabs]]\npath = \"/a.rveprefab\"\nmax_instances = 0\n");
        assert!(error.contains("max_instances"), "{error}");
    }

    #[test]
    fn rejects_archive_names_that_scenes_reject() {
        let too_long = "x".repeat(65);
        for archive in ["", "bad name", "base", "Shared", too_long.as_str(), "тест"] {
            let input = format!("[[prefabs]]\npath = \"/a.rveprefab\"\narchive = {archive:?}\n");
            assert!(
                PrefabCatalogFile::parse(&input).is_err(),
                "accepted archive {archive:?}"
            );
        }
        let accepted = PrefabCatalogFile::parse(
            "[[prefabs]]\npath = \"/a.rveprefab\"\narchive = \"props_2-X\"\n",
        );
        assert!(accepted.is_ok());
    }

    #[test]
    fn serialization_round_trips_and_is_deterministic() {
        let catalog = PrefabCatalogFile {
            prefabs: vec![
                PrefabCatalogEntry {
                    path: "/prefabs/zeta.rveprefab".to_owned(),
                    archive: Some("props".to_owned()),
                    max_instances: Some(2),
                },
                PrefabCatalogEntry {
                    path: "/prefabs/alpha.rveprefab".to_owned(),
                    archive: None,
                    max_instances: None,
                },
            ],
        };
        let text = catalog.to_toml().unwrap();
        let parsed = PrefabCatalogFile::parse(&text).unwrap();
        assert_eq!(parsed, catalog);
        assert_eq!(parsed.to_toml().unwrap(), text);
        assert!(text.find("zeta").unwrap() < text.find("alpha").unwrap());
    }

    #[test]
    fn empty_list_serializes_to_a_parseable_document() {
        let text = PrefabCatalogFile::default().to_toml().unwrap();
        assert!(PrefabCatalogFile::parse(&text).unwrap().prefabs.is_empty());
    }

    #[test]
    fn to_toml_rejects_an_invalid_list_built_in_code() {
        let catalog = PrefabCatalogFile {
            prefabs: vec![PrefabCatalogEntry {
                path: "/prefabs/a.rveprefab".to_owned(),
                archive: None,
                max_instances: Some(0),
            }],
        };
        assert!(catalog.to_toml().unwrap_err().contains("max_instances"));
    }

    #[test]
    fn load_treats_a_missing_file_as_an_empty_list() {
        let path = unique_path();
        assert_eq!(
            PrefabCatalogFile::load(&path).unwrap(),
            PrefabCatalogFile::default()
        );
    }

    #[test]
    fn load_reports_format_errors_with_the_file_path() {
        let path = unique_path();
        fs::write(&path, "[[prefabs]]\npath = \"/a.rvescene\"\n").unwrap();
        let error = PrefabCatalogFile::load(&path).unwrap_err();
        fs::remove_file(&path).unwrap();
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.contains(".rveprefab"), "{error}");
    }

    #[test]
    fn file_name_constant_matches_the_dev_layout() {
        assert_eq!(PREFAB_CATALOG_FILE_NAME, "prefabs.rveprefabs");
    }
}
