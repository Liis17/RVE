use crate::error::BuildError;
use engine_assets::{AssetError, Assets};
use engine_files::{VirtualPath, strip_windows_verbatim_prefix};
use engine_project::{
    BootstrapConfig, PREFAB_CATALOG_FILE_NAME, PrefabCatalogFile, ProjectSettings,
};
use engine_resources::{
    ResourceConfig, ResourceError, ResourceErrorKind, ResourceHandle, ResourceSystem, SourceMode,
};
use engine_scene::{CameraSettings, PhysicsMaterialSettings, PostProcessSettings};
use engine_scene::{
    ParamSchema, ParamValue, PrefabTemplate, SceneAsset, SceneDocument, SceneError, SceneObject,
    expand,
};
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
    pub(crate) prefabs: PrefabCatalogFile,
    pub(crate) files: BTreeMap<VirtualPath, PathBuf>,
    pub(crate) assignments: BTreeMap<VirtualPath, String>,
    pub(crate) archives: Vec<String>,
    #[cfg(windows)]
    pub(crate) icon: Option<VirtualPath>,
    pub(crate) heavy_scripts: Vec<HeavyScriptUse>,
}

/// Heavy-скрипт объекта сцены или префаба.
#[derive(Clone, Debug)]
pub(crate) struct HeavyScriptUse {
    /// Владелец для сообщений: «сцена /…» или «префаб /…».
    pub(crate) owner: String,
    pub(crate) object: String,
    pub(crate) name: String,
    pub(crate) params: BTreeMap<String, ParamValue>,
}

/// Владелец ассета: включённая сцена или тяжёлый префаб из `prefabs.rveprefabs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Scene(usize),
    HeavyPrefab(VirtualPath),
}

#[derive(Default)]
struct AssetUsage {
    /// Сцены, которые используют ассет; для правила архива по умолчанию важно число сцен.
    scenes: BTreeSet<usize>,
    /// Явные архивы из записей `[[assets]]` и списка тяжёлых префабов вместе с владельцем.
    explicit_archives: Vec<(Owner, String)>,
}

impl AssetUsage {
    fn add_owner(&mut self, owner: &Owner) {
        if let Owner::Scene(index) = owner {
            self.scenes.insert(*index);
        }
    }
}

