use crate::error::BuildError;
use engine_assets::{AssetError, Assets};
use engine_files::{VirtualPath, strip_windows_verbatim_prefix};
use engine_project::{BootstrapConfig, ProjectSettings};
use engine_resources::{
    ResourceConfig, ResourceError, ResourceErrorKind, ResourceHandle, ResourceSystem, SourceMode,
};
use engine_scene::{ParamSchema, ParamValue, SceneDocument, SceneError, expand};
use engine_script::{ScriptInterruptHandle, lite_properties};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

const LITE_PROPERTIES_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub(crate) struct ProjectBuild {
    pub(crate) project_dir: PathBuf,
    pub(crate) bootstrap_path: PathBuf,
    pub(crate) settings: ProjectSettings,
    pub(crate) files: BTreeMap<VirtualPath, PathBuf>,
    pub(crate) assignments: BTreeMap<VirtualPath, String>,
    pub(crate) archives: Vec<String>,
    #[cfg(windows)]
    pub(crate) icon: Option<VirtualPath>,
    pub(crate) heavy_scripts: Vec<HeavyScriptUse>,
}

/// Heavy-скрипт объекта включённой сцены.
#[derive(Clone, Debug)]
pub(crate) struct HeavyScriptUse {
    pub(crate) scene: String,
    pub(crate) object: String,
    pub(crate) name: String,
    pub(crate) params: BTreeMap<String, ParamValue>,
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
    let mut heavy_scripts = Vec::new();
    let mut usages = BTreeMap::<VirtualPath, AssetUsage>::new();
    let mut base_paths = BTreeSet::<VirtualPath>::new();
    let mut lite_schemas = BTreeMap::new();

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
        // Зависимости, скрипты и параметры берутся из раскрытого документа: префабы
        // приносят свои `[[assets]]` и объекты.
        let expansion = expand(&document, &mut |prefab| load_prefab(&handle, prefab))
            .map_err(|error| BuildError::input(format!("ошибка сцены /{path}: {error}")))?;
        let document = expansion.document;
        validate_lite_params(&document, &path, &handle, &mut lite_schemas)?;
        heavy_scripts.extend(document.objects.iter().flat_map(|object| {
            object.heavy_scripts.iter().map(|script| HeavyScriptUse {
                scene: path.to_string(),
                object: object.id.clone(),
                name: script.name.clone(),
                params: script.params.clone(),
            })
        }));

        base_paths.insert(path.clone());
        let scene_index = enabled_scenes.len();
        enabled_scenes.push(EnabledScene {
            path: path.clone(),
            stem,
        });

        // Файлы префабов объявлены в `[[assets]]` использующих их документов и попадают
        // в архивы по тем же правилам; здесь они учитываются и без такой записи.
        for prefab in expansion.prefabs {
            require_asset_file(&assets_dir, &prefab)?;
            usages.entry(prefab).or_default().scenes.insert(scene_index);
        }
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

    validate_heavy_scripts_configured(&settings, &heavy_scripts)?;

    Ok(ProjectBuild {
        project_dir,
        bootstrap_path,
        settings,
        files,
        assignments,
        archives,
        #[cfg(windows)]
        icon,
        heavy_scripts,
    })
}

/// Проверяет `params` lite-скриптов сцены по схеме `properties` их классов; схемы
/// кэшируются по пути скрипта для всех сцен.
fn validate_lite_params(
    document: &SceneDocument,
    scene: &VirtualPath,
    handle: &ResourceHandle,
    schemas: &mut BTreeMap<VirtualPath, Option<ParamSchema>>,
) -> Result<(), BuildError> {
    for object in &document.objects {
        for script in &object.lite_scripts {
            if !schemas.contains_key(&script.path) {
                let label = format!("/{}", script.path);
                let bytes = handle
                    .read_all(label.as_str())
                    .and_then(|request| request.wait())
                    .map_err(|error| resource_error(&format!("скрипт {label}"), error))?;
                let source = String::from_utf8(bytes).map_err(|error| {
                    BuildError::input(format!("скрипт {label} должен быть UTF-8: {error}"))
                })?;
                let schema =
                    read_lite_properties_with_timeout(&label, &source, LITE_PROPERTIES_TIMEOUT)
                        .map_err(BuildError::input)?;
                schemas.insert(script.path.clone(), schema);
            }
            if let Some(schema) = &schemas[&script.path] {
                schema.resolve(&script.params).map_err(|error| {
                    BuildError::input(format!(
                        "сцена /{scene}, объект {:?}, скрипт /{}: {error}",
                        object.id, script.path
                    ))
                })?;
            }
        }
    }
    Ok(())
}

