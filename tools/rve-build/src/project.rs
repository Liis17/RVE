use crate::error::BuildError;
use engine_assets::{AssetError, Assets};
use engine_files::{VirtualPath, strip_windows_verbatim_prefix};
use engine_project::{BootstrapConfig, ProjectSettings};
use engine_resources::{
    ResourceConfig, ResourceError, ResourceErrorKind, ResourceSystem, SourceMode,
};
use engine_scene::SceneDocument;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub(crate) struct ProjectBuild {
    pub(crate) project_dir: PathBuf,
    pub(crate) bootstrap_path: PathBuf,
    pub(crate) settings: ProjectSettings,
    pub(crate) files: BTreeMap<VirtualPath, PathBuf>,
    pub(crate) assignments: BTreeMap<VirtualPath, String>,
    pub(crate) archives: Vec<String>,
    pub(crate) icon: Option<VirtualPath>,
}

#[derive(Default)]
struct AssetUsage {
    scenes: BTreeSet<usize>,
    explicit_archives: Vec<(usize, String)>,
}

#[derive(Clone)]
struct EnabledScene {
    path: VirtualPath,
    stem: String,
}

pub(crate) fn prepare_project(project_dir: &Path) -> Result<ProjectBuild, BuildError> {
    let project_dir = fs::canonicalize(project_dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BuildError::input(format!(
                "каталог проекта не найден: {}",
                project_dir.display()
            ))
        } else {
            BuildError::operational(format!(
                "не удалось открыть каталог проекта {}: {error}",
                project_dir.display()
            ))
        }
    })?;
    let project_dir = normalize_project_path(&project_dir)?;
    if !project_dir.is_dir() {
        return Err(BuildError::input(format!(
            "каталог проекта не является каталогом: {}",
            project_dir.display()
        )));
    }

    let settings_path = project_dir.join("project.rveproject");
    let settings_source = read_project_text(&settings_path)?;
    let settings = ProjectSettings::parse(&settings_source)
        .map_err(|error| BuildError::input(format!("ошибка project.rveproject: {error}")))?;

    let bootstrap_path = project_dir.join("bootstrap.rves");
    let bootstrap_source = read_project_text(&bootstrap_path)?;
    let bootstrap = BootstrapConfig::parse(&bootstrap_source)
        .map_err(|error| BuildError::input(format!("ошибка bootstrap.rves: {error}")))?;

    let assets_dir = project_dir.join("Assets");
    let mut resources = ResourceSystem::start(ResourceConfig {
        root: assets_dir.clone(),
        source_mode: SourceMode::Loose,
        ..ResourceConfig::default()
    })
    .map_err(|error| resource_error("не удалось открыть Assets", error))?;
    let result = prepare_with_resources(
        project_dir,
        assets_dir,
        bootstrap_path,
        settings,
        bootstrap,
        &mut resources,
    );
    let shutdown = resources.shutdown().map_err(|error| {
        BuildError::operational(format!("не удалось остановить ResourceSystem: {error}"))
    });
    match (result, shutdown) {
        (Err(error), _) => Err(error),
        (Ok(project), Ok(())) => Ok(project),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn prepare_with_resources(
    project_dir: PathBuf,
    assets_dir: PathBuf,
    bootstrap_path: PathBuf,
    settings: ProjectSettings,
    bootstrap: BootstrapConfig,
    resources: &mut ResourceSystem,
) -> Result<ProjectBuild, BuildError> {
    let handle = resources.handle();
    let assets = Assets::with_resources(handle.clone());
    let mut enabled_scenes = Vec::new();
    let mut usages = BTreeMap::<VirtualPath, AssetUsage>::new();
    let mut base_paths = BTreeSet::<VirtualPath>::new();

    for scene in settings.scenes.iter().filter(|scene| scene.enabled) {
        let path = VirtualPath::parse(&scene.path)
            .map_err(|error| BuildError::input(format!("некорректная сцена: {error}")))?;
        let stem = Path::new(path.as_str())
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        require_asset_file(&assets_dir, &path)?;
        let bytes = handle
            .read_all(format!("/{path}"))
            .and_then(|request| request.wait())
            .map_err(|error| resource_error(&format!("сцена /{path}"), error))?;
        let source = String::from_utf8(bytes).map_err(|error| {
            BuildError::input(format!("сцена /{path} должна быть UTF-8: {error}"))
        })?;
        let document = SceneDocument::parse(format!("/{path}"), &source)
            .map_err(|error| BuildError::input(format!("ошибка сцены /{path}: {error}")))?;

        base_paths.insert(path.clone());
        let scene_index = enabled_scenes.len();
        enabled_scenes.push(EnabledScene {
            path: path.clone(),
            stem,
        });

        for asset in document.assets {
            let asset_path = asset.path;
            let usage = usages.entry(asset_path.clone()).or_default();
            usage.scenes.insert(scene_index);
            if let Some(archive) = asset.archive {
                usage.explicit_archives.push((scene_index, archive));
            }
            handle
                .stat(format!("/{asset_path}"))
                .and_then(|request| request.wait())
                .map_err(|error| resource_error(&format!("ресурс /{asset_path}"), error))?;
            require_asset_file(&assets_dir, &asset_path)?;

            if extension_is(asset_path.as_str(), "fbx") {
                let dependencies = assets
                    .model_dependencies(&format!("/{asset_path}"))
                    .map_err(|error| asset_error(&format!("модель /{asset_path}"), error))?;
                for dependency in dependencies {
                    let dependency = VirtualPath::parse(&dependency).map_err(|error| {
                        BuildError::input(format!(
                            "модель /{asset_path} вернула неверную текстуру: {error}"
                        ))
                    })?;
                    handle
                        .stat(format!("/{dependency}"))
                        .and_then(|request| request.wait())
                        .map_err(|error| {
                            resource_error(&format!("текстура /{dependency}"), error)
                        })?;
                    require_asset_file(&assets_dir, &dependency)?;
                    usages
                        .entry(dependency)
                        .or_default()
                        .scenes
                        .insert(scene_index);
                }
            }
        }
    }

    let icon = bootstrap
        .app_icon
        .as_deref()
        .map(VirtualPath::parse)
        .transpose()
        .map_err(|error| BuildError::input(format!("некорректный AppIcon: {error}")))?;
    if let Some(icon_path) = &icon {
        if !extension_is(icon_path.as_str(), "png") {
            return Err(BuildError::input(format!(
                "AppIcon /{icon_path} должен иметь расширение .png"
            )));
        }
        assets
            .load_texture(&format!("/{icon_path}"))
            .map_err(|error| asset_error(&format!("иконка /{icon_path}"), error))?;
        require_asset_file(&assets_dir, icon_path)?;
        base_paths.insert(icon_path.clone());
    }

    let files = usages
        .keys()
        .chain(base_paths.iter())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| Ok((path.clone(), require_asset_file(&assets_dir, path)?)))
        .collect::<Result<BTreeMap<_, _>, BuildError>>()?;
    let assignments = assign_archives(&enabled_scenes, &usages, &base_paths, icon.as_ref())?;
    let archives = archive_order(&assignments);

    Ok(ProjectBuild {
        project_dir,
        bootstrap_path,
        settings,
        files,
        assignments,
        archives,
        icon,
    })
}