/// Ресурсы, по которым проверяется файл ассета.
struct AssetSources<'a> {
    handle: &'a ResourceHandle,
    assets: &'a Assets,
    assets_dir: &'a Path,
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
    let prefabs = PrefabCatalogFile::load(project_dir.join(PREFAB_CATALOG_FILE_NAME))
        .map_err(BuildError::input)?;

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
        prefabs,
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
    prefabs: PrefabCatalogFile,
    bootstrap: BootstrapConfig,
    resources: &mut ResourceSystem,
) -> Result<ProjectBuild, BuildError> {
    let handle = resources.handle();
    let assets = Assets::with_resources(handle.clone());
    let sources = AssetSources {
        handle: &handle,
        assets: &assets,
        assets_dir: &assets_dir,
    };
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
        validate_lite_params(
            &document.objects,
            &format!("сцена /{path}"),
            &handle,
            &mut lite_schemas,
        )?;
        heavy_scripts.extend(document.objects.iter().flat_map(|object| {
            object.heavy_scripts.iter().map(|script| HeavyScriptUse {
                owner: format!("сцена /{path}"),
                object: object.id.clone(),
                name: script.name.clone(),
                params: script.params.clone(),
            })
        }));
        let mut scene_assets = document.assets.clone();
        scene_assets.extend(unused_prefab_assets(
            &path,
            &document.assets,
            &expansion.prefabs,
            &handle,
            &mut lite_schemas,
            &mut heavy_scripts,
        )?);

        base_paths.insert(path.clone());
        let scene_index = enabled_scenes.len();
        enabled_scenes.push(EnabledScene {
            path: path.clone(),
            stem,
        });
        let owner = Owner::Scene(scene_index);

        // Файлы префабов, раскрытых сценой, учитываются как её ресурсы; для них не нужна
        // запись `[[assets]]`.
        for prefab in &expansion.prefabs {
            record_asset(&mut usages, &sources, prefab, &owner, None)?;
        }
        for asset in scene_assets {
            record_asset(&mut usages, &sources, &asset.path, &owner, None)?;
            if let Some(archive) = asset.archive {
                add_explicit_archive(&mut usages, &asset.path, &owner, archive);
            }
        }
    }

    // Тяжёлые префабы из списка проекта: файл и его зависимости получают явный архив записи,
    // если он задан. Иначе действуют правила сцены или `shared`.
    for entry in &prefabs.prefabs {
        let path = VirtualPath::parse(&entry.path).map_err(|error| {
            BuildError::input(format!(
                "некорректный путь тяжёлого префаба {:?}: {error}",
                entry.path
            ))
        })?;
        let owner = Owner::HeavyPrefab(path.clone());
        let template = load_spawn_template(
            &path,
            &format!("префаб /{path}"),
            &handle,
            &mut lite_schemas,
            &mut heavy_scripts,
        )?;
        template.check_spawnable().map_err(|error| {
            BuildError::input(format!("тяжёлый префаб /{path} нельзя заспавнить: {error}"))
        })?;
        for asset in template.assets() {
            record_asset(
                &mut usages,
                &sources,
                &asset.path,
                &owner,
                entry.archive.as_deref(),
            )?;
            if let Some(archive) = &asset.archive {
                add_explicit_archive(&mut usages, &asset.path, &owner, archive.clone());
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
        prefabs,
        files,
        assignments,
        archives,
        #[cfg(windows)]
        icon,
        heavy_scripts,
    })
}

/// `.rveprefab` из `[[assets]]` сцены, которого нет среди её экземпляров. Раскрытый документ
/// его не содержит, поэтому шаблон даёт ресурсы, lite- и heavy-скрипты для spawn. Возвращает
/// ресурсы шаблона, включая вложенные префабы.
fn unused_prefab_assets(
    scene: &VirtualPath,
    assets: &[SceneAsset],
    instanced: &[VirtualPath],
    handle: &ResourceHandle,
    schemas: &mut BTreeMap<VirtualPath, Option<ParamSchema>>,
    heavy_scripts: &mut Vec<HeavyScriptUse>,
) -> Result<Vec<SceneAsset>, BuildError> {
    let mut extra = Vec::new();
    for asset in assets
        .iter()
        .filter(|asset| extension_is(asset.path.as_str(), "rveprefab"))
    {
        if instanced.contains(&asset.path) {
            continue;
        }
        let template = load_spawn_template(
            &asset.path,
            &format!("сцена /{scene}"),
            handle,
            schemas,
            heavy_scripts,
        )?;
        extra.extend(template.assets().iter().cloned());
    }
    Ok(extra)
}

/// Раскрывает префаб для spawn и проверяет его lite-параметры. Heavy-скрипты получают метку
/// владельца `owner`, например «сцена /…» или «префаб /…».
fn load_spawn_template(
    path: &VirtualPath,
    owner: &str,
    handle: &ResourceHandle,
    schemas: &mut BTreeMap<VirtualPath, Option<ParamSchema>>,
    heavy_scripts: &mut Vec<HeavyScriptUse>,
) -> Result<PrefabTemplate, BuildError> {
    let template = PrefabTemplate::load(path, &mut |child: &VirtualPath| {
        read_prefab_text(handle, child)
    })
    .map_err(|error| BuildError::input(format!("ошибка префаба /{path}: {error}")))?;
    validate_lite_params(
        template.objects(),
        &format!("префаб /{path}"),
        handle,
        schemas,
    )?;
    heavy_scripts.extend(template.objects().iter().flat_map(|object| {
        object.heavy_scripts.iter().map(|script| HeavyScriptUse {
            owner: owner.to_owned(),
            object: object.id.clone(),
            name: script.name.clone(),
            params: script.params.clone(),
        })
    }));
    Ok(template)
}

