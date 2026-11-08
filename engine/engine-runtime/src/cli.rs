use engine_render::DebugView;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

pub(crate) const USAGE: &str = "Использование: rve-player [--project <dir>] [--scripts <path>] [--frames N] [--debug-view lit|wireframe|normals|overdraw|lod|cascades] [--host [--host-addr <addr:port>] | --connect <addr:port> (--server-fingerprint <hex> | --insecure-no-pin)] [--server-key <file>] [--password-file <file>]";
pub(crate) const SERVER_USAGE: &str = "Использование: rve-server [--project <dir>] [--scripts <path>] [--frames N] [--bind <addr:port>] [--server-key <file>] [--password-file <file>]";

/// Сетевые флаги. Без `--host` и `--connect` процесс работает offline.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct NetworkArgs {
    /// `--host`: процесс принимает подключения (listen-server или выделенный сервер).
    pub host: bool,
    /// `--host-addr` (у `rve-server` — `--bind`): адрес прослушивания. Без него порт берётся из `[network].port`.
    pub host_addr: Option<SocketAddr>,
    /// `--connect`: адрес сервера.
    pub connect: Option<SocketAddr>,
    /// `--server-fingerprint`: закреплённый отпечаток ключа сервера.
    pub fingerprint: Option<[u8; 32]>,
    /// `--insecure-no-pin`: подключение без закрепления отпечатка.
    pub insecure_no_pin: bool,
    /// `--server-key`: файл ключа сервера; без него ключ эфемерный.
    pub server_key: Option<PathBuf>,
    /// `--password-file`: файл пароля комнаты.
    pub password_file: Option<PathBuf>,
}

impl NetworkArgs {
    /// Сетевой режим выбран флагами.
    pub fn is_requested(&self) -> bool {
        self.host || self.connect.is_some()
    }

    fn validate(&self) -> Result<(), String> {
        if self.host && self.connect.is_some() {
            return Err("--host и --connect нельзя указывать вместе".to_owned());
        }
        if !self.host {
            if self.host_addr.is_some() {
                return Err("--host-addr требует --host".to_owned());
            }
            if self.server_key.is_some() {
                return Err("--server-key требует --host".to_owned());
            }
        }
        if !self.is_requested() && self.password_file.is_some() {
            return Err("--password-file требует --host или --connect".to_owned());
        }
        match self.connect {
            Some(_) => match (self.fingerprint.is_some(), self.insecure_no_pin) {
                (true, true) => {
                    Err("--server-fingerprint и --insecure-no-pin взаимоисключающие".to_owned())
                }
                (false, false) => {
                    Err("--connect требует --server-fingerprint или --insecure-no-pin".to_owned())
                }
                _ => Ok(()),
            },
            None if self.fingerprint.is_some() || self.insecure_no_pin => {
                Err("--server-fingerprint и --insecure-no-pin требуют --connect".to_owned())
            }
            None => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PlayerArgs {
    pub project: Option<PathBuf>,
    pub scripts: Option<PathBuf>,
    pub frames: Option<u64>,
    pub debug_view: Option<DebugView>,
    pub network: NetworkArgs,
}

impl PlayerArgs {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        parse_args(args, false)
    }

    /// Аргументы `rve-server`: тот же набор, что у игры, плюс `--bind`, без режимов клиента и отладки.
    pub fn parse_server(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        parse_args(args, true)
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

fn parse_args(
    args: impl IntoIterator<Item = OsString>,
    server: bool,
) -> Result<PlayerArgs, String> {
    let mut parsed = PlayerArgs::default();
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
                    .ok_or_else(|| "--frames должен быть положительным числом u64".to_owned())?;
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
            Some("--debug-view") if !server => {
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
            Some("--host") if !server => set_flag(&mut parsed.network.host, "--host")?,
            Some("--insecure-no-pin") if !server => {
                set_flag(&mut parsed.network.insecure_no_pin, "--insecure-no-pin")?;
            }
            Some("--host-addr") if !server => {
                set_address(&mut parsed.network.host_addr, &mut args, "--host-addr")?;
            }
            Some("--bind") if server => {
                set_address(&mut parsed.network.host_addr, &mut args, "--bind")?;
            }
            Some("--connect") if !server => {
                set_address(&mut parsed.network.connect, &mut args, "--connect")?;
            }
            Some("--server-fingerprint") if !server => {
                if parsed.network.fingerprint.is_some() {
                    return Err("параметр --server-fingerprint указан повторно".to_owned());
                }
                let value = value_after(&mut args, "--server-fingerprint", "отпечаток")?;
                let text = value
                    .to_str()
                    .ok_or_else(|| "--server-fingerprint должен быть в UTF-8".to_owned())?;
                parsed.network.fingerprint = Some(parse_fingerprint(text)?);
            }
            Some("--server-key") => {
                if parsed.network.server_key.is_some() {
                    return Err("параметр --server-key указан повторно".to_owned());
                }
                let value = value_after(&mut args, "--server-key", "путь к файлу ключа")?;
                parsed.network.server_key = Some(PathBuf::from(value));
            }
            Some("--password-file") => {
                if parsed.network.password_file.is_some() {
                    return Err("параметр --password-file указан повторно".to_owned());
                }
                let value = value_after(&mut args, "--password-file", "путь к файлу пароля")?;
                parsed.network.password_file = Some(PathBuf::from(value));
            }
            Some(other) => return Err(format!("неизвестный аргумент {other:?}")),
            None => return Err("аргументы должны быть в UTF-8".to_owned()),
        }
    }
    if server {
        // Выделенный сервер всегда принимает подключения; без --bind адрес берётся из проекта.
        parsed.network.host = true;
    }
    if parsed.debug_view.is_some() && parsed.project.is_none() {
        return Err("--debug-view доступен только при запуске с --project".to_owned());
    }
    parsed.network.validate()?;
    Ok(parsed)
}

fn set_flag(flag: &mut bool, name: &str) -> Result<(), String> {
    if *flag {
        return Err(format!("параметр {name} указан повторно"));
    }
    *flag = true;
    Ok(())
}

fn set_address(
    slot: &mut Option<SocketAddr>,
    args: &mut impl Iterator<Item = OsString>,
    name: &str,
) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("параметр {name} указан повторно"));
    }
    let value = value_after(args, name, "адрес addr:port")?;
    let address = value
        .to_str()
        .and_then(|text| text.parse::<SocketAddr>().ok())
        .ok_or_else(|| format!("{name} должен быть адресом addr:port"))?;
    *slot = Some(address);
    Ok(())
}