fn assign_archives(
    scenes: &[EnabledScene],
    usages: &BTreeMap<VirtualPath, AssetUsage>,
    base_paths: &BTreeSet<VirtualPath>,
    icon: Option<&VirtualPath>,
) -> Result<BTreeMap<VirtualPath, String>, BuildError> {
    let mut assignments = BTreeMap::new();
    for path in usages
        .keys()
        .chain(base_paths.iter())
        .collect::<BTreeSet<_>>()
    {
        let usage = usages.get(path);
        if base_paths.contains(path) {
            if let Some((scene_index, archive)) = usage.and_then(|usage| {
                usage
                    .explicit_archives
                    .iter()
                    .find(|(_, archive)| archive != "base")
            }) {
                let reason = if icon == Some(path) {
                    "AppIcon всегда должен находиться в base"
                } else {
                    "файл включённой сцены всегда должен находиться в base"
                };
                return Err(BuildError::input(format!(
                    "{reason}: /{path} явно назначен в архив {archive:?} сценой /{}",
                    scenes[*scene_index].path
                )));
            }
            assignments.insert((*path).clone(), "base".to_owned());
            continue;
        }

        let usage = usage.expect("non-base paths are present in the usage map");
        if let Some(first) = usage.explicit_archives.first() {
            if let Some(second) = usage
                .explicit_archives
                .iter()
                .skip(1)
                .find(|(_, archive)| archive != &first.1)
            {
                return Err(BuildError::input(format!(
                    "ассет /{path} назначен в разные архивы {:?} (сцена /{}) и {:?} (сцена /{})",
                    first.1, scenes[first.0].path, second.1, scenes[second.0].path
                )));
            }
            assignments.insert((*path).clone(), first.1.clone());
            continue;
        }

        let archive = if usage.scenes.len() == 1 {
            let scene_index = *usage.scenes.first().unwrap();
            let archive = format!("scene-{}", scenes[scene_index].stem);
            validate_scene_archive_name(&archive, &scenes[scene_index].path)?;
            archive
        } else {
            "shared".to_owned()
        };
        assignments.insert((*path).clone(), archive);
    }
    validate_archive_names(assignments.values())?;
    Ok(assignments)
}

