use crate::error::BuildError;
use crate::platform;
use crate::player;
use crate::temporary::TempDir;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const SDK_MARKER: &str = "rve-sdk.toml";

#[derive(Debug, Deserialize)]
pub(crate) struct SdkManifest {
    pub(crate) engine_version: String,
    abi_version: u32,
    target: String,
}

#[derive(Debug)]
pub(crate) enum PlayerLocation {
    Checkout(PathBuf),
    Sdk {
        executable: PathBuf,
        manifest: SdkManifest,
    },
}

pub(crate) fn locate_player_in(
    executable_dir: &Path,
    checkout_player: impl FnOnce() -> Result<PathBuf, BuildError>,
) -> Result<PlayerLocation, BuildError> {
    let marker = executable_dir.join(SDK_MARKER);
    let contents = match fs::read_to_string(&marker) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return checkout_player()
                .map(PlayerLocation::Checkout)
                .map_err(|error| {
                    BuildError::operational(format!(
                        "{error}; рядом с rve-build нет {SDK_MARKER} (если это SDK — маркер потерян)"
                    ))
                });
        }
        Err(error) => {
            return Err(BuildError::operational(format!(
                "не удалось прочитать SDK-маркер {}: {error}",
                marker.display()
            )));
        }
    };
    let manifest = parse_sdk_manifest(&contents).map_err(|error| {
        BuildError::operational(format!(
            "некорректный SDK-маркер {}: {error}",
            marker.display()
        ))
    })?;
    validate_sdk_manifest(&manifest)?;

    let executable = executable_dir.join(player_filename());
    if !executable.is_file() {
        return Err(BuildError::operational(format!(
            "в SDK отсутствует {} рядом с rve-build; повторно соберите SDK из checkout командой `rve-build sdk --output <dir>`",
            executable.display()
        )));
    }
    Ok(PlayerLocation::Sdk {
        executable,
        manifest,
    })
}

fn parse_sdk_manifest(contents: &str) -> Result<SdkManifest, String> {
    toml::from_str(contents).map_err(|error| error.to_string())
}

fn validate_sdk_manifest(manifest: &SdkManifest) -> Result<(), BuildError> {
    let expected_abi = rve_sdk::abi::RVE_SCRIPT_ABI_VERSION;
    if manifest.abi_version != expected_abi {
        return Err(BuildError::operational(format!(
            "SDK ABI version {} не совпадает с ожидаемой версией {expected_abi}",
            manifest.abi_version
        )));
    }
    let expected_target = current_target();
    if manifest.target != expected_target {
        return Err(BuildError::operational(format!(
            "SDK target {:?} не совпадает с текущей платформой {expected_target:?}",
            manifest.target
        )));
    }
    Ok(())
}

fn player_filename() -> &'static str {
    if cfg!(windows) {
        "rve-player.exe"
    } else {
        "rve-player"
    }
}

fn rve_build_filename() -> &'static str {
    if cfg!(windows) {
        "rve-build.exe"
    } else {
        "rve-build"
    }
}

pub(crate) fn current_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

pub(crate) fn build_sdk(output: &Path) -> Result<(), BuildError> {
    let output = player::absolute_output_path(output)?;
    player::ensure_output_missing(&output)?;

    let rve_build = player::current_executable_path()?;
    let executable_dir = player::executable_directory(&rve_build)?;
    let location = locate_player_in(&executable_dir, player::build_checkout_player)?;
    let rve_player = checkout_player_for_sdk(location)?;

    let output_parent = output.parent().ok_or_else(|| {
        BuildError::input(format!(
            "output должен называть каталог: {}",
            output.display()
        ))
    })?;
    player::create_dir_all(output_parent)?;
    let output_parent = fs::canonicalize(output_parent).map_err(|error| {
        BuildError::operational(format!(
            "не удалось определить каталог output {}: {error}",
            output.display()
        ))
    })?;
    let output_name = output
        .file_name()
        .ok_or_else(|| BuildError::input("output должен иметь имя каталога"))?
        .to_owned();
    let output = player::normalize_build_path(&output_parent.join(output_name))?;
    player::ensure_output_missing(&output)?;

    let staging = TempDir::create(&output_parent, "rve-build-sdk")?;
    let root = player::repo_root()?;
    assemble_sdk_layout(
        staging.path(),
        &SdkSources {
            rve_build: &rve_build,
            rve_player: &rve_player,
            rve_sdk: &root.join("engine/rve-sdk"),
            engine_version: env!("CARGO_PKG_VERSION"),
            abi_version: rve_sdk::abi::RVE_SCRIPT_ABI_VERSION,
            target: &current_target(),
        },
    )?;

    if cfg!(debug_assertions) {
        eprintln!(
            "предупреждение: SDK содержит текущую debug-сборку rve-build; для распространяемого SDK запустите `cargo run --release -p rve-build -- sdk --output <dir>`"
        );
    }

    match platform::publish_directory_no_replace(staging.path(), &output) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(BuildError::input(
            format!("output directory already exists: {}", output.display()),
        )),
        Err(error) => Err(BuildError::operational(format!(
            "не удалось опубликовать SDK {}: {error}",
            output.display()
        ))),
    }
}