/// Учитывает использование файла `path` владельцем `owner`: файл должен существовать, настройки
/// камеры проверяются, а текстуры модели становятся ресурсами того же владельца. `list_archive`
/// — явный архив записи списка тяжёлых префабов; он относится к файлу и его зависимостям.
fn record_asset(
    usages: &mut BTreeMap<VirtualPath, AssetUsage>,
    sources: &AssetSources<'_>,
    path: &VirtualPath,
    owner: &Owner,
    list_archive: Option<&str>,
) -> Result<(), BuildError> {
    usages.entry(path.clone()).or_default().add_owner(owner);
    sources
        .handle
        .stat(format!("/{path}"))
        .and_then(|request| request.wait())
        .map_err(|error| resource_error(&format!("ресурс /{path}"), error))?;
    require_asset_file(sources.assets_dir, path)?;
    if let Some(archive) = list_archive {
        add_explicit_archive(usages, path, owner, archive.to_owned());
    }

    if extension_is(path.as_str(), "camerasettings") {
        let bytes = sources
            .handle
            .read_all(format!("/{path}"))
            .and_then(|request| request.wait())
            .map_err(|error| resource_error(&format!("настройки камеры /{path}"), error))?;
        let source = String::from_utf8(bytes).map_err(|error| {
            BuildError::input(format!(
                "настройки камеры /{path} должны быть UTF-8: {error}"
            ))
        })?;
        CameraSettings::parse(format!("/{path}"), &source)
            .map_err(|error| BuildError::input(error.to_string()))?;
    }

    if extension_is(path.as_str(), "postprocess") {
        let bytes = sources
            .handle
            .read_all(format!("/{path}"))
            .and_then(|request| request.wait())
            .map_err(|error| resource_error(&format!("постобработка /{path}"), error))?;
        let source = String::from_utf8(bytes).map_err(|error| {
            BuildError::input(format!("постобработка /{path} должна быть UTF-8: {error}"))
        })?;
        PostProcessSettings::parse(format!("/{path}"), &source)
            .map_err(|error| BuildError::input(error.to_string()))?;
    }

    if extension_is(path.as_str(), "rvephysmat") {
        let bytes = sources
            .handle
            .read_all(format!("/{path}"))
            .and_then(|request| request.wait())
            .map_err(|error| resource_error(&format!("физический материал /{path}"), error))?;
        let source = String::from_utf8(bytes).map_err(|error| {
            BuildError::input(format!(
                "физический материал /{path} должен быть UTF-8: {error}"
            ))
        })?;
        PhysicsMaterialSettings::parse(format!("/{path}"), &source)
            .map_err(|error| BuildError::input(error.to_string()))?;
    }

    if extension_is(path.as_str(), "fbx") {
        let dependencies = sources
            .assets
            .model_dependencies(&format!("/{path}"))
            .map_err(|error| asset_error(&format!("модель /{path}"), error))?;
        for dependency in dependencies {
            let dependency = VirtualPath::parse(&dependency).map_err(|error| {
                BuildError::input(format!("модель /{path} вернула неверную текстуру: {error}"))
            })?;
            sources
                .handle
                .stat(format!("/{dependency}"))
                .and_then(|request| request.wait())
                .map_err(|error| resource_error(&format!("текстура /{dependency}"), error))?;
            require_asset_file(sources.assets_dir, &dependency)?;
            usages
                .entry(dependency.clone())
                .or_default()
                .add_owner(owner);
            if let Some(archive) = list_archive {
                add_explicit_archive(usages, &dependency, owner, archive.to_owned());
            }
        }
    }
    Ok(())
}

