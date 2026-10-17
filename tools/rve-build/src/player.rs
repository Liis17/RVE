use crate::archives::build_project_archives;
use crate::error::BuildError;
#[cfg(windows)]
use crate::ico::png_to_ico;
use crate::platform;
use crate::project::{ProjectBuild, prepare_project};
use crate::sdk::{self, PlayerLocation};
use crate::temporary::TempDir;
use engine_files::{VirtualPath, strip_windows_verbatim_prefix};
use engine_pak::{Archive, ArchiveCatalog, ArchiveLimits, ArchiveSetRegistry, CatalogLimits};
use engine_script::ScriptLibrary;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) fn build_distribution(
    project_dir: &Path,
    output: &Path,
    vulkan_sdk: Option<&Path>,
) -> Result<(), BuildError> {
    let output = absolute_output_path(output)?;
    ensure_output_missing(&output)?;
    let project = prepare_project(project_dir)?;
    let scripts_library = build_game_scripts(&project, true)?;
    let executable_name = player_file_name(&project.settings.name, &project.settings.id)?;
    #[cfg(target_os = "macos")]
    let vulkan_files = platform::find_macos_vulkan_files(vulkan_sdk)?;
    #[cfg(not(target_os = "macos"))]
    let _ = vulkan_sdk;

    let executable = locate_player()?;

    let output_parent = output.parent().ok_or_else(|| {
        BuildError::input(format!(
            "output должен называть каталог: {}",
            output.display()
        ))
    })?;
    fs::create_dir_all(output_parent).map_err(|error| {
        BuildError::operational(format!(
            "не удалось создать родительский каталог output {}: {error}",
            output_parent.display()
        ))
    })?;
    let output_parent = fs::canonicalize(output_parent).map_err(|error| {
        BuildError::operational(format!(
            "не удалось определить каталог output {}: {error}",
            output.display()
        ))
    })?;
    let output_parent = normalize_build_path(&output_parent)?;
    let output_name = output
        .file_name()
        .ok_or_else(|| BuildError::input("output должен иметь имя каталога"))?;
    let output = normalize_build_path(&output_parent.join(output_name))?;
    ensure_output_missing(&output)?;

    let staging = TempDir::create(&output_parent, "rve-build-output")?;
    #[cfg(target_os = "macos")]
    let (resources_dir, binary_dir) = {
        let contents = staging
            .path()
            .join(format!("{executable_name}.app"))
            .join("Contents");
        create_dir_all(&contents.join("MacOS"))?;
        let settings = &project.settings;
        let plist = platform::info_plist(
            &executable_name,
            &settings.name,
            &settings.id,
            &settings.version,
        );
        write_file(&contents.join("Info.plist"), plist.as_bytes())?;
        (contents.join("Resources"), contents.join("MacOS"))
    };
    #[cfg(not(target_os = "macos"))]
    let (resources_dir, binary_dir) = (staging.path().to_owned(), staging.path().to_owned());
    let engine_dir = resources_dir.join("Engine");
    create_dir_all(&engine_dir)?;
    copy_file(&project.bootstrap_path, &engine_dir.join("bootstrap.rves"))?;
    if let Some(library) = &scripts_library {
        copy_file(library, &engine_dir.join(scripts_library_filename()))?;
    }
    let assets_dir = resources_dir.join("Assets");
    build_project_archives(&project, &assets_dir)?;
    validate_staged_archives(&assets_dir)?;

    #[cfg(target_os = "macos")]
    platform::copy_macos_vulkan_files(&vulkan_files, &engine_dir)?;

    let output_binary = binary_dir.join(&executable_name);
    copy_file(&executable, &output_binary)?;
    #[cfg(windows)]
    if let Some(icon_path) = &project.icon {
        let source = project
            .files
            .get(icon_path)
            .ok_or_else(|| BuildError::input(format!("иконка /{icon_path} не найдена")))?;
        let png = fs::read(source).map_err(|error| {
            BuildError::operational(format!("не удалось прочитать иконку /{icon_path}: {error}"))
        })?;
        let ico = png_to_ico(&png).map_err(|error| {
            BuildError::input(format!("ошибка Windows-иконки /{icon_path}: {error}"))
        })?;
        platform::patch_executable_icon(&output_binary, &ico)?;
    }

    match platform::publish_directory_no_replace(staging.path(), &output) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(BuildError::input(
            format!("output directory already exists: {}", output.display()),
        )),
        Err(error) => Err(BuildError::operational(format!(
            "не удалось опубликовать {}: {error}",
            output.display()
        ))),
    }
}

