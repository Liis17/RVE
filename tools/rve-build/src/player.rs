use crate::archives::build_project_archives;
use crate::error::BuildError;
#[cfg(windows)]
use crate::ico::png_to_ico;
use crate::platform;
use crate::project::{HeavyScriptUse, ProjectBuild, prepare_project, validate_script_references};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlayerFlavor {
    Distribution,
    Dev,
    /// Выделенный сервер `rve-server`: release без функций, без DLSS.
    Server,
}

impl PlayerFlavor {
    fn target_directory(self) -> &'static str {
        match self {
            Self::Distribution => "player",
            Self::Dev => "player-dev",
            Self::Server => "server",
        }
    }

    /// Пакет Cargo, который собирает этот вариант.
    pub(crate) fn package(self) -> &'static str {
        match self {
            Self::Server => "rve-server",
            Self::Distribution | Self::Dev => "rve-player",
        }
    }

    fn cargo_arguments(self, dlss: bool) -> Vec<&'static str> {
        let mut arguments = vec!["build"];
        if self != Self::Dev {
            arguments.push("--release");
        }
        arguments.extend(["--locked", "-p", self.package()]);
        if self != Self::Dev {
            arguments.push("--no-default-features");
        }
        if dlss {
            arguments.extend(["--features", "dlss"]);
        }
        arguments.push("--message-format=json");
        arguments
    }
}

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

    #[cfg(windows)]
    let executable = locate_player(
        PlayerFlavor::Distribution,
        project.settings.runtime.dlss_library.is_some(),
    )?;
    #[cfg(not(windows))]
    let executable = locate_player(PlayerFlavor::Distribution, false)?;
    let server_executable = locate_player(PlayerFlavor::Server, false)?;

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
    #[cfg(windows)]
    copy_fsr_runtime(&project, &engine_dir)?;
    #[cfg(windows)]
    copy_dlss_runtime(&project, &engine_dir)?;
    let assets_dir = resources_dir.join("Assets");
    build_project_archives(&project, &assets_dir)?;
    validate_staged_archives(&assets_dir)?;

    #[cfg(target_os = "macos")]
    platform::copy_macos_vulkan_files(&vulkan_files, &engine_dir)?;

    let output_binary = binary_dir.join(&executable_name);
    copy_file(&executable, &output_binary)?;
    // Сервер лежит рядом с Engine (Windows) или рядом с `.app` (macOS), а не внутри бандла: без окна.
    copy_file(
        &server_executable,
        &staging
            .path()
            .join(sdk::player_filename(PlayerFlavor::Server)),
    )?;
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
    #[cfg(windows)]
    let executable = locate_player(
        PlayerFlavor::Dev,
        project.settings.runtime.dlss_library.is_some(),
    )?;
    #[cfg(not(windows))]
    let executable = locate_player(PlayerFlavor::Dev, false)?;
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

pub(crate) fn scripts_project(project: &Path) -> Result<Option<PathBuf>, BuildError> {
    build_game_scripts(&prepare_project(project)?, false)
}

#[cfg(target_os = "macos")]
fn ensure_command_line_tools() -> Result<(), BuildError> {
    let status = Command::new("xcode-select")
        .arg("-p")
        .status()
        .map_err(|e| BuildError::operational(format!("xcode-select: {e}")))?;
    if status.success() {
        return Ok(());
    }
    Command::new("xcode-select")
        .arg("--install")
        .status()
        .map_err(|e| BuildError::operational(format!("xcode-select --install: {e}")))?;
    Err(BuildError::operational(
        "Установите Command Line Tools в открывшемся диалоге и повторите сборку",
    ))
}

