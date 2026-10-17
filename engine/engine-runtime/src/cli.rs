use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) const USAGE: &str =
    "Использование: rve-player [--project <dir>] [--scripts <path>] [--frames N]";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PlayerArgs {
    pub project: Option<PathBuf>,
    pub scripts: Option<PathBuf>,
    pub frames: Option<u64>,
}

impl PlayerArgs {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            match argument.to_str() {
                Some("--project") => {
                    if parsed.project.is_some() {
                        return Err("параметр --project указан повторно".to_owned());
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| "укажите каталог после --project".to_owned())?;
                    if value.to_string_lossy().starts_with("--") {
                        return Err("укажите каталог после --project".to_owned());
                    }
                    parsed.project = Some(PathBuf::from(value));
                }
                Some("--frames") => {
                    if parsed.frames.is_some() {
                        return Err("параметр --frames указан повторно".to_owned());
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| "укажите число после --frames".to_owned())?;
                    let frames = value
                        .to_str()
                        .and_then(|value| value.parse::<u64>().ok())
                        .filter(|value| *value > 0)
                        .ok_or_else(|| {
                            "--frames должен быть положительным числом u64".to_owned()
                        })?;
                    parsed.frames = Some(frames);
                }
                Some("--scripts") => {
                    if parsed.scripts.is_some() {
                        return Err("параметр --scripts указан повторно".to_owned());
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| "укажите путь после --scripts".to_owned())?;
                    if value.to_string_lossy().starts_with("--") {
                        return Err("укажите путь после --scripts".to_owned());
                    }
                    parsed.scripts = Some(PathBuf::from(value));
                }
                Some(other) => return Err(format!("неизвестный аргумент {other:?}")),
                None => return Err("аргументы должны быть в UTF-8".to_owned()),
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::PlayerArgs;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn parse(args: &[&str]) -> Result<PlayerArgs, String> {
        PlayerArgs::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn accepts_project_and_positive_frame_limit_in_either_order() {
        assert_eq!(
            parse(&["--project", "my game", "--frames", "120"]).unwrap(),
            PlayerArgs {
                project: Some("my game".into()),
                scripts: None,
                frames: Some(120),
            }
        );
        assert_eq!(
            parse(&["--frames", "1", "--project", "./game"]).unwrap(),
            PlayerArgs {
                project: Some("./game".into()),
                scripts: None,
                frames: Some(1),
            }
        );
        assert_eq!(
            parse(&["--scripts", "scripts/game.dll", "--project", "game"])
                .unwrap()
                .scripts,
            Some(PathBuf::from("scripts/game.dll"))
        );
    }

    #[test]
    fn rejects_repeated_unknown_missing_and_invalid_arguments() {
        for args in [
            vec!["--project", "a", "--project", "b"],
            vec!["--frames", "1", "--frames", "2"],
            vec!["--unknown"],
            vec!["--project"],
            vec!["--project", "--frames", "2"],
            vec!["--frames"],
            vec!["--frames", "0"],
            vec!["--frames", "-1"],
            vec!["--frames", "18446744073709551616"],
            vec!["--frames", "1.5"],
            vec!["--scripts", "scripts.dll", "--scripts", "other.dll"],
            vec!["--scripts"],
            vec!["--scripts", "--project", "game"],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
    }
}