pub(crate) fn run_project(project_dir: &Path, player_args: &[OsString]) -> Result<i32, BuildError> {
    let project = prepare_project(project_dir)?;
    let scripts_library = build_game_scripts(&project, false)?;
    let executable = locate_player()?;
    let project_dir = normalize_build_path(&project.project_dir)?;
    let mut command = Command::new(&executable);
    command.arg("--project").arg(project_dir);
    if let Some(library) = scripts_library {
        command.arg("--scripts").arg(library);
    }
    let status = command.args(player_args).status().map_err(|error| {
        BuildError::operational(format!(
            "не удалось запустить rve-player {}: {error}",
            executable.display()
        ))
    })?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(windows)]
fn scripts_library_filename() -> &'static str {
    "game_scripts.dll"
}

#[cfg(target_os = "macos")]
fn scripts_library_filename() -> &'static str {
    "libgame_scripts.dylib"
}

#[cfg(not(any(windows, target_os = "macos")))]
fn scripts_library_filename() -> &'static str {
    "libgame_scripts.so"
}

fn build_game_scripts(
    project: &ProjectBuild,
    release: bool,
) -> Result<Option<PathBuf>, BuildError> {
    let Some(manifest_path) = heavy_scripts_manifest(project)? else {
        return Ok(None);
    };
    let project_root = normalize_build_path(&project.project_dir)?;
    let scripts_dir = manifest_path.parent().unwrap_or(&project_root);
    let target = cargo_cdylib_target(&manifest_path)?;
    let target_dir = normalize_build_path(&project_root.join(".rve/target"))?;
    create_dir_all(&target_dir)?;

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));

    let mut command = Command::new(&cargo);
    command
        .arg("build")
        .arg("--lib")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--message-format=json")
        .current_dir(scripts_dir)
        .env("CARGO_TARGET_DIR", &target_dir);
    if release {
        command.arg("--release");
    }
    let stdout = run_cargo(
        &mut command,
        "cargo build для heavy_scripts",
        CargoFailure::Project,
    )?;
    let artifact = parse_cargo_output(
        &stdout,
        &target.target_name,
        "cdylib",
        Some(&target.package_id),
    )
    .map_err(|message| {
        BuildError::operational(format!(
            "не удалось найти собранную библиотеку скриптов: {message}"
        ))
    })?;
    let extension = scripts_library_extension();
    let library_path = select_library_file(&artifact.filenames, extension).ok_or_else(|| {
        BuildError::operational(format!(
            "cargo не сообщил файл cdylib с расширением .{extension} для {}",
            target.target_name
        ))
    })?;
    let library_path = normalize_build_path(library_path)?;

    // SAFETY: building and loading this library is required to validate the project's script ABI.
    let library = unsafe { ScriptLibrary::load(&library_path) }.map_err(|error| {
        BuildError::input(format!(
            "не удалось загрузить библиотеку heavy_scripts {}: {error}",
            library_path.display()
        ))
    })?;
    validate_heavy_script_names(&library, &project.heavy_scripts)?;
    drop(library);
    Ok(Some(library_path))
}

fn select_library_file<'a>(filenames: &'a [PathBuf], extension: &str) -> Option<&'a PathBuf> {
    filenames.iter().find(|path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
    })
}

fn validate_heavy_script_names(
    library: &ScriptLibrary,
    heavy_scripts: &[(String, String)],
) -> Result<(), BuildError> {
    for (scene, name) in heavy_scripts {
        if !library.contains_script(name) {
            return Err(BuildError::input(format!(
                "сцена /{scene} ссылается на heavy-скрипт {name:?}, которого нет в библиотеке"
            )));
        }
    }
    Ok(())
}