fn validate_scene_archive_name(archive: &str, scene: &VirtualPath) -> Result<(), BuildError> {
    if !valid_archive_name(archive) {
        return Err(BuildError::input(format!(
            "имя архива {archive:?}, полученное из имени сцены /{}, недопустимо; используйте ASCII-буквы, цифры, _ или - и длину до 64 символов",
            scene
        )));
    }
    Ok(())
}

fn validate_archive_names<'a>(names: impl Iterator<Item = &'a String>) -> Result<(), BuildError> {
    let mut folded_names = BTreeMap::<String, String>::new();
    for name in names {
        if name != "base" && name != "shared" && !valid_archive_name(name) {
            return Err(BuildError::input(format!(
                "недопустимое имя архива {name:?}; допустимы 1–64 ASCII-букв, цифр, _ и -"
            )));
        }
        let folded = name.to_ascii_lowercase();
        if let Some(previous) = folded_names.get(&folded)
            && previous != name
        {
            return Err(BuildError::input(format!(
                "имена архивов {previous:?} и {name:?} различаются только регистром"
            )));
        }
        folded_names.insert(folded, name.clone());
    }
    Ok(())
}

fn archive_order(assignments: &BTreeMap<VirtualPath, String>) -> Vec<String> {
    let mut names = assignments.values().cloned().collect::<BTreeSet<_>>();
    names.remove("base");
    let has_shared = names.remove("shared");
    let mut archives = vec!["base".to_owned()];
    if has_shared {
        archives.push("shared".to_owned());
    }
    archives.extend(names);
    archives
}

fn valid_archive_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && !is_reserved_windows_name(name)
}

fn is_reserved_windows_name(name: &str) -> bool {
    let device = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            device.strip_prefix(prefix).is_some_and(|number| {
                number.len() == 1 && (b'1'..=b'9').contains(&number.as_bytes()[0])
            })
        })
}

fn read_project_text(path: &Path) -> Result<String, BuildError> {
    let bytes = fs::read(path).map_err(|error| project_file_error(path, error))?;
    String::from_utf8(bytes).map_err(|error| {
        BuildError::input(format!(
            "файл {} должен быть UTF-8: {error}",
            path.display()
        ))
    })
}

fn project_file_error(path: &Path, error: std::io::Error) -> BuildError {
    if error.kind() == std::io::ErrorKind::NotFound {
        BuildError::input(format!("обязательный файл не найден: {}", path.display()))
    } else {
        BuildError::operational(format!("не удалось прочитать {}: {error}", path.display()))
    }
}

fn require_asset_file(root: &Path, path: &VirtualPath) -> Result<PathBuf, BuildError> {
    let source = root.join(path.as_str());
    let metadata =
        fs::symlink_metadata(&source).map_err(|error| project_file_error(&source, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BuildError::input(format!(
            "ресурс /{path} должен быть обычным файлом без symlink"
        )));
    }
    let canonical_root = fs::canonicalize(root).map_err(|error| project_file_error(root, error))?;
    let canonical_root = normalize_project_path(&canonical_root)?;
    let canonical_source =
        fs::canonicalize(&source).map_err(|error| project_file_error(&source, error))?;
    let canonical_source = normalize_project_path(&canonical_source)?;
    if !canonical_source.starts_with(&canonical_root) {
        return Err(BuildError::input(format!(
            "ресурс /{path} выходит за пределы Assets"
        )));
    }
    Ok(canonical_source)
}

fn normalize_project_path(path: &Path) -> Result<PathBuf, BuildError> {
    strip_windows_verbatim_prefix(path).map_err(|error| {
        BuildError::input(format!(
            "неподдерживаемая форма Windows-пути {}: {error}",
            path.display()
        ))
    })
}

fn resource_error(context: &str, error: ResourceError) -> BuildError {
    resource_error_ref(context, &error)
}

fn resource_error_ref(context: &str, error: &ResourceError) -> BuildError {
    let missing = matches!(
        &error,
        ResourceError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound
    );
    if missing || error.kind() == ResourceErrorKind::NotFound {
        BuildError::input(format!("{context}: ресурс не найден ({error})"))
    } else if error.kind() == ResourceErrorKind::Io {
        BuildError::operational(format!("{context}: {error}"))
    } else {
        BuildError::input(format!("{context}: {error}"))
    }
}

