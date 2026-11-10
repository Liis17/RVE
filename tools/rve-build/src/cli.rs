use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

pub(crate) const USAGE: &str = "Usage:\n  rve-build build <project-dir> --output <dir> [--vulkan-sdk <dir>] [--with-server]\n  rve-build run <project-dir> [-- <rve-player args...>]\n  rve-build sdk --output <dir> (--rust-toolchain <dir> | --no-rust-toolchain) [--vulkan-sdk <dir>]\n  rve-build scripts <project-dir>\n  rve-build --help";

pub(crate) enum CommandLine {
    Help,
    Scripts {
        project_dir: PathBuf,
    },
    Build {
        project_dir: PathBuf,
        output: PathBuf,
        vulkan_sdk: Option<PathBuf>,
        with_server: bool,
    },
    Run {
        project_dir: PathBuf,
        player_args: Vec<OsString>,
    },
    Sdk {
        output: PathBuf,
        rust_toolchain: Option<PathBuf>,
        vulkan_sdk: Option<PathBuf>,
    },
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<CommandLine, String> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Err("ожидается команда build, run или --help".to_owned());
    };
    if is_help(&command) {
        return Ok(CommandLine::Help);
    }

    match command.to_str() {
        Some("build") => parse_build(args),
        Some("run") => parse_run(args),
        Some("sdk") => parse_sdk(args),
        Some("scripts") => {
            let project_dir = args.next().ok_or("для scripts требуется <project-dir>")?;
            if is_help(&project_dir) {
                return Ok(CommandLine::Help);
            }
            if args.next().is_some() {
                return Err("лишние аргументы scripts".to_owned());
            }
            Ok(CommandLine::Scripts {
                project_dir: project_dir.into(),
            })
        }
        Some(value) => Err(format!("неизвестная команда {value:?}")),
        None => Err("имя команды должно быть текстом UTF-8".to_owned()),
    }
}

fn parse_sdk(mut args: impl Iterator<Item = OsString>) -> Result<CommandLine, String> {
    let mut output = None;
    let mut toolchain = None;
    let mut selected = false;
    let mut vulkan_sdk = None;
    while let Some(argument) = args.next() {
        if is_help(&argument) {
            return Ok(CommandLine::Help);
        }
        match argument.to_str() {
            Some("--output") => {
                if output.is_some() {
                    return Err("параметр --output указан повторно".to_owned());
                }
                output =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        "после --output требуется каталог".to_owned()
                    })?));
            }
            Some("--rust-toolchain" | "--no-rust-toolchain") => {
                if selected {
                    return Err("выберите один флаг toolchain".to_owned());
                }
                selected = true;
                if argument == "--rust-toolchain" {
                    toolchain = Some(PathBuf::from(
                        args.next().ok_or("ожидается каталог toolchain")?,
                    ));
                }
            }
            Some("--vulkan-sdk") => {
                if vulkan_sdk.is_some() {
                    return Err("повторный --vulkan-sdk".to_owned());
                }
                vulkan_sdk = Some(PathBuf::from(
                    args.next().ok_or("ожидается каталог Vulkan SDK")?,
                ));
            }
            Some(value) => return Err(format!("неизвестный аргумент sdk: {value:?}")),
            None => return Err("аргумент sdk должен быть текстом UTF-8".to_owned()),
        }
    }
    let output = output.ok_or_else(|| "для sdk требуется --output <dir>".to_owned())?;
    if !selected {
        return Err("требуется --rust-toolchain <dir> или --no-rust-toolchain".to_owned());
    }
    Ok(CommandLine::Sdk {
        output,
        rust_toolchain: toolchain,
        vulkan_sdk,
    })
}

fn parse_build(mut args: impl Iterator<Item = OsString>) -> Result<CommandLine, String> {
    let Some(first) = args.next() else {
        return Err("для build требуется <project-dir>".to_owned());
    };
    if is_help(&first) {
        return Ok(CommandLine::Help);
    }
    let project_dir = PathBuf::from(first);
    let mut output = None;
    let mut vulkan_sdk = None;
    let mut with_server = false;
    while let Some(argument) = args.next() {
        if is_help(&argument) {
            return Ok(CommandLine::Help);
        }
        match argument.to_str() {
            Some("--output") => {
                if output.is_some() {
                    return Err("параметр --output указан повторно".to_owned());
                }
                output =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        "после --output требуется каталог".to_owned()
                    })?));
            }
            Some("--with-server") => {
                if with_server {
                    return Err("параметр --with-server указан повторно".to_owned());
                }
                with_server = true;
            }
            Some("--vulkan-sdk") => {
                if vulkan_sdk.is_some() {
                    return Err("параметр --vulkan-sdk указан повторно".to_owned());
                }
                vulkan_sdk =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        "после --vulkan-sdk требуется каталог".to_owned()
                    })?));
            }
            Some(value) => return Err(format!("неизвестный аргумент build: {value:?}")),
            None => return Err("аргумент build должен быть текстом UTF-8".to_owned()),
        }
    }
    let output = output.ok_or_else(|| "для build требуется --output <dir>".to_owned())?;
    Ok(CommandLine::Build {
        project_dir,
        output,
        vulkan_sdk,
        with_server,
    })
}

