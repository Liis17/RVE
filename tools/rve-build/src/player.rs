use crate::archives::build_project_archives;
use crate::error::BuildError;
use crate::ico::png_to_ico;
use crate::platform;
use crate::project::{ProjectBuild, prepare_project};
use crate::temporary::TempDir;
use engine_files::{VirtualPath, strip_windows_verbatim_prefix};
use engine_pak::{Archive, ArchiveCatalog, ArchiveLimits, ArchiveSetRegistry, CatalogLimits};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use toml::{Table, Value};

const PLAYER_PACKAGE: &str = "rve-game";
const EMBED_RESOURCE_VERSION: &str = "=3.0.11";

pub(crate) fn build_distribution(
    project_dir: &Path,
    output: &Path,
    vulkan_sdk: Option<&Path>,
) -> Result<(), BuildError> {
    let output = absolute_output_path(output)?;
    ensure_output_missing(&output)?;
    let project = prepare_project(project_dir)?;
    let executable_name = player_file_name(&project.settings.name, &project.settings.id)?;
    #[cfg(target_os = "macos")]
    let vulkan_files = platform::find_macos_vulkan_files(vulkan_sdk)?;
    #[cfg(not(target_os = "macos"))]
    let _ = vulkan_sdk;

    let executable = build_player(&project, true)?;

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
            output_parent.display()
        ))
    })?;
    let output_parent = normalize_build_path(&output_parent)?;
    let output_name = output
        .file_name()
        .ok_or_else(|| BuildError::input("output должен иметь имя каталога"))?;
    let output = normalize_build_path(&output_parent.join(output_name))?;
    ensure_output_missing(&output)?;

    let staging = TempDir::create(&output_parent, "rve-build-output")?;
    // Finder opens a bare executable through Terminal, so macOS releases are `.app` bundles.
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
    let assets_dir = resources_dir.join("Assets");
    build_project_archives(&project, &assets_dir)?;
    validate_staged_archives(&assets_dir)?;

    #[cfg(target_os = "macos")]
    platform::copy_macos_vulkan_files(&vulkan_files, &engine_dir)?;

    let output_binary = binary_dir.join(executable_name);
    copy_file(&executable, &output_binary)?;
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
    let executable = build_player(&project, false)?;
    let project_dir = normalize_build_path(&project.project_dir)?;
    let status = Command::new(&executable)
        .arg("--project")
        .arg(project_dir)
        .args(player_args)
        .status()
        .map_err(|error| {
            BuildError::operational(format!(
                "не удалось запустить rve-player {}: {error}",
                executable.display()
            ))
        })?;
    Ok(status.code().unwrap_or(1))
}

fn build_player(project: &ProjectBuild, release: bool) -> Result<PathBuf, BuildError> {
    let player_dir = normalize_build_path(&project.project_dir.join(".rve/player"))?;
    let target_dir = normalize_build_path(&project.project_dir.join(".rve/target"))?;
    fs::create_dir_all(&player_dir).map_err(|error| {
        BuildError::operational(format!(
            "не удалось создать {}: {error}",
            player_dir.display()
        ))
    })?;
    fs::create_dir_all(&target_dir).map_err(|error| {
        BuildError::operational(format!(
            "не удалось создать {}: {error}",
            target_dir.display()
        ))
    })?;

    let heavy = resolve_heavy_dependency(project)?;
    write_generated_player(project, &player_dir, heavy.as_ref())?;
    seed_player_lock(&player_dir)?;

    let executable = cargo_build(&player_dir, &target_dir, release)?;
    #[cfg(windows)]
    if project.icon.is_some() {
        platform::verify_group_icon(&executable)?;
    }
    Ok(executable)
}

struct HeavyDependency {
    path: PathBuf,
    package: String,
}