fn value_after(
    args: &mut impl Iterator<Item = OsString>,
    flag: &str,
    expected: &str,
) -> Result<OsString, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("укажите {expected} после {flag}"))?;
    if value.to_string_lossy().starts_with("--") {
        return Err(format!("укажите {expected} после {flag}"));
    }
    Ok(value)
}

/// Отпечаток: ровно 64 hex-символа, регистр не важен.
pub(crate) fn parse_fingerprint(text: &str) -> Result<[u8; 32], String> {
    engine_network::parse_fingerprint(text)
        .ok_or_else(|| "--server-fingerprint должен содержать 64 hex-символа".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{PlayerArgs, parse_fingerprint};
    use engine_render::DebugView;
    use std::ffi::OsString;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    const FINGERPRINT: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn parse(args: &[&str]) -> Result<PlayerArgs, String> {
        PlayerArgs::parse(args.iter().map(OsString::from))
    }

    fn parse_server(args: &[&str]) -> Result<PlayerArgs, String> {
        PlayerArgs::parse_server(args.iter().map(OsString::from))
    }

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn accepts_project_and_positive_frame_limit_in_either_order() {
        assert_eq!(
            parse(&["--project", "my game", "--frames", "120"]).unwrap(),
            PlayerArgs {
                project: Some("my game".into()),
                frames: Some(120),
                ..PlayerArgs::default()
            }
        );
        assert_eq!(
            parse(&["--frames", "1", "--project", "./game"]).unwrap(),
            PlayerArgs {
                project: Some("./game".into()),
                frames: Some(1),
                ..PlayerArgs::default()
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

    #[test]
    fn offline_by_default_and_host_or_connect_enable_network() {
        assert!(
            !parse(&["--project", "game"])
                .unwrap()
                .network
                .is_requested()
        );

        let host = parse(&["--host"]).unwrap().network;
        assert!(host.host && host.is_requested());
        assert_eq!(host.host_addr, None);

        let addr = parse(&["--host", "--host-addr", "0.0.0.0:9000"])
            .unwrap()
            .network;
        assert_eq!(addr.host_addr, Some(address("0.0.0.0:9000")));

        let pinned = parse(&[
            "--connect",
            "127.0.0.1:7777",
            "--server-fingerprint",
            FINGERPRINT,
        ])
        .unwrap()
        .network;
        assert_eq!(pinned.connect, Some(address("127.0.0.1:7777")));
        assert_eq!(
            pinned.fingerprint,
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
                0xcc, 0xdd, 0xee, 0xff
            ])
        );

        let insecure = parse(&["--connect", "[::1]:7777", "--insecure-no-pin"])
            .unwrap()
            .network;
        assert!(insecure.insecure_no_pin && insecure.connect.is_some());
    }

    #[test]
    fn network_flags_reject_conflicting_and_misplaced_combinations() {
        for args in [
            vec!["--host", "--connect", "127.0.0.1:7777", "--insecure-no-pin"],
            vec!["--host-addr", "0.0.0.0:7777"],
            vec!["--server-key", "key.bin"],
            vec!["--password-file", "pass.txt"],
            vec!["--connect", "127.0.0.1:7777"],
            vec![
                "--connect",
                "127.0.0.1:7777",
                "--server-fingerprint",
                FINGERPRINT,
                "--insecure-no-pin",
            ],
            vec!["--server-fingerprint", FINGERPRINT],
            vec!["--insecure-no-pin"],
            vec!["--host", "--server-fingerprint", FINGERPRINT],
            vec!["--host", "--host", "--host-addr", "not-an-address"],
            vec!["--host-addr", "127.0.0.1"],
            vec!["--connect", "127.0.0.1:70000", "--insecure-no-pin"],
            vec![
                "--host",
                "--host-addr",
                "0.0.0.0:7777",
                "--host-addr",
                "0.0.0.0:7778",
            ],
            vec!["--connect", "127.0.0.1:7777", "--server-fingerprint", "zz"],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn password_file_is_allowed_with_host_or_connect_only() {
        assert_eq!(
            parse(&["--host", "--password-file", "pass.txt"])
                .unwrap()
                .network
                .password_file,
            Some(PathBuf::from("pass.txt"))
        );
        assert!(
            parse(&[
                "--connect",
                "127.0.0.1:7777",
                "--insecure-no-pin",
                "--password-file",
                "pass.txt",
            ])
            .is_ok()
        );
    }

    #[test]
    fn server_key_is_a_host_option() {
        let key = parse(&["--host", "--server-key", "server.key"])
            .unwrap()
            .network;
        assert_eq!(key.server_key, Some(PathBuf::from("server.key")));
    }

    #[test]
    fn server_parses_bind_and_always_hosts() {
        let server = parse_server(&[
            "--project",
            "game",
            "--bind",
            "0.0.0.0:7000",
            "--server-key",
            "server.key",
            "--password-file",
            "pass.txt",
            "--frames",
            "300",
        ])
        .unwrap();
        assert_eq!(server.project, Some(PathBuf::from("game")));
        assert_eq!(server.frames, Some(300));
        assert!(server.network.host);
        assert_eq!(server.network.host_addr, Some(address("0.0.0.0:7000")));
        assert_eq!(server.network.server_key, Some(PathBuf::from("server.key")));

        assert!(parse_server(&[]).unwrap().network.host);
    }

    #[test]
    fn server_rejects_player_only_flags() {
        for args in [
            vec!["--debug-view", "lit", "--project", "game"],
            vec!["--host"],
            vec!["--connect", "127.0.0.1:7777", "--insecure-no-pin"],
            vec!["--insecure-no-pin"],
            vec!["--host-addr", "0.0.0.0:7777"],
            vec!["--bind", "0.0.0.0:7777", "--bind", "0.0.0.0:7778"],
            vec!["--bind", "bad"],
            vec!["--unknown"],
        ] {
            assert!(parse_server(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn fingerprint_requires_64_hex_digits_in_either_case() {
        assert!(parse_fingerprint(FINGERPRINT).is_ok());
        assert_eq!(
            parse_fingerprint(&FINGERPRINT.to_uppercase()).unwrap(),
            parse_fingerprint(FINGERPRINT).unwrap()
        );
        assert!(parse_fingerprint(&FINGERPRINT[1..]).is_err());
        assert!(parse_fingerprint(&format!("{}g", &FINGERPRINT[1..])).is_err());
        assert!(parse_fingerprint("").is_err());
    }
}