fn heavy_scripts_manifest(project: &ProjectBuild) -> Result<Option<PathBuf>, BuildError> {
    let Some(relative) = project.settings.heavy_scripts.as_deref() else {
        return Ok(None);
    };
    let project_root = normalize_build_path(&project.project_dir)?;
    let path = project_root.join(relative);
    let path = fs::canonicalize(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BuildError::input(format!(
                "heavy_scripts каталог не найден: {}",
                path.display()
            ))
        } else {
            BuildError::operational(format!(
                "не удалось прочитать heavy_scripts {}: {error}",
                path.display()
            ))
        }
    })?;
    let path = normalize_build_path(&path)?;
    if !path.starts_with(&project_root) || !path.is_dir() {
        return Err(BuildError::input(format!(
            "heavy_scripts должен указывать на каталог внутри проекта: {}",
            path.display()
        )));
    }
    let manifest_path = path.join("Cargo.toml");
    let manifest_path = fs::canonicalize(&manifest_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BuildError::input(format!(
                "в heavy_scripts отсутствует {}",
                manifest_path.display()
            ))
        } else {
            BuildError::operational(format!(
                "не удалось прочитать {}: {error}",
                manifest_path.display()
            ))
        }
    })?;
    normalize_build_path(&manifest_path).map(Some)
}

fn scripts_library_extension() -> &'static str {
    #[cfg(windows)]
    {
        "dll"
    }
    #[cfg(target_os = "macos")]
    {
        "dylib"
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        "so"
    }
}

#[derive(Debug)]
struct CdylibTarget {
    package_id: String,
    target_name: String,
}

fn cargo_cdylib_target(manifest_path: &Path) -> Result<CdylibTarget, BuildError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let stdout = run_cargo(
        Command::new(cargo)
            .arg("metadata")
            .arg("--no-deps")
            .arg("--format-version")
            .arg("1")
            .arg("--manifest-path")
            .arg(manifest_path)
            .current_dir(manifest_path.parent().unwrap_or(Path::new("."))),
        "cargo metadata heavy_scripts",
        CargoFailure::Project,
    )?;
    parse_cdylib_target(&stdout, manifest_path)
        .map_err(|message| BuildError::input(format!("heavy_scripts: {message}")))
}

fn parse_cdylib_target(stdout: &str, manifest_path: &Path) -> Result<CdylibTarget, String> {
    let metadata: JsonValue = serde_json::from_str(stdout)
        .map_err(|error| format!("некорректный cargo metadata: {error}"))?;
    let packages = metadata
        .get("packages")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "cargo metadata не содержит packages".to_owned())?;
    let package = packages
        .iter()
        .find(|package| {
            package
                .get("manifest_path")
                .and_then(JsonValue::as_str)
                .is_some_and(|path| Path::new(path) == manifest_path)
        })
        .ok_or_else(|| {
            format!(
                "cargo metadata не содержит пакет из {}",
                manifest_path.display()
            )
        })?;
    let package_id = package
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "cargo metadata не содержит package id".to_owned())?
        .to_owned();
    let targets = package
        .get("targets")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "cargo metadata не содержит targets".to_owned())?;
    let target = targets.iter().find(|target| {
        target
            .get("kind")
            .and_then(JsonValue::as_array)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("cdylib")))
    });
    let Some(target) = target else {
        return Err(format!(
            "{} должен содержать lib-target с crate-type = [\"cdylib\"]",
            manifest_path.display()
        ));
    };
    let target_name = target
        .get("name")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "cdylib target не имеет имени".to_owned())?
        .to_owned();
    Ok(CdylibTarget {
        package_id,
        target_name,
    })
}

struct CargoArtifact {
    executable: Option<PathBuf>,
    filenames: Vec<PathBuf>,
    fresh: bool,
}