fn resolve_heavy_dependency(project: &ProjectBuild) -> Result<Option<HeavyDependency>, BuildError> {
    let Some(relative) = project.settings.heavy_scripts.as_deref() else {
        return Ok(None);
    };
    let project_root = fs::canonicalize(&project.project_dir).map_err(|error| {
        BuildError::operational(format!(
            "не удалось определить каталог проекта {}: {error}",
            project.project_dir.display()
        ))
    })?;
    let project_root = normalize_build_path(&project_root)?;
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
    let source = fs::read_to_string(&manifest_path).map_err(|error| {
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
    let manifest: Value = toml::from_str(&source).map_err(|error| {
        BuildError::input(format!("некорректный heavy_scripts Cargo.toml: {error}"))
    })?;
    let package = manifest
        .get("package")
        .and_then(Value::as_table)
        .and_then(|package| package.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            BuildError::input(format!(
                "{} должен задавать [package].name",
                manifest_path.display()
            ))
        })?
        .to_owned();
    Ok(Some(HeavyDependency { path, package }))
}

fn write_generated_player(
    project: &ProjectBuild,
    player_dir: &Path,
    heavy: Option<&HeavyDependency>,
) -> Result<(), BuildError> {
    let mut root = Table::new();
    root.insert("workspace".to_owned(), Value::Table(Table::new()));

    let mut package = Table::new();
    package.insert("name".to_owned(), Value::String(PLAYER_PACKAGE.to_owned()));
    package.insert("version".to_owned(), Value::String("0.1.0".to_owned()));
    package.insert("edition".to_owned(), Value::String("2024".to_owned()));
    root.insert("package".to_owned(), Value::Table(package));

    let repo_root = repo_root()?;
    let runtime_path = normalize_build_path(&repo_root.join("engine/engine-runtime"))?;
    let mut dependencies = Table::new();
    dependencies.insert(
        "engine-runtime".to_owned(),
        path_dependency(&runtime_path, None),
    );
    if let Some(heavy) = heavy {
        let heavy_path = normalize_build_path(&heavy.path)?;
        dependencies.insert(
            "game_heavy".to_owned(),
            path_dependency(&heavy_path, Some(&heavy.package)),
        );
    }
    root.insert("dependencies".to_owned(), Value::Table(dependencies));

    if project.icon.is_some() {
        let mut build_dependencies = Table::new();
        build_dependencies.insert(
            "embed-resource".to_owned(),
            Value::String(EMBED_RESOURCE_VERSION.to_owned()),
        );
        let mut windows_target = Table::new();
        windows_target.insert(
            "build-dependencies".to_owned(),
            Value::Table(build_dependencies),
        );
        let mut target = Table::new();
        target.insert("cfg(windows)".to_owned(), Value::Table(windows_target));
        root.insert("target".to_owned(), Value::Table(target));
    }

    let manifest = toml::to_string_pretty(&Value::Table(root)).map_err(|error| {
        BuildError::operational(format!(
            "не удалось сгенерировать player Cargo.toml: {error}"
        ))
    })?;
    write_file(&player_dir.join("Cargo.toml"), manifest.as_bytes())?;
    let main_source = match heavy {
        Some(_) => "fn main() -> std::process::ExitCode {\n    engine_runtime::run_player(game_heavy::register)\n}\n"
            .to_owned(),
        None => {
            "fn main() -> std::process::ExitCode {\n    engine_runtime::run_player(|_| {})\n}\n"
                .to_owned()
        }
    };
    let main_source = format!(
        "#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = \"windows\")]\n\n{main_source}"
    );
    let src_dir = player_dir.join("src");
    fs::create_dir_all(&src_dir).map_err(|error| {
        BuildError::operational(format!("не удалось создать {}: {error}", src_dir.display()))
    })?;
    write_file(&src_dir.join("main.rs"), main_source.as_bytes())?;

    if let Some(icon_path) = &project.icon {
        let source = project
            .files
            .get(icon_path)
            .ok_or_else(|| BuildError::input(format!("иконка /{icon_path} не найдена")))?;
        let icon_bytes = fs::read(source).map_err(|error| {
            BuildError::operational(format!("не удалось прочитать иконку /{icon_path}: {error}"))
        })?;
        png_to_ico(&icon_bytes).map_err(|error| {
            BuildError::input(format!("ошибка Windows-иконки /{icon_path}: {error}"))
        })?;
        write_file(&player_dir.join("icon.png"), &icon_bytes)?;
        let support_dir = player_dir.join("build_support");
        fs::create_dir_all(&support_dir).map_err(|error| {
            BuildError::operational(format!(
                "не удалось создать {}: {error}",
                support_dir.display()
            ))
        })?;
        write_file(
            &support_dir.join("ico.rs"),
            include_str!("ico.rs").as_bytes(),
        )?;
        write_file(
            &player_dir.join("build.rs"),
            windows_icon_build_script().as_bytes(),
        )?;
    } else {
        write_file(&player_dir.join("build.rs"), b"fn main() {}\n")?;
        remove_generated_file(&player_dir.join("icon.png"))?;
        remove_generated_file(&player_dir.join("build_support/ico.rs"))?;
    }
    Ok(())
}

fn path_dependency(path: &Path, package: Option<&str>) -> Value {
    let mut dependency = Table::new();
    dependency.insert(
        "path".to_owned(),
        Value::String(path.to_string_lossy().into_owned()),
    );
    if let Some(package) = package {
        dependency.insert("package".to_owned(), Value::String(package.to_owned()));
    }
    Value::Table(dependency)
}

fn windows_icon_build_script() -> &'static str {
    r#"#[path = "build_support/ico.rs"]
mod ico;

use std::{env, fs};

fn write_if_changed(path: &str, contents: &[u8]) -> std::io::Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == contents => Ok(()),
        Ok(_) => fs::write(path, contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::write(path, contents),
        Err(error) => Err(error),
    }
}