fn add_explicit_archive(
    usages: &mut BTreeMap<VirtualPath, AssetUsage>,
    path: &VirtualPath,
    owner: &Owner,
    archive: String,
) {
    usages
        .entry(path.clone())
        .or_default()
        .explicit_archives
        .push((owner.clone(), archive));
}

/// Проверяет `params` lite-скриптов объектов `owner` (сцены или префаба) по схеме
/// `properties` их классов; схемы кэшируются по пути скрипта для всех документов.
fn validate_lite_params(
    objects: &[SceneObject],
    owner: &str,
    handle: &ResourceHandle,
    schemas: &mut BTreeMap<VirtualPath, Option<ParamSchema>>,
) -> Result<(), BuildError> {
    for object in objects {
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
                        "{owner}, объект {:?}, скрипт /{}: {error}",
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
            "{} ссылается на heavy-скрипт {:?}, но в project.rveproject не задан heavy_scripts",
            script.owner, script.name
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
            if let Some((owner, archive)) = usage.and_then(|usage| {
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
                    "{reason}: /{path} явно назначен в архив {archive:?} ({})",
                    owner_label(owner, scenes)
                )));
            }
            assignments.insert((*path).clone(), "base".to_owned());
            continue;
        }

        let usage = usage.expect("non-base paths are present in the usage map");
        // Запись сцены — решение автора: любой конфликт с ней, в том числе с записью тяжёлого
        // префаба, остаётся ошибкой. Записи только тяжёлых префабов (файл префаба и его
        // зависимости) при расхождении дают `shared`.
        if let Some(first) = usage
            .explicit_archives
            .iter()
            .find(|(owner, _)| matches!(owner, Owner::Scene(_)))
        {
            if let Some(second) = usage
                .explicit_archives
                .iter()
                .find(|(_, archive)| archive != &first.1)
            {
                return Err(BuildError::input(format!(
                    "ассет /{path} назначен в разные архивы {:?} ({}) и {:?} ({})",
                    first.1,
                    owner_label(&first.0, scenes),
                    second.1,
                    owner_label(&second.0, scenes)
                )));
            }
            assignments.insert((*path).clone(), first.1.clone());
            continue;
        }
        if let Some(first) = usage.explicit_archives.first() {
            let archive = if usage
                .explicit_archives
                .iter()
                .all(|(_, archive)| archive == &first.1)
            {
                first.1.clone()
            } else {
                "shared".to_owned()
            };
            assignments.insert((*path).clone(), archive);
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

/// Метка владельца для сообщений об ошибках: «сцена /…» или «тяжёлый префаб /…».
fn owner_label(owner: &Owner, scenes: &[EnabledScene]) -> String {
    match owner {
        Owner::Scene(index) => format!("сцена /{}", scenes[*index].path),
        Owner::HeavyPrefab(path) => format!("тяжёлый префаб /{path}"),
    }
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
        AssetUsage, EnabledScene, HeavyScriptUse, Owner, assign_archives,
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
                owner: "сцена /scenes/main.rvescene".to_owned(),
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
                explicit_archives: vec![(Owner::Scene(1), "props".to_owned())],
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
                explicit_archives: vec![
                    (Owner::Scene(0), "first".to_owned()),
                    (Owner::Scene(1), "second".to_owned()),
                ],
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
                    explicit_archives: vec![(Owner::Scene(0), "Props".to_owned())],
                },
            ),
            (
                path("/models/two.fbx"),
                AssetUsage {
                    scenes: BTreeSet::from([1]),
                    explicit_archives: vec![(Owner::Scene(1), "props".to_owned())],
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
                explicit_archives: vec![(Owner::Scene(0), "props".to_owned())],
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
                "[[assets]]\npath = \"/scripts/rotate.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects.lite_scripts]]\npath = \"/scripts/rotate.lua\"\nparams = {params}\n"
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
    let text = read_prefab_text(handle, path).map_err(|message| SceneError {
        source: label.clone(),
        object_id: None,
        message,
    })?;
    SceneDocument::parse_prefab(label, &text)
}

/// Текст префаба для [`PrefabTemplate::load`] и раскрытия экземпляров.
fn read_prefab_text(handle: &ResourceHandle, path: &VirtualPath) -> Result<String, String> {
    let label = format!("/{path}");
    let bytes = handle
        .read_all(label.as_str())
        .and_then(|request| request.wait())
        .map_err(|resource| resource.to_string())?;
    String::from_utf8(bytes).map_err(|utf8| format!("файл должен быть UTF-8: {utf8}"))
}

#[cfg(test)]
mod prefab_tests {
    use super::prepare_project;
    use crate::temporary::TempDir;
    use engine_files::VirtualPath;
    use std::fs;
    use std::path::Path;

    const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";

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
    fn physics_materials_are_validated_and_packed_with_their_scene() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-physics-material").unwrap();
        fs::write(
            root.path().join("project.rveproject"),
            "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n[[scenes]]\npath = \"/scenes/main.rvescene\"\n",
        )
        .unwrap();
        fs::write(root.path().join("bootstrap.rves"), "").unwrap();
        write(
            root.path(),
            "scenes/main.rvescene",
            &format!(
                "[[assets]]\npath = \"/materials/ice.rvephysmat\"\n{CAMERA}[[objects]]\nid = \"ball\"\n[objects.collider]\nshape = \"sphere\"\nradius = 0.5\nmaterial = \"/materials/ice.rvephysmat\"\n"
            ),
        );
        write(
            root.path(),
            "materials/ice.rvephysmat",
            "static_friction = 0.05\n",
        );
        let build = prepare_project(root.path()).unwrap();
        assert!(build.files.contains_key(&path("/materials/ice.rvephysmat")));

        write(
            root.path(),
            "materials/ice.rvephysmat",
            "static_friction = -1.0\n",
        );
        let error = prepare_project(root.path()).unwrap_err();
        assert!(error.to_string().contains("static_friction"), "{error}");
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

#[cfg(test)]
mod unused_prefab_tests {
    use super::prepare_project;
    use crate::temporary::TempDir;
    use engine_files::VirtualPath;
    use std::fs;
    use std::path::Path;

    const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    const SPARK_SCRIPT: &str =
        "return { properties = { speed = { type = \"float\", default = 1 } } }\n";

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join("Assets").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Сцена объявляет `spark` в `[[assets]]` без экземпляров. `spark` тянет скрипт и вложенный
    /// `glow`; `unused` в сцене не упомянут. `max_line` добавляется в запись префаба сцены.
    fn project(root: &Path, speed: &str, max_line: &str) {
        fs::write(
            root.join("project.rveproject"),
            "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n[[scenes]]\npath = \"/scenes/main.rvescene\"\n",
        )
        .unwrap();
        fs::write(root.join("bootstrap.rves"), "").unwrap();
        write(root, "scripts/spark.lua", SPARK_SCRIPT);
        write(
            root,
            "prefabs/glow.rveprefab",
            "[[objects]]\nid = \"light\"\n",
        );
        write(
            root,
            "prefabs/spark.rveprefab",
            &format!(
                "[[assets]]\npath = \"/scripts/spark.lua\"\n[[assets]]\npath = \"/prefabs/glow.rveprefab\"\n[[objects]]\nid = \"spark\"\n[[objects.lite_scripts]]\npath = \"/scripts/spark.lua\"\nparams = {speed}\n[[instances]]\nid = \"glow\"\nparent = \"spark\"\nprefab = \"/prefabs/glow.rveprefab\"\n"
            ),
        );
        write(
            root,
            "prefabs/unused.rveprefab",
            "[[objects]]\nid = \"junk\"\n",
        );
        write(
            root,
            "scenes/main.rvescene",
            &format!("[[assets]]\npath = \"/prefabs/spark.rveprefab\"\n{max_line}{CAMERA}"),
        );
    }

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    #[test]
    fn fast_prefab_without_instance_is_packed_with_its_dependencies() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-unused").unwrap();
        project(root.path(), "{ speed = 2 }", "");
        let build = prepare_project(root.path()).unwrap();
        let archive = |value: &str| build.assignments.get(&path(value)).map(String::as_str);
        assert_eq!(archive("/prefabs/spark.rveprefab"), Some("scene-main"));
        assert_eq!(archive("/scripts/spark.lua"), Some("scene-main"));
        assert_eq!(archive("/prefabs/glow.rveprefab"), Some("scene-main"));
        assert!(build.files.contains_key(&path("/scripts/spark.lua")));
    }

    #[test]
    fn prefab_files_that_no_scene_declares_are_not_packed() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-unused").unwrap();
        project(root.path(), "{ speed = 2 }", "");
        let build = prepare_project(root.path()).unwrap();
        assert!(
            !build
                .assignments
                .contains_key(&path("/prefabs/unused.rveprefab"))
        );
        assert!(!build.files.contains_key(&path("/prefabs/unused.rveprefab")));
    }

    #[test]
    fn invalid_lite_params_inside_an_unused_fast_prefab_are_a_project_error() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-unused").unwrap();
        project(root.path(), "{ speed = \"fast\" }", "");
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(
            error.to_string().contains("/prefabs/spark.rveprefab"),
            "{error}"
        );
        assert!(error.to_string().contains("/scripts/spark.lua"), "{error}");
    }

    #[test]
    fn max_instances_stays_in_the_packed_scene_and_must_be_positive() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-unused").unwrap();
        project(root.path(), "{ speed = 2 }", "max_instances = 100\n");
        let build = prepare_project(root.path()).unwrap();
        let source = fs::read(root.path().join("Assets/scenes/main.rvescene")).unwrap();
        assert_eq!(
            fs::read(&build.files[&path("/scenes/main.rvescene")]).unwrap(),
            source
        );

        project(root.path(), "{ speed = 2 }", "max_instances = 0\n");
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("max_instances"), "{error}");
    }
}