fn parse_cargo_output(
    stdout: &str,
    target_name: &str,
    kind: &str,
    package_id: Option<&str>,
) -> Result<CargoArtifact, String> {
    let mut artifact = None;
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<JsonValue>(line) else {
            continue;
        };
        if message.get("reason").and_then(JsonValue::as_str) != Some("compiler-artifact") {
            continue;
        }
        if package_id.is_some_and(|expected| {
            message.get("package_id").and_then(JsonValue::as_str) != Some(expected)
        }) {
            continue;
        }
        let Some(target) = message.get("target") else {
            continue;
        };
        let matching_target = target.get("name").and_then(JsonValue::as_str) == Some(target_name)
            && target
                .get("kind")
                .and_then(JsonValue::as_array)
                .is_some_and(|kinds| kinds.iter().any(|value| value.as_str() == Some(kind)));
        if !matching_target {
            continue;
        }
        let filenames = message
            .get("filenames")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(JsonValue::as_str)
            .map(PathBuf::from)
            .collect();
        artifact = Some(CargoArtifact {
            executable: message
                .get("executable")
                .and_then(JsonValue::as_str)
                .map(PathBuf::from),
            filenames,
            fresh: message
                .get("fresh")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
        });
    }
    artifact
        .ok_or_else(|| format!("cargo output не содержит target {target_name:?} с kind {kind:?}"))
}

fn locate_player() -> Result<PathBuf, BuildError> {
    let executable = current_executable_path()?;
    let executable_dir = executable_directory(&executable)?;
    match sdk::locate_player_in(&executable_dir, build_checkout_player)? {
        PlayerLocation::Checkout(executable) => Ok(executable),
        PlayerLocation::Sdk {
            executable,
            manifest,
        } => {
            println!("rve-player: prebuilt (SDK)");
            println!("rve-sdk engine version: {}", manifest.engine_version);
            Ok(executable)
        }
    }
}

pub(crate) fn current_executable_path() -> Result<PathBuf, BuildError> {
    let executable = std::env::current_exe().map_err(|error| {
        BuildError::operational(format!("не удалось определить путь к rve-build: {error}"))
    })?;
    canonical_executable_path(&executable)
}

fn canonical_executable_path(executable: &Path) -> Result<PathBuf, BuildError> {
    let executable = fs::canonicalize(executable).map_err(|error| {
        BuildError::operational(format!(
            "не удалось разрешить путь к rve-build {}: {error}",
            executable.display()
        ))
    })?;
    normalize_build_path(&executable).map_err(|error| {
        BuildError::operational(format!(
            "не удалось нормализовать путь к rve-build: {error}"
        ))
    })
}

pub(crate) fn executable_directory(executable: &Path) -> Result<PathBuf, BuildError> {
    executable.parent().map(Path::to_path_buf).ok_or_else(|| {
        BuildError::operational("не удалось определить каталог исполняемого файла rve-build")
    })
}

pub(crate) fn build_checkout_player() -> Result<PathBuf, BuildError> {
    let root = repo_root()?;
    let target_dir = normalize_build_path(&root.join("build/player"))?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let stdout = run_cargo(
        Command::new(cargo)
            .arg("build")
            .arg("--release")
            .arg("--locked")
            .arg("-p")
            .arg("rve-player")
            .arg("--message-format=json")
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &target_dir),
        "сборка rve-player",
        CargoFailure::Operational,
    )?;
    let artifact = parse_cargo_output(&stdout, "rve-player", "bin", None)
        .map_err(|message| BuildError::operational(format!("сборка rve-player: {message}")))?;
    let executable = artifact.executable.ok_or_else(|| {
        BuildError::operational("cargo не сообщил executable для бинарника rve-player")
    })?;
    println!(
        "rve-player: {}",
        if artifact.fresh {
            "up to date"
        } else {
            "rebuilt"
        }
    );
    normalize_build_path(&executable)
        .map_err(|error| BuildError::operational(format!("сборка rve-player: {error}")))
}

#[derive(Clone, Copy)]
enum CargoFailure {
    Project,
    Operational,
}

impl CargoFailure {
    fn build_error(self, message: String) -> BuildError {
        match self {
            Self::Project => BuildError::input(message),
            Self::Operational => BuildError::operational(message),
        }
    }
}