fn main() {
    println!("cargo:rerun-if-changed=icon.png");
    println!("cargo:rerun-if-changed=build_support/ico.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    #[cfg(windows)]
    {
        let png = fs::read("icon.png").expect("cannot read generated icon.png");
        let ico = ico::png_to_ico(&png).unwrap_or_else(|error| panic!("cannot generate app.ico: {error}"));
        write_if_changed("app.ico", &ico).expect("cannot write generated app.ico");
        write_if_changed("app.rc", b"1 ICON \"app.ico\"\n").expect("cannot write generated app.rc");
        embed_resource::compile("app.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap_or_else(|error| panic!("cannot embed app.ico: {error}"));
    }
}
"#
}

fn seed_player_lock(player_dir: &Path) -> Result<(), BuildError> {
    let destination = player_dir.join("Cargo.lock");
    if destination.exists() {
        return Ok(());
    }
    let source = repo_root()?.join("Cargo.lock");
    copy_file(&source, &destination)
}

fn cargo_build(player_dir: &Path, target_dir: &Path, release: bool) -> Result<PathBuf, BuildError> {
    let player_dir = normalize_build_path(player_dir)?;
    let target_dir = normalize_build_path(target_dir)?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    command
        .arg("build")
        .arg("--message-format=json-render-diagnostics")
        .current_dir(&player_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if release {
        command.arg("--release");
    }
    let output = command.output().map_err(|error| {
        BuildError::operational(format!("не удалось запустить cargo build: {error}"))
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    display_cargo_output(&stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            eprint!("{stderr}");
        }
        return Err(BuildError::operational(format!(
            "cargo build завершился с кодом {}",
            output.status.code().unwrap_or(1)
        )));
    }
    let executable = parse_cargo_output(&stdout).map_err(|_| {
        BuildError::operational(
            "cargo build завершился без compiler-artifact executable для бинарника rve-game",
        )
    })?;
    normalize_build_path(&executable)
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

fn parse_cargo_output(stdout: &str) -> Result<PathBuf, String> {
    let mut executable = None;
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<JsonValue>(line) else {
            continue;
        };
        if message.get("reason").and_then(JsonValue::as_str) != Some("compiler-artifact") {
            continue;
        }
        let Some(target) = message.get("target") else {
            continue;
        };
        let is_game_binary = target.get("name").and_then(JsonValue::as_str) == Some(PLAYER_PACKAGE)
            && target
                .get("kind")
                .and_then(JsonValue::as_array)
                .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
        if is_game_binary && let Some(path) = message.get("executable").and_then(JsonValue::as_str)
        {
            executable = Some(PathBuf::from(path));
        }
    }
    executable.ok_or_else(|| "cargo output has no rve-game executable".to_owned())
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

fn repo_root() -> Result<PathBuf, BuildError> {
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

fn create_dir_all(path: &Path) -> Result<(), BuildError> {
    fs::create_dir_all(path).map_err(|error| {
        BuildError::operational(format!("не удалось создать {}: {error}", path.display()))
    })
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), BuildError> {
    fs::copy(source, destination).map_err(|error| {
        BuildError::operational(format!(
            "не удалось скопировать {} в {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(())
}

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

fn remove_generated_file(path: &Path) -> Result<(), BuildError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BuildError::operational(format!(
            "не удалось удалить ранее сгенерированный {}: {error}",
            path.display()
        ))),
    }
}

fn absolute_output_path(path: &Path) -> Result<PathBuf, BuildError> {
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

fn ensure_output_missing(path: &Path) -> Result<(), BuildError> {
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

fn normalize_build_path(path: &Path) -> Result<PathBuf, BuildError> {
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
        build_distribution, parse_cargo_output, path_dependency, player_file_name,
        resolve_heavy_dependency, validate_staged_archives, write_generated_player,
    };
    use crate::error::BuildError;
    use crate::project::ProjectBuild;
    use crate::temporary::TempDir;
    use engine_files::{VirtualPath, strip_windows_verbatim_prefix};
    use engine_project::ProjectSettings;
    use png::{BitDepth, ColorType, Encoder};
    use std::collections::BTreeMap;
    use std::fs;
    use std::fs::{File, FileTimes};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};
    use toml::{Table, Value};

    #[test]
    fn parses_player_executable_from_mixed_cargo_output() {
        let output = concat!(
            "Compiling rve-game v0.1.0\n",
            "this line is not JSON\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"build_script_build\",\"kind\":[\"custom-build\"]},\"executable\":\"/tmp/build-script\"}\n",
            "{malformed JSON\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"rve-game\",\"kind\":[\"bin\"]},\"executable\":\"/tmp/rve-game\"}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"rve-game\",\"kind\":[\"bin\"]}}\n"
        );
        assert_eq!(
            parse_cargo_output(output).unwrap(),
            PathBuf::from("/tmp/rve-game")
        );
        assert!(parse_cargo_output("not json\n{broken").is_err());
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

    fn sample_project(project_dir: &Path, heavy_scripts: Option<String>) -> ProjectBuild {
        ProjectBuild {
            project_dir: project_dir.to_path_buf(),
            bootstrap_path: project_dir.join("bootstrap.rves"),
            settings: ProjectSettings::parse(&format!(
                "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n{heavy_line}[[scenes]]\npath=\"scene.rvescene\"\n",
                heavy_line = heavy_scripts
                    .as_ref()
                    .map(|path| format!("heavy_scripts = {path:?}\n"))
                    .unwrap_or_default()
            ))
            .unwrap(),
            files: BTreeMap::new(),
            assignments: BTreeMap::new(),
            archives: vec!["base".to_owned()],
            icon: None,
        }
    }

    fn tiny_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[32, 64, 128, 255]).unwrap();
        writer.finish().unwrap();
        bytes
    }

    #[test]
    fn generates_player_manifest_and_main_for_games_with_and_without_heavy_scripts() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-player-generated").unwrap();
        let without_project = root.path().join("no-heavy-project");
        let without_dir = root.path().join("no-heavy-player");
        fs::create_dir_all(&without_project).unwrap();
        fs::create_dir_all(&without_dir).unwrap();
        let icon_path = VirtualPath::parse("/icons/app.png").unwrap();
        let icon_source = root.path().join("app.png");
        fs::write(&icon_source, tiny_png()).unwrap();
        let mut no_heavy = sample_project(&without_project, None);
        no_heavy.icon = Some(icon_path.clone());
        no_heavy.files.insert(icon_path, icon_source);
        write_generated_player(&no_heavy, &without_dir, None).unwrap();
        let manifest = fs::read_to_string(without_dir.join("Cargo.toml")).unwrap();
        let manifest_value: toml::Value = toml::from_str(&manifest).unwrap();
        assert!(manifest_value.get("workspace").is_some());
        assert_eq!(
            manifest_value["dependencies"]["engine-runtime"]["path"].as_str(),
            Some(repo_engine_runtime_path().to_string_lossy().as_ref())
        );
        assert!(
            fs::read_to_string(without_dir.join("src/main.rs"))
                .unwrap()
                .contains("engine_runtime::run_player(|_| {})")
        );

        let unchanged_files = [
            without_dir.join("Cargo.toml"),
            without_dir.join("src/main.rs"),
            without_dir.join("build.rs"),
            without_dir.join("icon.png"),
            without_dir.join("build_support/ico.rs"),
        ];
        let old_time = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        for path in &unchanged_files {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(old_time))
                .unwrap();
        }
        let old_times = unchanged_files
            .iter()
            .map(|path| fs::metadata(path).unwrap().modified().unwrap())
            .collect::<Vec<_>>();
        write_generated_player(&no_heavy, &without_dir, None).unwrap();
        let new_times = unchanged_files
            .iter()
            .map(|path| fs::metadata(path).unwrap().modified().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(new_times, old_times);

        let project_dir = root.path().join("heavy-project");
        let with_dir = root.path().join("heavy-player");
        let heavy_dir = project_dir.join("Heavy Scripts");
        fs::create_dir_all(&heavy_dir).unwrap();
        fs::create_dir_all(&with_dir).unwrap();
        fs::write(
            heavy_dir.join("Cargo.toml"),
            "[workspace]\n[package]\nname=\"heavy-package\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .unwrap();
        let with_heavy = sample_project(&project_dir, Some("Heavy Scripts".to_owned()));
        let dependency = resolve_heavy_dependency(&with_heavy).unwrap().unwrap();
        write_generated_player(&with_heavy, &with_dir, Some(&dependency)).unwrap();
        let manifest = fs::read_to_string(with_dir.join("Cargo.toml")).unwrap();
        let manifest_value: toml::Value = toml::from_str(&manifest).unwrap();
        assert_eq!(
            manifest_value["dependencies"]["game_heavy"]["package"].as_str(),
            Some("heavy-package")
        );
        assert_eq!(
            manifest_value["dependencies"]["game_heavy"]["path"].as_str(),
            dependency.path.to_str()
        );
        assert!(
            fs::read_to_string(with_dir.join("src/main.rs"))
                .unwrap()
                .contains("engine_runtime::run_player(game_heavy::register)")
        );
    }

    #[test]
    #[cfg(windows)]
    fn generated_release_player_uses_gui_subsystem_and_debug_uses_console() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-player-subsystem").unwrap();
        let project = sample_project(root.path(), None);
        for heavy in [false, true] {
            let player_dir = root.path().join(if heavy { "heavy" } else { "lite" });
            fs::create_dir_all(&player_dir).unwrap();
            let dependency = super::HeavyDependency {
                path: root.path().to_owned(),
                package: "fixture-heavy".to_owned(),
            };
            write_generated_player(&project, &player_dir, heavy.then_some(&dependency)).unwrap();
            let source_path = player_dir.join("src/main.rs");
            let source = fs::read_to_string(&source_path).unwrap();
            // Replace only runtime dependencies; compile the actual generated entry point.
            fs::write(&source_path, format!(
                "{source}\nmod engine_runtime {{\n    pub fn run_player(_: fn(&mut ())) -> std::process::ExitCode {{\n        std::process::ExitCode::SUCCESS\n    }}\n}}\nmod game_heavy {{ pub fn register(_: &mut ()) {{}} }}\n"
            )).unwrap();
            for (debug_assertions, expected) in [("yes", 3_u16), ("no", 2_u16)] {
                let executable = player_dir.join(format!("player-{debug_assertions}.exe"));
                let output = std::process::Command::new(
                    std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
                )
                .arg("--edition=2024")
                .arg("-C")
                .arg(format!("debug-assertions={debug_assertions}"))
                .arg(&source_path)
                .arg("-o")
                .arg(&executable)
                .output()
                .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let bytes = fs::read(&executable).unwrap();
                let pe_offset = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
                assert_eq!(&bytes[pe_offset..pe_offset + 4], b"PE\0\0");
                // Subsystem is at offset 68 in both PE32 and PE32+ optional headers.
                let offset = pe_offset + 24 + 68;
                let subsystem = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
                assert_eq!(
                    subsystem, expected,
                    "heavy={heavy}, debug_assertions={debug_assertions}"
                );
            }
        }
    }

    #[test]
    fn removes_generated_icon_files_when_regenerating_player_without_icon() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-player-icon-removal").unwrap();
        let project_dir = root.path().join("project");
        let player_dir = root.path().join("player");
        fs::create_dir_all(&project_dir).unwrap();
        fs::create_dir_all(&player_dir).unwrap();

        let icon_path = VirtualPath::parse("/icons/app.png").unwrap();
        let icon_source = root.path().join("app.png");
        fs::write(&icon_source, tiny_png()).unwrap();
        let mut with_icon = sample_project(&project_dir, None);
        with_icon.icon = Some(icon_path.clone());
        with_icon.files.insert(icon_path, icon_source);
        write_generated_player(&with_icon, &player_dir, None).unwrap();

        assert!(player_dir.join("icon.png").is_file());
        assert!(player_dir.join("build_support/ico.rs").is_file());
        let manifest: Value =
            toml::from_str(&fs::read_to_string(player_dir.join("Cargo.toml")).unwrap()).unwrap();
        assert!(
            manifest["target"]["cfg(windows)"]["build-dependencies"]["embed-resource"]
                .as_str()
                .is_some()
        );

        let without_icon = sample_project(&project_dir, None);
        write_generated_player(&without_icon, &player_dir, None).unwrap();

        assert!(!player_dir.join("icon.png").exists());
        assert!(!player_dir.join("build_support/ico.rs").exists());
        assert_eq!(
            fs::read_to_string(player_dir.join("build.rs")).unwrap(),
            "fn main() {}\n"
        );
        let manifest: Value =
            toml::from_str(&fs::read_to_string(player_dir.join("Cargo.toml")).unwrap()).unwrap();
        assert!(
            manifest
                .get("target")
                .and_then(|target| target.get("cfg(windows)"))
                .and_then(|windows| windows.get("build-dependencies"))
                .and_then(|dependencies| dependencies.get("embed-resource"))
                .is_none()
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
            "[package]\nname=\"outside-heavy\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .unwrap();

        for value in ["../outside", outside.to_str().unwrap()] {
            let mut project = sample_project(&project_dir, None);
            project.settings.heavy_scripts = Some(value.to_owned());
            let error = match resolve_heavy_dependency(&project) {
                Err(error) => error,
                Ok(_) => panic!("accepted heavy_scripts path {value:?}"),
            };
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
            "[package]\nname=\"outside-heavy\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .unwrap();
        symlink(&outside, project_dir.join("linked-heavy")).unwrap();

        let project = sample_project(&project_dir, Some("linked-heavy".to_owned()));
        let error = match resolve_heavy_dependency(&project) {
            Err(error) => error,
            Ok(_) => panic!("accepted symlinked heavy_scripts outside project"),
        };
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn toml_path_dependencies_escape_spaces_quotes_and_windows_backslashes() {
        let path = PathBuf::from(r#"C:\Games\My "Game"\Heavy Scripts"#);
        let value = Value::Table(Table::from_iter([(
            "game_heavy".to_owned(),
            path_dependency(&path, Some("heavy-package")),
        )]));
        let serialized = toml::to_string(&value).unwrap();
        let parsed: Value = toml::from_str(&serialized).unwrap();
        assert_eq!(
            parsed["game_heavy"]["path"].as_str(),
            Some(path.to_string_lossy().as_ref())
        );
        assert_eq!(
            parsed["game_heavy"]["package"].as_str(),
            Some("heavy-package")
        );
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
    fn generated_player_temp_dir_is_removed_when_the_test_returns_an_error() {
        let parent = TempDir::create(&std::env::temp_dir(), "rve-player-error-cleanup").unwrap();
        let mut player_path = None;
        let result = (|| -> Result<(), BuildError> {
            let generated = TempDir::create(parent.path(), "generated-player")?;
            player_path = Some(generated.path().to_path_buf());
            let main = generated.path().join("main.rs");
            fs::write(&main, b"partial").unwrap();
            Err(BuildError::operational("fixture error"))
        })();
        assert_eq!(result.unwrap_err().exit_code(), 1);
        assert!(!player_path.unwrap().exists());
    }

    #[test]
    fn unchanged_generated_file_keeps_its_modification_time() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-player-mtime").unwrap();
        let path = root.path().join("Cargo.toml");
        super::write_file(&path, b"same content").unwrap();
        let old_time = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old_time))
            .unwrap();
        let old_time = fs::metadata(&path).unwrap().modified().unwrap();

        super::write_file(&path, b"same content").unwrap();

        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), old_time);
    }

    #[test]
    fn repo_runtime_path_is_canonical_and_normalized() {
        let path = super::repo_root().unwrap().join("engine/engine-runtime");
        let canonical = fs::canonicalize(&path).unwrap();
        assert_eq!(strip_windows_verbatim_prefix(&canonical).unwrap(), path);
    }

    fn repo_engine_runtime_path() -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("engine/engine-runtime");
        let canonical = fs::canonicalize(path).unwrap();
        strip_windows_verbatim_prefix(&canonical).unwrap()
    }
}