#[cfg(test)]
mod heavy_prefab_tests {
    use super::prepare_project;
    use crate::temporary::TempDir;
    use engine_files::VirtualPath;
    use std::fs;
    use std::path::Path;

    const SPIN_SCRIPT: &str =
        "return { properties = { speed = { type = \"float\", default = 1 } } }\n";
    const TOWER_BODY: &str = "[[assets]]\npath = \"/scripts/spin.lua\"\n[[assets]]\npath = \"/textures/tower.png\"\n[[objects]]\nid = \"tower\"\n[[objects.lite_scripts]]\npath = \"/scripts/spin.lua\"\nparams = { speed = 2 }\n";

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join("Assets").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    /// Проект с одной сценой, в которой есть `scene_assets`, и префабом `/prefabs/tower.rveprefab`.
    /// `list` — содержимое `prefabs.rveprefabs`; `None` означает отсутствие файла.
    fn project(root: &Path, scene_assets: &str, list: Option<&str>) {
        fs::write(
            root.join("project.rveproject"),
            "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n[[scenes]]\npath = \"/scenes/main.rvescene\"\n",
        )
        .unwrap();
        fs::write(root.join("bootstrap.rves"), "").unwrap();
        if let Some(list) = list {
            fs::write(root.join("prefabs.rveprefabs"), list).unwrap();
        }
        write(
            root,
            "scenes/main.rvescene",
            &format!("{scene_assets}[[objects]]\nid = \"camera\"\n[objects.camera]\n"),
        );
        write(root, "scripts/spin.lua", SPIN_SCRIPT);
        write(root, "textures/tower.png", "texture");
        write(root, "prefabs/tower.rveprefab", TOWER_BODY);
    }

