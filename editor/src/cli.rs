//! Разбор аргументов командной строки редактора.

use std::ffi::OsString;
use std::path::PathBuf;

pub const USAGE: &str = "Использование: rve-editor [--project <dir>] [--scripts <library>]";

/// Настройки запуска; без `--project` редактор показывает стартовый экран.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct EditorArgs {
    pub project: Option<PathBuf>,
    pub scripts: Option<PathBuf>,
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<EditorArgs, String> {
    let mut parsed = EditorArgs::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let target = match arg.to_str() {
            Some("--project") => &mut parsed.project,
            Some("--scripts") => &mut parsed.scripts,
            _ => return Err(format!("неизвестный аргумент {arg:?}")),
        };
        let value = args
            .next()
            .ok_or_else(|| format!("аргументу {arg:?} нужно значение"))?;
        if target.replace(PathBuf::from(value)).is_some() {
            return Err(format!("аргумент {arg:?} указан повторно"));
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<EditorArgs, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn project_and_scripts_are_optional() {
        assert_eq!(parse(&[]).unwrap(), EditorArgs::default());
        let args = parse(&["--project", "game", "--scripts", "lib.dll"]).unwrap();
        assert_eq!(args.project, Some(PathBuf::from("game")));
        assert_eq!(args.scripts, Some(PathBuf::from("lib.dll")));
    }

    #[test]
    fn unknown_duplicate_and_valueless_arguments_are_rejected() {
        assert!(parse(&["--frames"]).unwrap_err().contains("неизвестный"));
        assert!(parse(&["--project"]).unwrap_err().contains("значение"));
        assert!(
            parse(&["--project", "a", "--project", "b"])
                .unwrap_err()
                .contains("повторно")
        );
    }
}
