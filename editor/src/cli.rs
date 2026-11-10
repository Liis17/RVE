//! Разбор аргументов командной строки редактора.

use std::ffi::OsString;
use std::path::PathBuf;

pub const USAGE: &str = "Использование: rve-editor [--project <dir>] [--scripts <library>] [--mcp]";

/// Настройки запуска; без `--project` редактор показывает стартовый экран.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct EditorArgs {
    pub project: Option<PathBuf>,
    pub scripts: Option<PathBuf>,
    pub mcp: bool,
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<EditorArgs, String> {
    let mut parsed = EditorArgs::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--mcp" {
            if parsed.mcp {
                return Err(format!("аргумент {arg:?} указан повторно"));
            }
            parsed.mcp = true;
            continue;
        }
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
    fn mcp_flag_enables_the_server_for_this_run() {
        let args = parse(&["--mcp", "--project", "game"]).unwrap();
        assert!(args.mcp);
        assert_eq!(args.project, Some(PathBuf::from("game")));
        assert!(!parse(&[]).unwrap().mcp);
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
        assert!(parse(&["--mcp", "--mcp"]).unwrap_err().contains("повторно"));
    }
}