fn run_cargo(
    command: &mut Command,
    context: &str,
    failure: CargoFailure,
) -> Result<String, BuildError> {
    command.stdout(Stdio::piped()).stderr(Stdio::inherit());
    let output = command.output().map_err(|error| {
        BuildError::operational(format!("не удалось запустить {context}: {error}"))
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    display_cargo_output(&stdout);
    if !output.status.success() {
        return Err(failure.build_error(format!(
            "{context} завершился с кодом {}",
            output.status.code().unwrap_or(1)
        )));
    }
    Ok(stdout)
}

fn display_cargo_output(stdout: &str) {
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<JsonValue>(line) else {
            println!("{line}");
            continue;
        };
        if message.get("reason").and_then(JsonValue::as_str) == Some("compiler-message")
            && let Some(rendered) = message
                .get("message")
                .and_then(|message| message.get("rendered"))
                .and_then(JsonValue::as_str)
        {
            eprint!("{rendered}");
        }
    }
}

pub(crate) fn validate_staged_archives(assets_dir: &Path) -> Result<(), BuildError> {
    let registry_path = VirtualPath::parse("/rve/assets.toml").expect("valid registry path");
    let base_path = assets_dir.join("base.rvepak");
    let base = Archive::open_file(&base_path).map_err(|error| {
        BuildError::operational(format!(
            "не удалось открыть staging base.rvepak {}: {error}",
            base_path.display()
        ))
    })?;
    let registry_bytes = base.read_all(&registry_path).map_err(|error| {
        BuildError::operational(format!(
            "не удалось самостоятельно прочитать /rve/assets.toml из base.rvepak: {error}"
        ))
    })?;
    drop(base);
    let registry_text = String::from_utf8(registry_bytes).map_err(|error| {
        BuildError::operational(format!(
            "staging /rve/assets.toml должен быть UTF-8: {error}"
        ))
    })?;
    let registry = ArchiveSetRegistry::parse(&registry_text).map_err(|error| {
        BuildError::operational(format!("некорректный staging /rve/assets.toml: {error}"))
    })?;

    let catalog = ArchiveCatalog::open_directory(
        assets_dir,
        ArchiveLimits::default(),
        CatalogLimits::default(),
    )
    .map_err(|error| {
        BuildError::operational(format!(
            "не удалось открыть staging-каталог архивов {}: {error}",
            assets_dir.display()
        ))
    })?;
    let archive_ids = catalog
        .archive_paths()
        .map(|(id, path)| {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| {
                    BuildError::operational(format!(
                        "имя staging-архива должно быть UTF-8: {}",
                        path.display()
                    ))
                })?;
            Ok((name.to_owned(), id))
        })
        .collect::<Result<BTreeMap<_, _>, BuildError>>()?;
    registry
        .validate_against_catalog(&catalog, &archive_ids, assets_dir)
        .map_err(|error| {
            BuildError::operational(format!("staging-набор архивов не прошёл проверку: {error}"))
        })
}

pub(crate) fn repo_root() -> Result<PathBuf, BuildError> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| BuildError::operational("не удалось определить корень checkout RVE"))?;
    let root = fs::canonicalize(&root).map_err(|error| {
        BuildError::operational(format!(
            "не удалось определить корень checkout RVE {}: {error}",
            root.display()
        ))
    })?;
    normalize_build_path(&root)
}

pub(crate) fn create_dir_all(path: &Path) -> Result<(), BuildError> {
    fs::create_dir_all(path).map_err(|error| {
        BuildError::operational(format!("не удалось создать {}: {error}", path.display()))
    })
}

pub(crate) fn copy_file(source: &Path, destination: &Path) -> Result<(), BuildError> {
    fs::copy(source, destination).map_err(|error| {
        BuildError::operational(format!(
            "не удалось скопировать {} в {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn write_file(path: &Path, contents: &[u8]) -> Result<(), BuildError> {
    match fs::read(path) {
        Ok(existing) if existing == contents => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(BuildError::operational(format!(
                "не удалось прочитать {} перед записью: {error}",
                path.display()
            )));
        }
    }
    fs::write(path, contents).map_err(|error| {
        BuildError::operational(format!("не удалось записать {}: {error}", path.display()))
    })
}

pub(crate) fn absolute_output_path(path: &Path) -> Result<PathBuf, BuildError> {
    let path = std::path::absolute(path).map_err(|error| {
        BuildError::operational(format!(
            "не удалось разрешить output {}: {error}",
            path.display()
        ))
    })?;
    let path = normalize_build_path(&path)?;
    if path.file_name().is_none() {
        return Err(BuildError::input(format!(
            "output должен называть новый каталог: {}",
            path.display()
        )));
    }
    Ok(path)
}

pub(crate) fn ensure_output_missing(path: &Path) -> Result<(), BuildError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(BuildError::input(format!(
            "output directory already exists: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BuildError::operational(format!(
            "не удалось проверить output {}: {error}",
            path.display()
        ))),
    }
}

pub(crate) fn player_file_name(name: &str, id: &str) -> Result<String, BuildError> {
    let mut characters = name
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<Vec<_>>();
    for character in characters.iter_mut().rev() {
        if matches!(character, '.' | ' ') {
            *character = '_';
        } else {
            break;
        }
    }
    let sanitized = characters.into_iter().collect::<String>();
    let stem = if sanitized.is_empty() { id } else { &sanitized };
    let windows_device_stem = stem
        .split('.')
        .next()
        .unwrap_or(stem)
        .trim_end_matches([' ', '.']);
    if stem.eq_ignore_ascii_case("engine")
        || stem.eq_ignore_ascii_case("assets")
        || is_reserved_windows_name(windows_device_stem)
    {
        return Err(BuildError::input(format!(
            "имя исполняемого файла {stem:?} зарезервировано или совпадает с каталогом Engine/Assets"
        )));
    }
    Ok(if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    })
}