    #[test]
    fn heavy_prefab_is_packed_with_its_archive_and_dependencies() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "",
            Some(
                "[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\narchive = \"towers\"\nmax_instances = 3\n",
            ),
        );
        let build = prepare_project(root.path()).unwrap();
        assert_eq!(build.prefabs.prefabs.len(), 1);
        assert_eq!(
            build.assignments[&path("/prefabs/tower.rveprefab")],
            "towers"
        );
        assert_eq!(build.assignments[&path("/textures/tower.png")], "towers");
        assert_eq!(build.assignments[&path("/scripts/spin.lua")], "towers");
        assert!(build.files.contains_key(&path("/prefabs/tower.rveprefab")));
    }

    #[test]
    fn heavy_prefab_without_archive_goes_to_shared() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "",
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\n"),
        );
        let build = prepare_project(root.path()).unwrap();
        assert_eq!(
            build.assignments[&path("/prefabs/tower.rveprefab")],
            "shared"
        );
        assert_eq!(build.assignments[&path("/textures/tower.png")], "shared");
        assert_eq!(build.assignments[&path("/scripts/spin.lua")], "shared");
    }

    #[test]
    fn dependency_used_by_a_scene_follows_the_scene_rule_without_a_list_archive() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "[[assets]]\npath = \"/textures/tower.png\"\n",
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\n"),
        );
        let build = prepare_project(root.path()).unwrap();
        assert_eq!(
            build.assignments[&path("/textures/tower.png")],
            "scene-main"
        );
        assert_eq!(
            build.assignments[&path("/prefabs/tower.rveprefab")],
            "shared"
        );
    }

    #[test]
    fn list_archive_wins_over_the_scene_rule_for_dependencies() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "[[assets]]\npath = \"/textures/tower.png\"\n",
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\narchive = \"towers\"\n"),
        );
        let build = prepare_project(root.path()).unwrap();
        assert_eq!(build.assignments[&path("/textures/tower.png")], "towers");
        assert_eq!(
            build.assignments[&path("/prefabs/tower.rveprefab")],
            "towers"
        );
    }

    #[test]
    fn list_archive_conflicting_with_a_scene_record_is_an_error() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "[[assets]]\npath = \"/prefabs/tower.rveprefab\"\narchive = \"scene-main\"\n",
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\narchive = \"towers\"\n"),
        );
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        let message = error.to_string();
        assert!(message.contains("\"towers\""), "{message}");
        assert!(message.contains("\"scene-main\""), "{message}");
        assert!(
            message.contains("тяжёлый префаб /prefabs/tower.rveprefab"),
            "{message}"
        );
        assert!(message.contains("/scenes/main.rvescene"), "{message}");
    }

    #[test]
    fn dependency_of_two_heavy_prefabs_with_different_archives_goes_to_shared() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "",
            Some(
                "[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\narchive = \"towers\"\n\
                 [[prefabs]]\npath = \"/prefabs/lamp.rveprefab\"\narchive = \"lamps\"\n",
            ),
        );
        write(
            root.path(),
            "prefabs/lamp.rveprefab",
            "[[assets]]\npath = \"/textures/tower.png\"\n[[objects]]\nid = \"lamp\"\n",
        );
        let build = prepare_project(root.path()).unwrap();
        // Общая текстура получает разные явные архивы от двух тяжёлых префабов: она в shared.
        assert_eq!(build.assignments[&path("/textures/tower.png")], "shared");
        // Файлы префабов и собственные зависимости остаются в своих архивах.
        assert_eq!(
            build.assignments[&path("/prefabs/tower.rveprefab")],
            "towers"
        );
        assert_eq!(build.assignments[&path("/prefabs/lamp.rveprefab")], "lamps");
        assert_eq!(build.assignments[&path("/scripts/spin.lua")], "towers");
        assert!(build.archives.iter().any(|archive| archive == "shared"));
    }

    #[test]
    fn heavy_prefab_that_cannot_be_spawned_fails_the_build() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "",
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\n"),
        );
        write(
            root.path(),
            "prefabs/tower.rveprefab",
            "[[objects]]\nid = \"tower\"\n[objects.camera]\n",
        );
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(
            error.to_string().contains("/prefabs/tower.rveprefab"),
            "{error}"
        );
    }

    #[test]
    fn heavy_prefab_lite_params_are_checked_against_the_script_schema() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "",
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\n"),
        );
        write(
            root.path(),
            "prefabs/tower.rveprefab",
            &TOWER_BODY.replace("speed = 2", "speed = \"fast\""),
        );
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(
            error.to_string().contains("/prefabs/tower.rveprefab"),
            "{error}"
        );
        assert!(error.to_string().contains("/scripts/spin.lua"), "{error}");
    }

    #[test]
    fn nested_prefab_files_of_a_heavy_prefab_take_its_list_archive() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "",
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\narchive = \"towers\"\n"),
        );
        write(
            root.path(),
            "prefabs/tower.rveprefab",
            "[[assets]]\npath = \"/prefabs/glow.rveprefab\"\n[[objects]]\nid = \"tower\"\n[[instances]]\nid = \"glow\"\nparent = \"tower\"\nprefab = \"/prefabs/glow.rveprefab\"\n",
        );
        write(
            root.path(),
            "prefabs/glow.rveprefab",
            "[[objects]]\nid = \"light\"\n",
        );
        let build = prepare_project(root.path()).unwrap();
        assert_eq!(
            build.assignments[&path("/prefabs/glow.rveprefab")],
            "towers"
        );
    }

    #[test]
    fn missing_heavy_prefab_file_is_an_error() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "",
            Some("[[prefabs]]\npath = \"/prefabs/missing.rveprefab\"\n"),
        );
        let error = prepare_project(root.path()).unwrap_err();
        assert!(error.to_string().contains("missing"), "{error}");
    }

    #[test]
    fn invalid_prefab_list_is_an_input_error() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-heavy").unwrap();
        project(
            root.path(),
            "",
            Some("[[prefabs]]\npath = \"/scenes/main.rvescene\"\n"),
        );
        let error = prepare_project(root.path()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains(".rveprefab"), "{error}");
    }
}