fn checkout_player_for_sdk(location: PlayerLocation) -> Result<PathBuf, BuildError> {
    match location {
        PlayerLocation::Checkout(executable) => Ok(executable),
        PlayerLocation::Sdk { .. } => Err(BuildError::operational(
            "команда `sdk` доступна только в checkout-режиме rve-build",
        )),
    }
}

struct SdkSources<'a> {
    rve_build: &'a Path,
    rve_player: &'a Path,
    rve_sdk: &'a Path,
    engine_version: &'a str,
    abi_version: u32,
    target: &'a str,
}

fn assemble_sdk_layout(staging: &Path, sources: &SdkSources<'_>) -> Result<(), BuildError> {
    player::copy_file(sources.rve_build, &staging.join(rve_build_filename()))?;
    player::copy_file(sources.rve_player, &staging.join(player_filename()))?;

    let sdk_directory = staging.join("rve-sdk");
    player::create_dir_all(&sdk_directory)?;
    player::copy_file(
        &sources.rve_sdk.join("Cargo.toml"),
        &sdk_directory.join("Cargo.toml"),
    )?;
    copy_directory(&sources.rve_sdk.join("src"), &sdk_directory.join("src"))?;

    let marker = format!(
        "engine_version = {:?}\nabi_version = {}\ntarget = {:?}\n",
        sources.engine_version, sources.abi_version, sources.target
    );
    fs::write(staging.join(SDK_MARKER), marker).map_err(|error| {
        BuildError::operational(format!("не удалось записать SDK-маркер: {error}"))
    })
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), BuildError> {
    player::create_dir_all(destination)?;
    let entries = fs::read_dir(source).map_err(|error| {
        BuildError::operational(format!(
            "не удалось прочитать каталог SDK {}: {error}",
            source.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            BuildError::operational(format!(
                "не удалось прочитать запись каталога SDK {}: {error}",
                source.display()
            ))
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else {
            player::copy_file(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        PlayerLocation, SdkManifest, SdkSources, assemble_sdk_layout, checkout_player_for_sdk,
        current_target, locate_player_in,
    };
    use crate::error::BuildError;
    use crate::temporary::TempDir;
    use std::fs;
    use std::path::PathBuf;

    fn sdk_marker(abi_version: u32, target: &str) -> String {
        format!("engine_version = \"0.1.0\"\nabi_version = {abi_version}\ntarget = \"{target}\"\n")
    }

    fn player_name() -> &'static str {
        if cfg!(windows) {
            "rve-player.exe"
        } else {
            "rve-player"
        }
    }

    fn build_name() -> &'static str {
        if cfg!(windows) {
            "rve-build.exe"
        } else {
            "rve-build"
        }
    }

    fn temp_dir(prefix: &str) -> TempDir {
        TempDir::create(&std::env::temp_dir(), prefix).unwrap()
    }

    #[test]
    fn marker_absence_selects_checkout_and_calls_injected_locator() {
        let root = temp_dir("rve-sdk-no-marker");
        let checkout_player = PathBuf::from("checkout/rve-player");
        let found = locate_player_in(root.path(), || Ok(checkout_player.clone())).unwrap();
        assert!(matches!(found, PlayerLocation::Checkout(path) if path == checkout_player));
    }

    #[test]
    fn checkout_failure_hints_that_the_sdk_marker_may_be_missing() {
        let root = temp_dir("rve-sdk-checkout-error");
        let error = locate_player_in(root.path(), || {
            Err(BuildError::operational("checkout unavailable"))
        })
        .unwrap_err();

        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("rve-sdk.toml"));
        assert!(error.to_string().contains("маркер потерян"));
    }

    #[test]
    fn marker_selects_the_sdk_player_without_calling_checkout_locator() {
        let root = temp_dir("rve-sdk-marker");
        fs::write(
            root.path().join("rve-sdk.toml"),
            sdk_marker(rve_sdk::abi::RVE_SCRIPT_ABI_VERSION, &current_target()),
        )
        .unwrap();
        let executable = root.path().join(player_name());
        fs::write(&executable, b"prebuilt player").unwrap();

        let found = locate_player_in(root.path(), || panic!("checkout must not be used")).unwrap();
        assert!(matches!(
            found,
            PlayerLocation::Sdk { executable: path, manifest }
                if path == executable && manifest.engine_version == "0.1.0"
        ));
    }

    #[test]
    fn sdk_command_is_rejected_when_the_current_player_location_is_sdk_mode() {
        let error = checkout_player_for_sdk(PlayerLocation::Sdk {
            executable: PathBuf::from("sdk/rve-player"),
            manifest: SdkManifest {
                engine_version: "0.1.0".to_owned(),
                abi_version: rve_sdk::abi::RVE_SCRIPT_ABI_VERSION,
                target: current_target(),
            },
        })
        .unwrap_err();

        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("checkout-режиме"));
    }

    #[test]
    fn sdk_marker_without_a_player_is_an_operational_error_with_a_hint() {
        let root = temp_dir("rve-sdk-missing-player");
        fs::write(
            root.path().join("rve-sdk.toml"),
            sdk_marker(rve_sdk::abi::RVE_SCRIPT_ABI_VERSION, &current_target()),
        )
        .unwrap();

        let error =
            locate_player_in(root.path(), || panic!("checkout must not be used")).unwrap_err();
        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("rve-player"));
        assert!(error.to_string().contains("rve-build sdk"));
    }

    #[test]
    fn sdk_marker_rejects_a_different_abi_version() {
        let root = temp_dir("rve-sdk-wrong-abi");
        fs::write(
            root.path().join("rve-sdk.toml"),
            sdk_marker(rve_sdk::abi::RVE_SCRIPT_ABI_VERSION + 1, &current_target()),
        )
        .unwrap();

        let error =
            locate_player_in(root.path(), || panic!("checkout must not be used")).unwrap_err();
        assert!(error.to_string().contains("ABI version"));
    }

    #[test]
    fn sdk_marker_rejects_a_different_target() {
        let root = temp_dir("rve-sdk-wrong-target");
        fs::write(
            root.path().join("rve-sdk.toml"),
            sdk_marker(rve_sdk::abi::RVE_SCRIPT_ABI_VERSION, "other-unknown"),
        )
        .unwrap();

        let error =
            locate_player_in(root.path(), || panic!("checkout must not be used")).unwrap_err();
        assert!(error.to_string().contains("target"));
    }

    #[test]
    fn sdk_marker_rejects_invalid_toml() {
        let root = temp_dir("rve-sdk-invalid-toml");
        fs::write(root.path().join("rve-sdk.toml"), "abi_version = [\n").unwrap();

        let error =
            locate_player_in(root.path(), || panic!("checkout must not be used")).unwrap_err();
        assert!(error.to_string().contains("SDK-маркер"));
    }

    #[test]
    fn sdk_layout_copies_only_the_cli_player_and_sdk_sources() {
        let root = temp_dir("rve-sdk-layout");
        let source = root.path().join("source");
        let staging = root.path().join("staging");
        let sdk_source = source.join("rve-sdk");
        fs::create_dir_all(sdk_source.join("src/nested")).unwrap();
        fs::create_dir_all(sdk_source.join("target")).unwrap();
        fs::write(source.join("rve-build"), b"build binary").unwrap();
        fs::write(source.join("rve-player"), b"player binary").unwrap();
        fs::write(
            sdk_source.join("Cargo.toml"),
            b"[package]\nname = \"rve-sdk\"\n",
        )
        .unwrap();
        fs::write(sdk_source.join("src/lib.rs"), b"pub fn api() {}\n").unwrap();
        fs::write(
            sdk_source.join("src/nested/api.rs"),
            b"pub fn nested() {}\n",
        )
        .unwrap();
        fs::write(sdk_source.join("target/ignored"), b"not packaged").unwrap();
        fs::create_dir(&staging).unwrap();

        let rve_build = source.join("rve-build");
        let rve_player = source.join("rve-player");
        let target = current_target();
        assemble_sdk_layout(
            &staging,
            &SdkSources {
                rve_build: &rve_build,
                rve_player: &rve_player,
                rve_sdk: &sdk_source,
                engine_version: "0.1.0-test",
                abi_version: 42,
                target: &target,
            },
        )
        .unwrap();

        assert_eq!(
            fs::read(staging.join(build_name())).unwrap(),
            b"build binary"
        );
        assert_eq!(
            fs::read(staging.join(player_name())).unwrap(),
            b"player binary"
        );
        assert_eq!(
            fs::read(staging.join("rve-sdk/Cargo.toml")).unwrap(),
            b"[package]\nname = \"rve-sdk\"\n"
        );
        assert_eq!(
            fs::read(staging.join("rve-sdk/src/nested/api.rs")).unwrap(),
            b"pub fn nested() {}\n"
        );
        assert!(!staging.join("rve-sdk/target").exists());
        let marker = fs::read_to_string(staging.join("rve-sdk.toml")).unwrap();
        assert!(marker.contains("engine_version = \"0.1.0-test\""));
        assert!(marker.contains("abi_version = 42"));
        assert!(marker.contains(&format!("target = \"{target}\"")));
    }
}