fn script_cargo_command(project: &Path, toolchain: Option<&Path>) -> Result<Command, BuildError> {
    let Some(toolchain) = toolchain else {
        return Ok(Command::new(
            std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        ));
    };
    let bin = toolchain.join("bin");
    let mut command = Command::new(bin.join(format!("cargo{}", std::env::consts::EXE_SUFFIX)));
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    command
        .env(
            "RUSTC",
            bin.join(format!("rustc{}", std::env::consts::EXE_SUFFIX)),
        )
        .env("CARGO_HOME", project.join(".rve/cargo-home"))
        .env(
            "PATH",
            std::env::join_paths(paths).map_err(|e| BuildError::operational(e.to_string()))?,
        )
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_BOOTSTRAP")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    #[cfg(windows)]
    {
        let target = format!("{}-pc-windows-gnu", std::env::consts::ARCH);
        let linker = toolchain
            .join("lib/rustlib")
            .join(target)
            .join("bin/rust-lld.exe");
        if linker.is_file() {
            let rustflags = [
                "-Zunstable-options".to_owned(),
                "-Clinker-flavor=gnu-lld".to_owned(),
                format!("-Clinker={}", linker.display()),
                "-Clink-self-contained=yes".to_owned(),
                "-Ctarget-feature=+crt-static".to_owned(),
            ]
            .join("\u{1f}");
            command
                .env("RUSTC_BOOTSTRAP", "1")
                .env("CARGO_ENCODED_RUSTFLAGS", rustflags);
        }
    }
    Ok(command)
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
    let exe = current_executable_path()?;
    let exe_dir = executable_directory(&exe)?;
    let sdk_root = if exe_dir.join("rve-sdk.toml").is_file() {
        exe_dir
    } else {
        repo_root()?.join("engine")
    };
    sdk::sync_guest_sdk(
        &sdk_root.join("rve-sdk"),
        &project_root.join(".rve/sdk/rve-sdk"),
    )?;
    #[cfg(target_os = "macos")]
    ensure_command_line_tools()?;
    let toolchain = sdk_root.join("toolchain");
    let bundled = toolchain.is_dir().then_some(toolchain.as_path());
    let target = cargo_cdylib_target(&manifest_path, &project_root, bundled)?;
    let target_dir = normalize_build_path(&project_root.join(".rve/target"))?;
    create_dir_all(&target_dir)?;

    let mut command = script_cargo_command(&project_root, bundled)?;
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
    validate_heavy_scripts(&library, &project.heavy_scripts)?;
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

/// Имена heavy-скриптов сцен есть в библиотеке, а их `params` проходят схему скрипта.
fn validate_heavy_scripts(
    library: &ScriptLibrary,
    heavy_scripts: &[HeavyScriptUse],
) -> Result<(), BuildError> {
    for script in heavy_scripts {
        if !library.contains_script(&script.name) {
            return Err(BuildError::input(format!(
                "{} ссылается на heavy-скрипт {:?}, которого нет в библиотеке",
                script.owner, script.name
            )));
        }
        if let Some(schema) = library.schema(&script.name) {
            let resolved = schema.resolve(&script.params).map_err(|error| {
                BuildError::input(format!(
                    "{}, объект {:?}, heavy-скрипт {:?}: {error}",
                    script.owner, script.object, script.name
                ))
            })?;
            validate_script_references(
                schema,
                &resolved,
                &script.references.objects,
                &script.references.assets,
            )
            .map_err(|error| {
                BuildError::input(format!(
                    "{}, объект {:?}, heavy-скрипт {:?}: {error}",
                    script.owner, script.object, script.name
                ))
            })?;
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

fn cargo_cdylib_target(
    manifest_path: &Path,
    project: &Path,
    toolchain: Option<&Path>,
) -> Result<CdylibTarget, BuildError> {
    let stdout = run_cargo(
        script_cargo_command(project, toolchain)?
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

fn locate_player(flavor: PlayerFlavor, dlss: bool) -> Result<PathBuf, BuildError> {
    let executable = current_executable_path()?;
    let executable_dir = executable_directory(&executable)?;
    match sdk::locate_player_in_with_dlss(&executable_dir, flavor, dlss, |flavor| {
        build_checkout_player(flavor, dlss)
    })? {
        PlayerLocation::Checkout(executable) => Ok(executable),
        PlayerLocation::Sdk {
            executable,
            manifest,
        } => {
            println!("{}: prebuilt (SDK)", flavor.package());
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

pub(crate) fn build_checkout_player(
    flavor: PlayerFlavor,
    dlss: bool,
) -> Result<PathBuf, BuildError> {
    let root = repo_root()?;
    let target_dir = normalize_build_path(&checkout_player_target_dir(&root, flavor))?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let package = flavor.package();
    let stdout = run_cargo(
        Command::new(cargo)
            .args(flavor.cargo_arguments(dlss))
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &target_dir),
        match flavor {
            PlayerFlavor::Distribution => "сборка distribution rve-player",
            PlayerFlavor::Dev => "сборка dev rve-player",
            PlayerFlavor::Server => "сборка rve-server",
        },
        CargoFailure::Operational,
    )?;
    let artifact = parse_cargo_output(&stdout, package, "bin", None)
        .map_err(|message| BuildError::operational(format!("сборка {package}: {message}")))?;
    let executable = artifact.executable.ok_or_else(|| {
        BuildError::operational(format!(
            "cargo не сообщил executable для бинарника {package}"
        ))
    })?;
    println!(
        "{package}: {}",
        if artifact.fresh {
            "up to date"
        } else {
            "rebuilt"
        }
    );
    normalize_build_path(&executable)
        .map_err(|error| BuildError::operational(format!("сборка {package}: {error}")))
}

pub(crate) fn build_sdk_binary(
    package: &str,
    distribution_player: bool,
    dlss: bool,
) -> Result<PathBuf, BuildError> {
    let mut command = sdk_build_command(package, distribution_player, dlss)?;
    let stdout = run_cargo(&mut command, "SDK build", CargoFailure::Operational)?;
    let artifact =
        parse_cargo_output(&stdout, package, "bin", None).map_err(BuildError::operational)?;
    let executable = artifact
        .executable
        .ok_or_else(|| BuildError::operational("cargo did not report executable"))?;
    normalize_build_path(&executable)
}

fn sdk_build_command(
    package: &str,
    distribution_player: bool,
    dlss: bool,
) -> Result<Command, BuildError> {
    let root = repo_root()?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let cache = if distribution_player {
        "sdk-player"
    } else {
        "sdk-tools"
    };
    let mut command = Command::new(cargo);
    command
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            package,
            "--message-format=json",
        ])
        .current_dir(root.join("build"))
        .env("CARGO_TARGET_DIR", root.join("build").join(cache));
    if distribution_player {
        command.arg("--no-default-features");
    }
    if dlss {
        command.args(["--features", "dlss"]);
    }
    if cfg!(windows) {
        // An explicit target keeps crt-static off host proc-macro crates.
        let environment = if cfg!(target_env = "gnu") {
            "gnu"
        } else {
            "msvc"
        };
        command.args([
            "--target",
            &format!("{}-pc-windows-{environment}", std::env::consts::ARCH),
        ]);
        command
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env("RUSTFLAGS", "-C target-feature=+crt-static");
    }
    Ok(command)
}

fn checkout_player_target_dir(root: &Path, flavor: PlayerFlavor) -> PathBuf {
    root.join("build").join(flavor.target_directory())
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

#[cfg(any(windows, test))]
fn copy_fsr_runtime(project: &ProjectBuild, engine_dir: &Path) -> Result<(), BuildError> {
    let Some(relative) = project.settings.runtime.fsr_library.as_deref() else {
        return Ok(());
    };
    let root = fs::canonicalize(&project.project_dir).map_err(|error| {
        BuildError::input(format!(
            "не удалось открыть каталог проекта для FSR: {error}"
        ))
    })?;
    let loader = fs::canonicalize(root.join(relative)).map_err(|error| {
        BuildError::input(format!("FSR loader DLL не найден ({relative}): {error}"))
    })?;
    if !loader.starts_with(&root) || !loader.is_file() {
        return Err(BuildError::input(format!(
            "runtime.fsr_library должен указывать на файл внутри проекта: {relative}"
        )));
    }
    let upscaler = fs::canonicalize(
        loader
            .parent()
            .ok_or_else(|| BuildError::input("FSR loader DLL не содержит каталог"))?
            .join("amd_fidelityfx_upscaler_vk.dll"),
    )
    .map_err(|error| {
        BuildError::input(format!(
            "FSR upscaler effect DLL не найден рядом с loader: {error}"
        ))
    })?;
    if !upscaler.starts_with(&root) || !upscaler.is_file() {
        return Err(BuildError::input(
            "FSR upscaler effect DLL должен находиться внутри каталога проекта",
        ));
    }
    let loader_name = loader
        .file_name()
        .ok_or_else(|| BuildError::input("FSR loader DLL не содержит имени файла"))?;
    create_dir_all(engine_dir)?;
    copy_file(&loader, &engine_dir.join(loader_name))?;
    copy_file(
        &upscaler,
        &engine_dir.join("amd_fidelityfx_upscaler_vk.dll"),
    )
}

#[cfg(any(windows, test))]
fn copy_dlss_runtime(project: &ProjectBuild, engine_dir: &Path) -> Result<(), BuildError> {
    let Some(relative) = project.settings.runtime.dlss_library.as_deref() else {
        return Ok(());
    };
    let root = fs::canonicalize(&project.project_dir).map_err(|error| {
        BuildError::input(format!(
            "не удалось открыть каталог проекта для DLSS: {error}"
        ))
    })?;
    let library = fs::canonicalize(root.join(relative)).map_err(|error| {
        BuildError::input(format!("DLSS runtime DLL не найден ({relative}): {error}"))
    })?;
    if !library.starts_with(&root) || !library.is_file() {
        return Err(BuildError::input(format!(
            "runtime.dlss_library должен указывать на файл внутри проекта: {relative}"
        )));
    }
    create_dir_all(engine_dir)?;
    copy_file(&library, &engine_dir.join("nvngx_dlss.dll"))
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
        CargoArtifact, CargoFailure, PlayerFlavor, build_distribution, checkout_player_target_dir,
        copy_dlss_runtime, copy_fsr_runtime, heavy_scripts_manifest, parse_cargo_output,
        parse_cdylib_target, player_file_name, sdk_build_command, select_library_file,
        validate_heavy_scripts, validate_staged_archives,
    };
    use crate::error::BuildError;
    use crate::project::{HeavyScriptUse, ProjectBuild, ScriptReferenceContext};
    use crate::temporary::TempDir;
    use engine_files::{VirtualPath, strip_windows_verbatim_prefix};
    use engine_project::ProjectSettings;
    use engine_scene::{ParamValue, SceneDocument};
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
            "Bobber" => test_factory { speed: f32 = 1.4 (0.0..=10.0) },
            "Spinner" => test_factory,
            "Reference" => test_factory {
                target: Option<Object> = None,
                camera: component(camera) = None,
                model: Asset = "",
            },
        };
        // SAFETY: script_module! stores the descriptors and names in static storage.
        unsafe { ScriptLibrary::from_module(module) }.unwrap()
    }

    #[test]
    fn bundled_command_uses_private_compiler_and_cargo_home() {
        let project = Path::new("project");
        let toolchain = Path::new("sdk/toolchain");
        let command = super::script_cargo_command(project, Some(toolchain)).unwrap();
        assert_eq!(
            command.get_program(),
            toolchain.join(format!("bin/cargo{}", std::env::consts::EXE_SUFFIX))
        );
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("RUSTC")],
            Some(
                toolchain
                    .join("bin")
                    .join(format!("rustc{}", std::env::consts::EXE_SUFFIX))
                    .as_os_str()
            )
        );
        assert_eq!(
            env[std::ffi::OsStr::new("CARGO_HOME")],
            Some(project.join(".rve/cargo-home").as_os_str())
        );
        assert_eq!(env[std::ffi::OsStr::new("RUSTUP_TOOLCHAIN")], None);
        assert_eq!(env[std::ffi::OsStr::new("RUSTFLAGS")], None);
        assert_eq!(
            std::env::split_paths(env[std::ffi::OsStr::new("PATH")].unwrap())
                .next()
                .unwrap(),
            toolchain.join("bin")
        );
    }

    #[cfg(windows)]
    #[test]
    fn bundled_windows_gnu_toolchain_uses_its_self_contained_linker() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-bundled-gnu-linker").unwrap();
        let toolchain = root.path().join("toolchain with spaces");
        let linker = toolchain
            .join("lib/rustlib")
            .join(format!("{}-pc-windows-gnu", std::env::consts::ARCH))
            .join("bin/rust-lld.exe");
        fs::create_dir_all(linker.parent().unwrap()).unwrap();
        fs::write(&linker, b"test linker marker").unwrap();

        let command =
            super::script_cargo_command(&root.path().join("project"), Some(&toolchain)).unwrap();
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();

        assert_eq!(
            env[std::ffi::OsStr::new("RUSTC_BOOTSTRAP")],
            Some(std::ffi::OsStr::new("1"))
        );
        let encoded = env[std::ffi::OsStr::new("CARGO_ENCODED_RUSTFLAGS")]
            .unwrap()
            .to_string_lossy();
        assert_eq!(
            encoded.split('\u{1f}').collect::<Vec<_>>(),
            [
                "-Zunstable-options",
                "-Clinker-flavor=gnu-lld",
                &format!("-Clinker={}", linker.display()),
                "-Clink-self-contained=yes",
                "-Ctarget-feature=+crt-static",
            ]
        );
    }

    #[test]
    fn new_project_template_builds_and_loads_its_heavy_scripts() {
        fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
            fs::create_dir_all(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let destination = destination.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    copy_tree(&entry.path(), &destination)?;
                } else {
                    fs::copy(entry.path(), destination)?;
                }
            }
            Ok(())
        }

        let parent = TempDir::create(&std::env::temp_dir(), "rve-new-project-template").unwrap();
        let project = parent.path().join("new-project");
        let template =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../editor/templates/new_project");
        copy_tree(&template, &project).unwrap();

        let library_path = super::scripts_project(&project).unwrap().unwrap();
        // SAFETY: the generated library remains on disk and follows the editor guest ABI.
        let library = unsafe { ScriptLibrary::load(&library_path) }.unwrap();
        assert!(library.contains_script("Orbit"));
        let schema = library.schema("Orbit").unwrap();
        assert_eq!(
            schema
                .specs()
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["radius", "height", "speed"]
        );
    }

    #[test]
    fn sdk_player_builds_preserve_dlss_and_distribution_settings() {
        for distribution in [false, true] {
            for dlss in [false, true] {
                let command = sdk_build_command("rve-player", distribution, dlss).unwrap();
                let arguments: Vec<_> = command
                    .get_args()
                    .map(|arg| arg.to_str().unwrap())
                    .collect();
                assert_eq!(arguments.contains(&"--no-default-features"), distribution);
                assert_eq!(
                    arguments
                        .windows(2)
                        .any(|args| args == ["--features", "dlss"]),
                    dlss
                );
                assert!(arguments.contains(&"--release"));
                let cache = command
                    .get_envs()
                    .find(|(key, _)| *key == "CARGO_TARGET_DIR")
                    .unwrap()
                    .1
                    .unwrap();
                assert!(Path::new(cache).ends_with(if distribution {
                    "sdk-player"
                } else {
                    "sdk-tools"
                }));
                if cfg!(windows) {
                    assert!(command.get_envs().any(|(key, value)| key == "RUSTFLAGS"
                        && value == Some(std::ffi::OsStr::new("-C target-feature=+crt-static"))));
                }
            }
        }
    }

    #[test]
    fn checkout_player_build_arguments_and_targets_follow_flavor() {
        let root = Path::new("checkout");
        assert_eq!(
            PlayerFlavor::Distribution.cargo_arguments(false),
            [
                "build",
                "--release",
                "--locked",
                "-p",
                "rve-player",
                "--no-default-features",
                "--message-format=json",
            ]
        );
        assert_eq!(
            PlayerFlavor::Dev.cargo_arguments(false),
            [
                "build",
                "--locked",
                "-p",
                "rve-player",
                "--message-format=json"
            ]
        );
        assert!(
            PlayerFlavor::Distribution
                .cargo_arguments(true)
                .windows(2)
                .any(|args| args == ["--features", "dlss"])
        );
        assert!(
            PlayerFlavor::Dev
                .cargo_arguments(true)
                .windows(2)
                .any(|args| args == ["--features", "dlss"])
        );
        assert_eq!(
            checkout_player_target_dir(root, PlayerFlavor::Distribution),
            root.join("build/player")
        );
        assert_eq!(
            checkout_player_target_dir(root, PlayerFlavor::Dev),
            root.join("build/player-dev")
        );
        assert_eq!(
            PlayerFlavor::Server.cargo_arguments(false),
            [
                "build",
                "--release",
                "--locked",
                "-p",
                "rve-server",
                "--no-default-features",
                "--message-format=json",
            ]
        );
        assert_eq!(
            checkout_player_target_dir(root, PlayerFlavor::Server),
            root.join("build/server")
        );
    }

    #[test]
    fn network_settings_survive_the_project_toml_written_for_release() {
        // Релиз пишет project.toml через to_toml: сетевые настройки должны вернуться без изменений.
        let settings = ProjectSettings::parse(
            "id = \"net\"\nname = \"Net\"\nversion = \"1\"\n[[scenes]]\npath = \"scene.rvescene\"\n[network]\nenabled = true\nport = 9100\nmax_clients = 16\ntick_hz = 20\ntimeout_ms = 5000\n",
        )
        .unwrap();
        let written = settings.to_toml().unwrap();
        assert!(written.contains("[network]"), "{written}");
        let reread = ProjectSettings::parse(&written).unwrap();
        assert_eq!(reread.network, settings.network);
        assert!(reread.network.enabled);
        assert_eq!(reread.network.port, 9100);
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
            prefabs: engine_project::PrefabCatalogFile::default(),
            files: BTreeMap::new(),
            assignments: BTreeMap::new(),
            archives: vec!["base".to_owned()],
            #[cfg(windows)]
            icon: None,
            heavy_scripts: Vec::new(),
        }
    }

    #[test]
    fn copies_optional_fsr_loader_and_effect_to_engine_directory() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-fsr-package").unwrap();
        let project_dir = root.path().join("project");
        let vendor_dir = project_dir.join("vendor");
        let engine_dir = root.path().join("output/Engine");
        fs::create_dir_all(&vendor_dir).unwrap();
        let loader = vendor_dir.join("amd_fidelityfx_loader_vk.dll");
        let upscaler = vendor_dir.join("amd_fidelityfx_upscaler_vk.dll");
        fs::write(&loader, b"loader").unwrap();
        fs::write(&upscaler, b"upscaler").unwrap();
        let mut project = sample_project(&project_dir, None);
        project.settings.runtime.fsr_library =
            Some("vendor/amd_fidelityfx_loader_vk.dll".to_owned());

        copy_fsr_runtime(&project, &engine_dir).unwrap();

        assert_eq!(
            fs::read(engine_dir.join("amd_fidelityfx_loader_vk.dll")).unwrap(),
            b"loader"
        );
        assert_eq!(
            fs::read(engine_dir.join("amd_fidelityfx_upscaler_vk.dll")).unwrap(),
            b"upscaler"
        );
    }

    #[test]
    fn configured_fsr_loader_requires_its_effect_dll() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-fsr-missing-effect").unwrap();
        let project_dir = root.path().join("project");
        let vendor_dir = project_dir.join("vendor");
        fs::create_dir_all(&vendor_dir).unwrap();
        fs::write(vendor_dir.join("loader.dll"), b"loader").unwrap();
        let mut project = sample_project(&project_dir, None);
        project.settings.runtime.fsr_library = Some("vendor/loader.dll".to_owned());

        let error = copy_fsr_runtime(&project, &root.path().join("output/Engine")).unwrap_err();

        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("upscaler effect DLL"));
    }

    #[test]
    fn copies_configured_dlss_runtime_into_engine_directory() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-dlss-package").unwrap();
        let project_dir = root.path().join("project");
        let vendor_dir = project_dir.join("vendor");
        let engine_dir = root.path().join("output/Engine");
        fs::create_dir_all(&vendor_dir).unwrap();
        fs::write(vendor_dir.join("nvngx_dlss.dll"), b"DLSS runtime").unwrap();
        let mut project = sample_project(&project_dir, None);
        project.settings.runtime.dlss_library = Some("vendor/nvngx_dlss.dll".to_owned());

        copy_dlss_runtime(&project, &engine_dir).unwrap();

        assert_eq!(
            fs::read(engine_dir.join("nvngx_dlss.dll")).unwrap(),
            b"DLSS runtime"
        );
    }

    #[test]
    fn configured_dlss_runtime_must_exist_before_packaging() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-dlss-missing").unwrap();
        let project_dir = root.path().join("project");
        fs::create_dir_all(&project_dir).unwrap();
        let mut project = sample_project(&project_dir, None);
        project.settings.runtime.dlss_library = Some("vendor/nvngx_dlss.dll".to_owned());

        let error = copy_dlss_runtime(&project, &root.path().join("output/Engine")).unwrap_err();

        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("DLSS runtime DLL не найден"));
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
            heavy("scenes/main.rvescene", "Bobber", "{ speed = 2 }"),
            heavy("scenes/side.rvescene", "Spinner", "{ anything = true }"),
        ];

        assert!(validate_heavy_scripts(&library, &scripts).is_ok());
    }

    #[test]
    fn missing_scene_heavy_script_is_a_project_error_with_scene_and_name() {
        let library = test_library();
        let error =
            validate_heavy_scripts(&library, &[heavy("scenes/main.rvescene", "Missing", "{}")])
                .unwrap_err();

        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("scenes/main.rvescene"));
        assert!(error.to_string().contains("Missing"));
    }

    fn heavy(scene: &str, name: &str, params: &str) -> HeavyScriptUse {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            params: BTreeMap<String, engine_scene::ParamValue>,
        }
        let wrapper: Wrapper = toml::from_str(&format!("params = {params}")).unwrap();
        HeavyScriptUse {
            owner: format!("сцена /{scene}"),
            object: "mesh".to_owned(),
            name: name.to_owned(),
            params: wrapper.params,
            references: std::sync::Arc::new(Default::default()),
        }
    }

    #[test]
    fn heavy_params_that_violate_the_library_schema_are_project_errors() {
        let library = test_library();
        for params in ["{ speed = 11.0 }", "{ speed = \"fast\" }", "{ spin = 1 }"] {
            let error = validate_heavy_scripts(
                &library,
                &[heavy("scenes/main.rvescene", "Bobber", params)],
            )
            .unwrap_err();
            assert_eq!(error.exit_code(), 2, "{params}: {error}");
            assert!(error.to_string().contains("\"mesh\""), "{error}");
            assert!(error.to_string().contains("Bobber"), "{error}");
        }
    }

    #[test]
    fn heavy_script_references_require_targets_components_and_declared_assets() {
        let library = test_library();
        let document = SceneDocument::parse(
            "references.rvescene",
            r#"
[[objects]]
id = "camera"
[objects.camera]
active = true
[[objects]]
id = "target"
name = "Target Camera"
[objects.camera]
[[objects]]
id = "plain"
"#,
        )
        .unwrap();
        let assets =
            std::collections::BTreeSet::from([VirtualPath::parse("models/ship.fbx").unwrap()]);
        let make_use = |target: &str, camera: &str, model: &str| {
            let params: BTreeMap<String, ParamValue> = toml::from_str(&format!(
                "target = {{ object = {target:?} }}\ncamera = {{ object = {camera:?} }}\nmodel = {{ asset = {model:?} }}"
            ))
            .unwrap();
            HeavyScriptUse {
                owner: "scene /references.rvescene".to_owned(),
                object: "camera".to_owned(),
                name: "Reference".to_owned(),
                params,
                references: std::sync::Arc::new(ScriptReferenceContext {
                    objects: document.objects.clone(),
                    assets: assets.clone(),
                }),
            }
        };

        assert!(
            validate_heavy_scripts(&library, &[make_use("target", "target", "models/ship.fbx")])
                .is_ok()
        );

        for (usage, expected) in [
            (make_use("missing", "target", "models/ship.fbx"), "missing"),
            (make_use("target", "plain", "models/ship.fbx"), "camera"),
            (
                make_use("target", "target", "models/missing.fbx"),
                "[[assets]]",
            ),
        ] {
            let error = validate_heavy_scripts(&library, &[usage]).unwrap_err();
            assert_eq!(error.exit_code(), 2);
            assert!(error.to_string().contains(expected), "{error}");
        }
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