fn read_lite_properties_with_timeout(
    label: &str,
    source: &str,
    timeout: Duration,
) -> Result<Option<ParamSchema>, String> {
    let interrupt = ScriptInterruptHandle::new();
    let (done, finished) = mpsc::channel::<()>();
    let watchdog = {
        let interrupt = interrupt.clone();
        std::thread::spawn(move || {
            if finished.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout) {
                interrupt.interrupt();
            }
        })
    };
    let result = lite_properties(label, source, &interrupt);
    drop(done);
    let _ = watchdog.join();
    if interrupt.is_interrupted() {
        Err(format!(
            "чтение properties скрипта {label} превысило deadline {} мс",
            timeout.as_millis()
        ))
    } else {
        result.map_err(|error| error.to_string())
    }
}

fn validate_heavy_scripts_configured(
    settings: &ProjectSettings,
    heavy_scripts: &[HeavyScriptUse],
) -> Result<(), BuildError> {
    if settings.heavy_scripts.is_none()
        && let Some(script) = heavy_scripts.first()
    {
        return Err(BuildError::input(format!(
            "сцена /{} ссылается на heavy-скрипт {:?}, но в project.rveproject не задан heavy_scripts",
            script.scene, script.name
        )));
    }
    Ok(())
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
    use super::{
        AssetUsage, EnabledScene, HeavyScriptUse, assign_archives,
        validate_heavy_scripts_configured,
    };
    use engine_files::VirtualPath;
    use engine_project::ProjectSettings;
    use std::collections::{BTreeMap, BTreeSet};

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    #[test]
    fn heavy_scene_reference_requires_a_scripts_library_setting() {
        let settings = ProjectSettings::parse(
            "id = \"demo\"\nname = \"Demo\"\nversion = \"1\"\n[[scenes]]\npath = \"scenes/main.rvescene\"\n",
        )
        .unwrap();
        let error = validate_heavy_scripts_configured(
            &settings,
            &[HeavyScriptUse {
                scene: "/scenes/main.rvescene".to_owned(),
                object: "mesh".to_owned(),
                name: "Bobber".to_owned(),
                params: BTreeMap::new(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("/scenes/main.rvescene"));
        assert!(error.to_string().contains("Bobber"));
        assert!(error.to_string().contains("heavy_scripts"));
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

#[cfg(test)]
mod lite_params_tests {
    use super::{prepare_project, read_lite_properties_with_timeout};
    use crate::temporary::TempDir;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    const SCRIPT: &str =
        "return { properties = { speed = { type = \"float\", default = 30, min = 0 } } }\n";

    fn project(root: &Path, params: &str) {
        let assets = root.join("Assets");
        fs::create_dir_all(assets.join("scripts")).unwrap();
        fs::write(
            root.join("project.rveproject"),
            "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n[[scenes]]\npath = \"/main.rvescene\"\n",
        )
        .unwrap();
        fs::write(root.join("bootstrap.rves"), "").unwrap();
        fs::write(assets.join("scripts/rotate.lua"), SCRIPT).unwrap();
        fs::write(
            assets.join("main.rvescene"),
            format!(
                "[[assets]]\npath = \"/scripts/rotate.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nfov_deg = 60.0\nnear = 0.1\nfar = 100.0\n[[objects.lite_scripts]]\npath = \"/scripts/rotate.lua\"\nparams = {params}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn lite_params_are_checked_against_the_script_schema() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-params").unwrap();
        project(root.path(), "{ speed = 90 }");
        prepare_project(root.path()).unwrap();
        for params in ["{ speed = -1.0 }", "{ speed = \"fast\" }", "{ spin = 1 }"] {
            project(root.path(), params);
            let error = prepare_project(root.path()).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{params}: {error}");
            assert!(error.to_string().contains("\"camera\""), "{error}");
            assert!(error.to_string().contains("/scripts/rotate.lua"), "{error}");
        }
    }

    #[test]
    fn malformed_properties_are_a_project_error() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-params").unwrap();
        project(root.path(), "{}");
        fs::write(
            root.path().join("Assets/scripts/rotate.lua"),
            "return { properties = { speed = { type = \"double\" } } }\n",
        )
        .unwrap();
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("rotate.lua"), "{error}");
    }

    #[test]
    fn unbounded_lite_properties_are_interrupted_at_the_deadline() {
        let started = Instant::now();
        let error = read_lite_properties_with_timeout(
            "loop.lua",
            "while true do end",
            Duration::from_millis(20),
        )
        .unwrap_err();

        assert!(error.contains("deadline"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

/// Префаб читается из `Assets` через тот же `ResourceHandle`, что и сцены.
fn load_prefab(handle: &ResourceHandle, path: &VirtualPath) -> Result<SceneDocument, SceneError> {
    let label = format!("/{path}");
    let error = |message: String| SceneError {
        source: label.clone(),
        object_id: None,
        message,
    };
    let bytes = handle
        .read_all(label.as_str())
        .and_then(|request| request.wait())
        .map_err(|resource| error(resource.to_string()))?;
    let text = String::from_utf8(bytes)
        .map_err(|utf8| error(format!("файл должен быть UTF-8: {utf8}")))?;
    SceneDocument::parse_prefab(label.clone(), &text)
}

#[cfg(test)]
mod prefab_tests {
    use super::prepare_project;
    use crate::temporary::TempDir;
    use engine_files::VirtualPath;
    use std::fs;
    use std::path::Path;

    const CAMERA: &str =
        "[[objects]]\nid = \"camera\"\n[objects.camera]\nfov_deg = 60.0\nnear = 0.1\nfar = 100.0\n";

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join("Assets").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn project(root: &Path) {
        fs::write(
            root.join("project.rveproject"),
            "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n[[scenes]]\npath = \"/scenes/main.rvescene\"\n[[scenes]]\npath = \"/scenes/second.rvescene\"\n",
        )
        .unwrap();
        fs::write(root.join("bootstrap.rves"), "").unwrap();
        // Общий префаб с вложенным; второй используется только main.
        write(
            root,
            "prefabs/lamp.rveprefab",
            "[[assets]]\npath = \"/prefabs/bulb.rveprefab\"\n[[assets]]\npath = \"/scripts/rotate.lua\"\n[[objects]]\nid = \"post\"\n[[objects.lite_scripts]]\npath = \"/scripts/rotate.lua\"\n[[instances]]\nid = \"bulb\"\nparent = \"post\"\nprefab = \"/prefabs/bulb.rveprefab\"\n",
        );
        write(
            root,
            "prefabs/bulb.rveprefab",
            "[[objects]]\nid = \"glass\"\n",
        );
        write(
            root,
            "prefabs/sign.rveprefab",
            "[[objects]]\nid = \"board\"\n",
        );
        write(
            root,
            "scripts/rotate.lua",
            "return { properties = { speed = { type = \"float\", default = 1 } } }\n",
        );
        let lamp = "[[instances]]\nid = \"lamp\"\nprefab = \"/prefabs/lamp.rveprefab\"\n";
        write(
            root,
            "scenes/main.rvescene",
            &format!(
                "[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n[[assets]]\npath = \"/prefabs/sign.rveprefab\"\n{CAMERA}{lamp}[[instances]]\nid = \"sign\"\nprefab = \"/prefabs/sign.rveprefab\"\n"
            ),
        );
        write(
            root,
            "scenes/second.rvescene",
            &format!("[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n{CAMERA}{lamp}"),
        );
    }

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    #[test]
    fn prefabs_and_their_resources_are_packed_by_scene_membership() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-prefab").unwrap();
        project(root.path());
        let build = prepare_project(root.path()).unwrap();
        let archive = |value: &str| build.assignments[&path(value)].as_str();
        assert_eq!(archive("/prefabs/lamp.rveprefab"), "shared");
        assert_eq!(archive("/prefabs/bulb.rveprefab"), "shared");
        assert_eq!(archive("/scripts/rotate.lua"), "shared");
        assert_eq!(archive("/prefabs/sign.rveprefab"), "scene-main");
        assert!(build.files.contains_key(&path("/prefabs/bulb.rveprefab")));
    }

    #[test]
    fn script_params_inside_prefabs_are_checked() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-prefab").unwrap();
        project(root.path());
        let lamp = "[[instances]]\nid = \"lamp\"\nprefab = \"/prefabs/lamp.rveprefab\"\n[[instances.overrides]]\ntarget = \"post\"\nlite_params = [{ path = \"/scripts/rotate.lua\", params = { spin = 1 } }]\n";
        write(
            root.path(),
            "scenes/second.rvescene",
            &format!("[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n{CAMERA}{lamp}"),
        );
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("lamp"), "{error}");
    }

    #[test]
    fn a_missing_prefab_is_a_project_error() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-prefab").unwrap();
        project(root.path());
        fs::remove_file(root.path().join("Assets/prefabs/bulb.rveprefab")).unwrap();
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("bulb.rveprefab"), "{error}");
    }
}