fn is_reserved_windows_name(stem: &str) -> bool {
    let device = stem.to_ascii_uppercase();
    matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            device.strip_prefix(prefix).is_some_and(|number| {
                number.len() == 1 && (b'1'..=b'9').contains(&number.as_bytes()[0])
            })
        })
}

pub(crate) fn normalize_build_path(path: &Path) -> Result<PathBuf, BuildError> {
    strip_windows_verbatim_prefix(path).map_err(|error| {
        BuildError::input(format!(
            "неподдерживаемая форма Windows-пути {}: {error}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CargoArtifact, CargoFailure, build_distribution, heavy_scripts_manifest,
        parse_cargo_output, parse_cdylib_target, player_file_name, select_library_file,
        validate_heavy_script_names, validate_staged_archives,
    };
    use crate::error::BuildError;
    use crate::project::ProjectBuild;
    use crate::temporary::TempDir;
    use engine_files::strip_windows_verbatim_prefix;
    use engine_project::ProjectSettings;
    use engine_script::ScriptLibrary;
    use rve_sdk::{Params, Script};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct TestScript;

    impl Script for TestScript {}

    fn test_factory(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(TestScript))
    }

    fn test_library() -> ScriptLibrary {
        let module = rve_sdk::script_module! {
            "Bobber" => test_factory,
            "Spinner" => test_factory,
        };
        // SAFETY: script_module! stores the descriptors and names in static storage.
        unsafe { ScriptLibrary::from_module(module) }.unwrap()
    }

    fn sample_project(project_dir: &Path, heavy_scripts: Option<&str>) -> ProjectBuild {
        let mut settings = ProjectSettings::parse(
            "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n[[scenes]]\npath=\"scene.rvescene\"\n",
        )
        .unwrap();
        settings.heavy_scripts = heavy_scripts.map(str::to_owned);
        ProjectBuild {
            project_dir: project_dir.to_path_buf(),
            bootstrap_path: project_dir.join("bootstrap.rves"),
            settings,
            files: BTreeMap::new(),
            assignments: BTreeMap::new(),
            archives: vec!["base".to_owned()],
            #[cfg(windows)]
            icon: None,
            heavy_scripts: Vec::new(),
        }
    }

    #[test]
    fn sanitizes_invalid_filename_characters_and_trailing_spaces_or_dots() {
        assert_eq!(
            player_file_name("RVE: Demo. ", "demo").unwrap(),
            if cfg!(windows) {
                "RVE_ Demo__.exe"
            } else {
                "RVE_ Demo__"
            }
        );
        assert_eq!(
            player_file_name("///", "demo").unwrap(),
            "___".to_owned() + if cfg!(windows) { ".exe" } else { "" }
        );
        assert_eq!(
            player_file_name("", "demo").unwrap(),
            if cfg!(windows) { "demo.exe" } else { "demo" }
        );
        assert_eq!(
            player_file_name("Game\u{1}", "demo").unwrap(),
            if cfg!(windows) { "Game_.exe" } else { "Game_" }
        );
    }

    #[test]
    fn rejects_reserved_and_distribution_directory_names() {
        for name in [
            "Engine", "eNgInE", "Assets", "aSSeTs", "CON", "nul.txt", "PRN", "AUX",
        ] {
            let error = player_file_name(name, "fallback").unwrap_err();
            assert_eq!(error.exit_code(), 2, "{name}");
        }
        for prefix in ["COM", "LPT"] {
            for digit in 1..=9 {
                let name = format!("{prefix}{digit}");
                assert_eq!(
                    player_file_name(&name, "fallback").unwrap_err().exit_code(),
                    2
                );
            }
        }
        assert!(player_file_name("COM0", "fallback").is_ok());
    }

    #[test]
    fn reads_the_package_id_and_renamed_cdylib_target_from_metadata() {
        let manifest = Path::new("C:/Games/Sample/HeavyScripts/Cargo.toml");
        let metadata = serde_json::json!({
            "packages": [{
                "id": "path+file:///HeavyScripts#heavy@0.1.0",
                "manifest_path": manifest,
                "targets": [{"name": "custom_scripts", "kind": ["cdylib"]}]
            }]
        });
        let target = parse_cdylib_target(&metadata.to_string(), manifest).unwrap();
        assert_eq!(target.package_id, "path+file:///HeavyScripts#heavy@0.1.0");
        assert_eq!(target.target_name, "custom_scripts");
    }

    #[test]
    fn rejects_a_heavy_scripts_package_without_a_cdylib_target() {
        let manifest = Path::new("C:/Games/Sample/HeavyScripts/Cargo.toml");
        let metadata = serde_json::json!({
            "packages": [{
                "id": "heavy",
                "manifest_path": manifest,
                "targets": [{"name": "heavy", "kind": ["rlib"]}]
            }]
        });
        let error = parse_cdylib_target(&metadata.to_string(), manifest).unwrap_err();
        assert!(error.contains("crate-type"));
    }

    #[test]
    fn cargo_artifact_selection_ignores_other_packages_and_uses_dylib_filename() {
        let output = concat!(
            "plain cargo output\n",
            "{\"reason\":\"compiler-artifact\",\"package_id\":\"dependency 1.0.0\",\"target\":{\"name\":\"scripts\",\"kind\":[\"cdylib\"]},\"filenames\":[\"/tmp/dependency.dll\"],\"fresh\":true}\n",
            "{\"reason\":\"compiler-artifact\",\"package_id\":\"game 1.0.0\",\"target\":{\"name\":\"scripts\",\"kind\":[\"lib\",\"cdylib\"]},\"filenames\":[\"/tmp/libscripts.rlib\",\"/tmp/libscripts.dll\",\"/tmp/libscripts.pdb\"],\"fresh\":false}\n",
            "{malformed\n"
        );
        let artifact = parse_cargo_output(output, "scripts", "cdylib", Some("game 1.0.0")).unwrap();
        assert_eq!(
            artifact.filenames,
            [
                PathBuf::from("/tmp/libscripts.rlib"),
                PathBuf::from("/tmp/libscripts.dll"),
                PathBuf::from("/tmp/libscripts.pdb"),
            ]
        );
        assert!(!artifact.fresh);
        assert!(
            parse_cargo_output(output, "scripts", "cdylib", Some("dependency 1.0.0"))
                .unwrap()
                .filenames[0]
                .ends_with("dependency.dll")
        );
        assert!(parse_cargo_output(output, "other", "bin", None).is_err());
    }

    #[test]
    fn selects_the_platform_library_from_multiple_compiler_artifact_filenames() {
        let extension = super::scripts_library_extension();
        let artifact = CargoArtifact {
            executable: None,
            filenames: [
                "game_scripts.rlib".into(),
                format!("game_scripts.{extension}").into(),
                "game_scripts.pdb".into(),
            ]
            .into(),
            fresh: false,
        };
        let selected = select_library_file(&artifact.filenames, extension);
        assert_eq!(selected, Some(&artifact.filenames[1]));
    }

    #[test]
    fn validates_all_scene_heavy_script_names_against_the_library() {
        let library = test_library();
        let scripts = [
            ("scenes/main.rvescene".to_owned(), "Bobber".to_owned()),
            ("scenes/side.rvescene".to_owned(), "Spinner".to_owned()),
        ];

        assert!(validate_heavy_script_names(&library, &scripts).is_ok());
    }

    #[test]
    fn missing_scene_heavy_script_is_a_project_error_with_scene_and_name() {
        let library = test_library();
        let error = validate_heavy_script_names(
            &library,
            &[("scenes/main.rvescene".to_owned(), "Missing".to_owned())],
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("scenes/main.rvescene"));
        assert!(error.to_string().contains("Missing"));
    }

    #[test]
    fn cargo_failure_categories_keep_project_errors_and_report_player_errors_as_operational() {
        assert_eq!(
            CargoFailure::Project
                .build_error("game script compile failed".to_owned())
                .exit_code(),
            2
        );
        assert_eq!(
            CargoFailure::Operational
                .build_error("rve-player compile failed".to_owned())
                .exit_code(),
            1
        );
    }

    #[test]
    fn rejects_heavy_script_paths_that_escape_the_project() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-heavy-paths").unwrap();
        let project_dir = root.path().join("project");
        let outside = root.path().join("outside");
        fs::create_dir_all(&project_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join("Cargo.toml"),
            "[package]\nname=\"outside-heavy\"\n",
        )
        .unwrap();

        for value in ["../outside", outside.to_str().unwrap()] {
            let project = sample_project(&project_dir, Some(value));
            let error = heavy_scripts_manifest(&project).unwrap_err();
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_heavy_script_symlink_that_escapes_the_project() {
        use std::os::unix::fs::symlink;

        let root = TempDir::create(&std::env::temp_dir(), "rve-heavy-symlink").unwrap();
        let project_dir = root.path().join("project");
        let outside = root.path().join("outside");
        fs::create_dir_all(&project_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join("Cargo.toml"),
            "[package]\nname=\"outside-heavy\"\n",
        )
        .unwrap();
        symlink(&outside, project_dir.join("linked-heavy")).unwrap();

        let project = sample_project(&project_dir, Some("linked-heavy"));
        let error = heavy_scripts_manifest(&project).unwrap_err();
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn existing_output_fails_before_touching_project_or_output_contents() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-output-exists").unwrap();
        let output = root.path().join("existing-output");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("marker"), b"keep").unwrap();
        let error =
            build_distribution(&root.path().join("missing-project"), &output, None).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert_eq!(fs::read(output.join("marker")).unwrap(), b"keep");
        assert!(!root.path().join("missing-project/.rve").exists());
    }

    #[test]
    fn staged_archive_validation_errors_are_operational_and_clean_up_staging() {
        let parent = TempDir::create(&std::env::temp_dir(), "rve-build-invalid-stage").unwrap();
        let mut staging_path = None;
        let result = (|| -> Result<(), BuildError> {
            let staging = TempDir::create(parent.path(), "rve-build-output-test")?;
            staging_path = Some(staging.path().to_path_buf());
            let assets = staging.path().join("Assets");
            fs::create_dir(&assets).unwrap();
            validate_staged_archives(&assets)?;
            Ok(())
        })();

        assert_eq!(result.unwrap_err().exit_code(), 1);
        assert!(!staging_path.unwrap().exists());
    }

    #[test]
    fn repo_root_is_canonical_and_normalized() {
        let path = super::repo_root().unwrap();
        let canonical = fs::canonicalize(&path).unwrap();
        assert_eq!(strip_windows_verbatim_prefix(&canonical).unwrap(), path);
    }

    #[test]
    fn executable_directory_uses_a_canonicalized_path() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-executable-path").unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        let executable_path = root.path().join("nested/../rve-build");
        fs::write(root.path().join("rve-build"), b"stub").unwrap();

        let canonical = super::canonical_executable_path(&executable_path).unwrap();
        let directory = super::executable_directory(&canonical).unwrap();
        let expected =
            strip_windows_verbatim_prefix(&fs::canonicalize(root.path()).unwrap()).unwrap();

        assert_eq!(directory, expected);
    }

    #[test]
    fn unchanged_generated_file_keeps_its_modification_time() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-player-mtime").unwrap();
        let path = root.path().join("generated");
        super::write_file(&path, b"same content").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        super::write_file(&path, b"same content").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }
}