fn asset_error(context: &str, error: AssetError) -> BuildError {
    match error {
        AssetError::Io { error, .. } => {
            if let Some(resource) = error
                .get_ref()
                .and_then(|source| source.downcast_ref::<ResourceError>())
            {
                resource_error_ref(context, resource)
            } else if error.kind() == std::io::ErrorKind::NotFound {
                BuildError::input(format!("{context}: {error}"))
            } else {
                BuildError::operational(format!("{context}: {error}"))
            }
        }
        error => BuildError::input(format!("{context}: {error}")),
    }
}

fn extension_is(path: &str, extension: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|found| found.eq_ignore_ascii_case(extension))
}

#[cfg(test)]
mod tests {
    use super::{AssetUsage, EnabledScene, assign_archives};
    use engine_files::VirtualPath;
    use std::collections::{BTreeMap, BTreeSet};

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    fn scenes() -> Vec<EnabledScene> {
        vec![
            EnabledScene {
                path: path("/scenes/main.rvescene"),
                stem: "main".to_owned(),
            },
            EnabledScene {
                path: path("/scenes/second.rvescene"),
                stem: "second".to_owned(),
            },
        ]
    }

    #[test]
    fn explicit_assignment_wins_and_default_uses_scene_membership() {
        let mut usages = BTreeMap::new();
        usages.insert(
            path("/models/one.fbx"),
            AssetUsage {
                scenes: BTreeSet::from([0]),
                explicit_archives: Vec::new(),
            },
        );
        usages.insert(
            path("/models/shared.fbx"),
            AssetUsage {
                scenes: BTreeSet::from([0, 1]),
                explicit_archives: Vec::new(),
            },
        );
        usages.insert(
            path("/models/props.fbx"),
            AssetUsage {
                scenes: BTreeSet::from([1]),
                explicit_archives: vec![(1, "props".to_owned())],
            },
        );
        let assignments = assign_archives(&scenes(), &usages, &BTreeSet::new(), None).unwrap();
        assert_eq!(assignments[&path("/models/one.fbx")], "scene-main");
        assert_eq!(assignments[&path("/models/shared.fbx")], "shared");
        assert_eq!(assignments[&path("/models/props.fbx")], "props");
    }

    #[test]
    fn rejects_conflicting_explicit_assignments_with_both_scenes() {
        let asset = path("/textures/shared.png");
        let usages = BTreeMap::from([(
            asset,
            AssetUsage {
                scenes: BTreeSet::from([0, 1]),
                explicit_archives: vec![(0, "first".to_owned()), (1, "second".to_owned())],
            },
        )]);
        let error = assign_archives(&scenes(), &usages, &BTreeSet::new(), None).unwrap_err();
        assert!(error.to_string().contains("/scenes/main.rvescene"));
        assert!(error.to_string().contains("/scenes/second.rvescene"));
    }

    #[test]
    fn rejects_archive_names_that_collide_without_case_sensitivity() {
        let usages = BTreeMap::from([
            (
                path("/models/one.fbx"),
                AssetUsage {
                    scenes: BTreeSet::from([0]),
                    explicit_archives: vec![(0, "Props".to_owned())],
                },
            ),
            (
                path("/models/two.fbx"),
                AssetUsage {
                    scenes: BTreeSet::from([1]),
                    explicit_archives: vec![(1, "props".to_owned())],
                },
            ),
        ]);
        let error = assign_archives(&scenes(), &usages, &BTreeSet::new(), None).unwrap_err();
        assert!(error.to_string().contains("Props"));
        assert!(error.to_string().contains("props"));
    }

    #[test]
    fn keeps_app_icon_in_base_and_rejects_an_explicit_other_archive() {
        let icon = path("/icons/app.png");
        let usages = BTreeMap::from([(
            icon.clone(),
            AssetUsage {
                scenes: BTreeSet::from([0]),
                explicit_archives: vec![(0, "props".to_owned())],
            },
        )]);
        let error = assign_archives(
            &scenes(),
            &usages,
            &BTreeSet::from([icon.clone()]),
            Some(&icon),
        )
        .unwrap_err();
        assert!(error.to_string().contains("AppIcon"));
        assert!(error.to_string().contains("/scenes/main.rvescene"));
    }

    #[test]
    fn rejects_a_default_archive_name_derived_from_an_invalid_scene_stem() {
        let scene = EnabledScene {
            path: path("/scenes/bad scene.rvescene"),
            stem: "bad scene".to_owned(),
        };
        let usages = BTreeMap::from([(
            path("/scripts/one.lua"),
            AssetUsage {
                scenes: BTreeSet::from([0]),
                explicit_archives: Vec::new(),
            },
        )]);
        let error = assign_archives(&[scene], &usages, &BTreeSet::new(), None).unwrap_err();
        assert!(error.to_string().contains("scene-bad scene"));
        assert!(error.to_string().contains("имени сцены"));
    }
}
