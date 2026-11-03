use engine_render::DebugView;
use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) const USAGE: &str = "Использование: rve-player [--project <dir>] [--scripts <path>] [--frames N] [--debug-view lit|wireframe|normals|overdraw|lod|cascades]";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PlayerArgs {
    pub project: Option<PathBuf>,
    pub scripts: Option<PathBuf>,
    pub frames: Option<u64>,
    pub debug_view: Option<DebugView>,
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
                Some("--debug-view") => {
                    if parsed.debug_view.is_some() {
                        return Err("параметр --debug-view указан повторно".to_owned());
                    }
                    let value = args
                        .next()
                        .ok_or_else(|| "укажите режим после --debug-view".to_owned())?;
                    let view = match value.to_str() {
                        Some("lit") => DebugView::Lit,
                        Some("wireframe") => DebugView::Wireframe,
                        Some("normals") => DebugView::Normals,
                        Some("overdraw") => DebugView::Overdraw,
                        Some("lod") => DebugView::LodLevels,
                        Some("cascades") => DebugView::ShadowCascades,
                        Some(_) => {
                            return Err(
                                "--debug-view должен быть lit, wireframe, normals, overdraw, lod или cascades".to_owned(),
                            );
                        }
                        None => return Err("аргументы должны быть в UTF-8".to_owned()),
                    };
                    parsed.debug_view = Some(view);
                }
                Some(other) => return Err(format!("неизвестный аргумент {other:?}")),
                None => return Err("аргументы должны быть в UTF-8".to_owned()),
            }
        }
        if parsed.debug_view.is_some() && parsed.project.is_none() {
            return Err("--debug-view доступен только при запуске с --project".to_owned());
        }
        Ok(parsed)
    }

    pub fn parse_for_build(
        args: impl IntoIterator<Item = OsString>,
        dev_player: bool,
    ) -> Result<Self, String> {
        let parsed = Self::parse(args)?;
        if parsed.debug_view.is_some() && !dev_player {
            return Err("--debug-view доступен только в dev-player".to_owned());
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::PlayerArgs;
    use engine_render::DebugView;
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
                debug_view: None,
            }
        );
        assert_eq!(
            parse(&["--frames", "1", "--project", "./game"]).unwrap(),
            PlayerArgs {
                project: Some("./game".into()),
                scripts: None,
                frames: Some(1),
                debug_view: None,
            }
        );
        assert_eq!(
            parse(&["--scripts", "scripts/game.dll", "--project", "game"])
                .unwrap()
                .scripts,
            Some(PathBuf::from("scripts/game.dll"))
        );
        assert_eq!(
            parse(&["--project", "game", "--debug-view", "cascades"])
                .unwrap()
                .debug_view,
            Some(DebugView::ShadowCascades)
        );
        for (name, view) in [
            ("lit", DebugView::Lit),
            ("wireframe", DebugView::Wireframe),
            ("normals", DebugView::Normals),
            ("overdraw", DebugView::Overdraw),
            ("lod", DebugView::LodLevels),
        ] {
            assert_eq!(
                parse(&["--project", "game", "--debug-view", name])
                    .unwrap()
                    .debug_view,
                Some(view)
            );
        }
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
            vec!["--debug-view"],
            vec!["--debug-view", "bad"],
            vec!["--debug-view", "lit", "--debug-view", "normals"],
            vec!["--debug-view", "lit"],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn debug_view_is_rejected_by_release_builds() {
        let args = ["--project", "game", "--debug-view", "normals"]
            .into_iter()
            .map(OsString::from);
        assert!(PlayerArgs::parse_for_build(args, false).is_err());

        let args = ["--project", "game", "--debug-view", "normals"]
            .into_iter()
            .map(OsString::from);
        assert_eq!(
            PlayerArgs::parse_for_build(args, true).unwrap().debug_view,
            Some(DebugView::Normals)
        );
    }
}
