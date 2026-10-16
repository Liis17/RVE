use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

pub(crate) const USAGE: &str = "Usage:\n  rve-build build <project-dir> --output <dir> [--vulkan-sdk <dir>]\n  rve-build run <project-dir> [-- <rve-player args...>]\n  rve-build --help";

pub(crate) enum CommandLine {
    Help,
    Build {
        project_dir: PathBuf,
        output: PathBuf,
        vulkan_sdk: Option<PathBuf>,
    },
    Run {
        project_dir: PathBuf,
        player_args: Vec<OsString>,
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
        Some(value) => Err(format!("неизвестная команда {value:?}")),
        None => Err("имя команды должно быть текстом UTF-8".to_owned()),
    }
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
            parse(args(&[
                "build", "my project", "--output", "out dir", "--vulkan-sdk", "sdk"
            ]))
            .unwrap(),
            CommandLine::Build { project_dir, output, vulkan_sdk }
                if project_dir == Path::new("my project")
                    && output == Path::new("out dir")
                    && vulkan_sdk == Some(PathBuf::from("sdk"))
        ));
        assert!(matches!(
            parse(args(&["run", "my project", "--", "--frames", "120"]))
                .unwrap(),
            CommandLine::Run { player_args, .. }
                if player_args == args(&["--frames", "120"])
        ));
    }

    #[test]
    fn rejects_bad_commands_and_duplicate_or_missing_options() {
        for values in [
            vec!["build", "game"],
            vec!["build", "game", "--output", "out", "--output", "other"],
            vec!["build", "game", "--output"],
            vec!["run", "game", "--frames", "20"],
        ] {
            assert!(parse(args(&values)).is_err(), "accepted {values:?}");
        }
        assert!(parse(args(&["publish", "game"])).is_err());
    }
}