fn parse_run(mut args: impl Iterator<Item = OsString>) -> Result<CommandLine, String> {
    let Some(first) = args.next() else {
        return Err("для run требуется <project-dir>".to_owned());
    };
    if is_help(&first) {
        return Ok(CommandLine::Help);
    }
    let project_dir = PathBuf::from(first);
    let Some(separator) = args.next() else {
        return Ok(CommandLine::Run {
            project_dir,
            player_args: Vec::new(),
        });
    };
    if separator != OsStr::new("--") {
        return Err("аргументы rve-player передаются после --".to_owned());
    }
    Ok(CommandLine::Run {
        project_dir,
        player_args: args.collect(),
    })
}

fn is_help(value: &OsStr) -> bool {
    value == OsStr::new("--help") || value == OsStr::new("-h")
}

#[cfg(test)]
mod tests {
    use super::{CommandLine, parse};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn scripts_requires_one_project_path() {
        assert!(
            matches!(parse(args(&["scripts", "my project"])).unwrap(), CommandLine::Scripts { project_dir } if project_dir == Path::new("my project"))
        );
        assert!(parse(args(&["scripts"])).is_err());
        assert!(parse(args(&["scripts", "game", "extra"])).is_err());
    }

    #[test]
    fn parses_help_build_and_player_arguments() {
        assert!(matches!(
            parse(args(&["--help"])).unwrap(),
            CommandLine::Help
        ));
        assert!(matches!(
            parse(args(&["build", "--help"])).unwrap(),
            CommandLine::Help
        ));
        assert!(matches!(
            parse(args(&["run", "--help"])).unwrap(),
            CommandLine::Help
        ));
        assert!(matches!(
            parse(args(&["sdk", "--help"])).unwrap(),
            CommandLine::Help
        ));
        assert!(matches!(
            parse(args(&[
                "build", "my project", "--output", "out dir", "--vulkan-sdk", "sdk"
            ]))
            .unwrap(),
            CommandLine::Build { project_dir, output, vulkan_sdk, with_server }
                if project_dir == Path::new("my project")
                    && output == Path::new("out dir")
                    && vulkan_sdk == Some(PathBuf::from("sdk"))
                    && !with_server
        ));
        assert!(matches!(
            parse(args(&["run", "my project", "--", "--frames", "120"]))
                .unwrap(),
            CommandLine::Run { player_args, .. }
                if player_args == args(&["--frames", "120"])
        ));
        assert!(matches!(
            parse(args(&["sdk", "--output", "sdk dir", "--no-rust-toolchain"])).unwrap(),
            CommandLine::Sdk { output, .. } if output == Path::new("sdk dir")
        ));
    }

    #[test]
    fn build_includes_the_server_only_with_its_flag() {
        assert!(matches!(
            parse(args(&["build", "game", "--with-server", "--output", "out"])).unwrap(),
            CommandLine::Build {
                with_server: true,
                ..
            }
        ));
        let error = parse(args(&[
            "build",
            "game",
            "--output",
            "out",
            "--with-server",
            "--with-server",
        ]))
        .err()
        .unwrap();
        assert!(error.contains("--with-server"), "{error}");
    }

    #[test]
    fn sdk_requires_exactly_one_toolchain_choice() {
        for flags in [
            vec![],
            vec!["--rust-toolchain"],
            vec!["--rust-toolchain", "rust", "--no-rust-toolchain"],
            vec!["--no-rust-toolchain", "--no-rust-toolchain"],
        ] {
            let mut values = vec!["sdk", "--output", "out"];
            values.extend(flags);
            assert!(parse(args(&values)).is_err());
        }
        assert!(
            matches!(parse(args(&["sdk", "--output", "out", "--rust-toolchain", "rust", "--vulkan-sdk", "vk"])).unwrap(),
            CommandLine::Sdk { rust_toolchain: Some(rust), vulkan_sdk: Some(vk), .. } if rust == Path::new("rust") && vk == Path::new("vk"))
        );
    }

    #[test]
    fn rejects_bad_commands_and_duplicate_or_missing_options() {
        for values in [
            vec!["build", "game"],
            vec!["build", "game", "--output", "out", "--output", "other"],
            vec!["build", "game", "--output"],
            vec!["run", "game", "--frames", "20"],
            vec!["sdk"],
            vec!["sdk", "--output"],
            vec!["sdk", "--output", "out", "--output", "other"],
            vec!["sdk", "--bad"],
        ] {
            assert!(parse(args(&values)).is_err(), "accepted {values:?}");
        }
        assert!(parse(args(&["publish", "game"])).is_err());
    }
}