#[cfg(test)]
mod demo_game_tests {
    use super::prepare_project;
    use engine_files::VirtualPath;
    use std::path::Path;

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    #[test]
    fn demo_game_packs_fast_prefabs_with_the_physics_scene_and_lists_the_heavy_prefab() {
        let demo = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/demo-game");
        let build = prepare_project(Path::new(demo)).unwrap();

        // Быстрые префабы объявлены только в physics.rvescene, поэтому живут в её архиве.
        for prefab in [
            "/prefabs/bullet.rveprefab",
            "/prefabs/bullet_hole.rveprefab",
        ] {
            assert_eq!(
                build.assignments[&path(prefab)],
                "scene-physics",
                "{prefab}"
            );
            assert!(build.files.contains_key(&path(prefab)), "{prefab}");
        }
        assert_eq!(
            build.assignments[&path("/scripts/bullet.lua")],
            "scene-physics"
        );
        assert_eq!(
            build.assignments[&path("/scripts/shooter.lua")],
            "scene-physics"
        );

        // Тяжёлый префаб из prefabs.rveprefabs разобран, упакован и не привязан к сцене.
        assert_eq!(build.prefabs.prefabs.len(), 1);
        assert_eq!(build.prefabs.prefabs[0].path, "/prefabs/tower.rveprefab");
        assert_eq!(build.prefabs.prefabs[0].archive, None);
        assert_eq!(
            build.assignments[&path("/prefabs/tower.rveprefab")],
            "shared"
        );
        assert!(build.files.contains_key(&path("/prefabs/tower.rveprefab")));
    }
}
